import crypto from 'node:crypto';
import fs from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { createRequire } from 'node:module';

import hljs from 'highlight.js';
import markdownIt from 'markdown-it';
import markdownItKatex from 'markdown-it-katex';
import { chromium } from 'playwright';

const require = createRequire(import.meta.url);

const TOOL_BLOCK_REGEX = /<<<CAIN_CODEX_TOOL_START>>>[\s\S]*?<<<CAIN_CODEX_TOOL_END>>>/g;
const RENDER_DIR = path.join(os.tmpdir(), 'cain-reply-markdown-images');
const MAX_RENDER_CHARS = 20_000;
const MAX_RENDER_HEIGHT = 14_000;
const KEEP_RENDER_FILES = 120;
const MAX_RENDER_AGE_MS = 4 * 60 * 60 * 1000;
const BROWSER_IDLE_CLOSE_MS = 90_000;
const EXTERNAL_IMAGE_WAIT_MS = 2_500;
const EXTERNAL_IMAGE_TIMEOUT_LABEL = '图片加载超时';
const EXTERNAL_IMAGE_ERROR_LABEL = '图片加载失败';
const DANGEROUS_HTML_TAGS = [
  'script',
  'style',
  'iframe',
  'frame',
  'frameset',
  'object',
  'embed',
  'meta',
  'link',
  'base',
  'form',
  'input',
  'button',
  'textarea',
  'select',
  'option',
  'svg',
  'math'
];

const markdown = markdownIt({
  html: true,
  linkify: true,
  typographer: true,
  breaks: true,
  highlight(code, language) {
    const source = String(code ?? '');
    const normalizedLanguage = String(language ?? '').trim();
    if (normalizedLanguage && hljs.getLanguage(normalizedLanguage)) {
      try {
        return `<pre class="hljs"><code>${hljs.highlight(source, { language: normalizedLanguage, ignoreIllegals: true }).value}</code></pre>`;
      } catch {
        // noop
      }
    }
    return `<pre class="hljs"><code>${hljs.highlightAuto(source).value}</code></pre>`;
  }
});
markdown.use(markdownItKatex);

let styleCache = null;
const fontDataUrlCache = new Map();
let browserHolder = {
  browser: null,
  page: null,
  idleTimer: null,
  activeRenderCount: 0
};
let renderQueue = Promise.resolve();

function getFontMimeType(fileName = '') {
  const ext = path.extname(String(fileName)).toLowerCase();
  if (ext === '.woff2') {
    return 'font/woff2';
  }
  if (ext === '.woff') {
    return 'font/woff';
  }
  if (ext === '.eot') {
    return 'application/vnd.ms-fontobject';
  }
  return 'font/ttf';
}

async function readFontDataUrl(absolutePath, fileName) {
  if (fontDataUrlCache.has(absolutePath)) {
    return fontDataUrlCache.get(absolutePath);
  }
  const raw = await fs.readFile(absolutePath);
  const mime = getFontMimeType(fileName);
  const dataUrl = `data:${mime};base64,${raw.toString('base64')}`;
  fontDataUrlCache.set(absolutePath, dataUrl);
  return dataUrl;
}

async function getStyleBundle() {
  if (styleCache) {
    return styleCache;
  }
  const markdownKatexDir = path.dirname(require.resolve('markdown-it-katex/package.json'));
  const bundledKatexCssPath = path.join(markdownKatexDir, 'node_modules', 'katex', 'dist', 'katex.min.css');
  const katexCssPath = await pathExists(bundledKatexCssPath)
    ? bundledKatexCssPath
    : require.resolve('katex/dist/katex.min.css');
  const katexFontsDir = path.join(path.dirname(katexCssPath), 'fonts');
  const rawKatexCss = await fs.readFile(katexCssPath, 'utf8');
  const fontRefs = Array.from(rawKatexCss.matchAll(/url\((['"]?)fonts\/([^'")]+)\1\)/g))
    .map((match) => String(match[2] ?? '').trim())
    .filter(Boolean);
  const embeddedMap = new Map();
  for (const ref of [...new Set(fontRefs)]) {
    const cleanName = ref.replace(/[?#].*$/, '');
    const absolutePath = path.join(katexFontsDir, cleanName);
    const dataUrl = await readFontDataUrl(absolutePath, cleanName);
    embeddedMap.set(ref, dataUrl);
  }
  const katexCss = rawKatexCss.replace(/url\((['"]?)fonts\/([^'")]+)\1\)/g, (_, _quote, fileName) => {
    const embedded = embeddedMap.get(String(fileName));
    return embedded ? `url("${embedded}")` : 'url("")';
  });
  const hljsCssPath = require.resolve('highlight.js/styles/vs2015.css');
  const hljsCss = await fs.readFile(hljsCssPath, 'utf8');
  styleCache = `${katexCss}\n${hljsCss}`;
  return styleCache;
}

function unwrapNestedMarkdownFences(text) {
  const source = String(text ?? '').replace(/\r\n/g, '\n');
  const lines = source.split('\n');
  const output = [];
  let index = 0;

  const parseFence = (line) => {
    const matched = String(line ?? '').match(/^[ \t]{0,3}(`{3,})([^\r\n]*)$/);
    if (!matched) {
      return null;
    }
    const fence = String(matched[1] ?? '');
    const tail = String(matched[2] ?? '');
    return {
      length: fence.length,
      info: tail.trim(),
      closing: tail.trim() === ''
    };
  };

  while (index < lines.length) {
    const openFence = parseFence(lines[index]);
    if (!openFence || !/^(md|markdown)$/i.test(openFence.info)) {
      output.push(lines[index]);
      index += 1;
      continue;
    }

    let cursor = index + 1;
    let nestedFenceDepth = 0;
    const bodyLines = [];
    let closed = false;
    while (cursor < lines.length) {
      const fence = parseFence(lines[cursor]);
      if (!fence || fence.length < openFence.length) {
        bodyLines.push(lines[cursor]);
        cursor += 1;
        continue;
      }
      if (nestedFenceDepth === 0 && fence.closing) {
        closed = true;
        cursor += 1;
        break;
      }
      if (fence.closing) {
        nestedFenceDepth = Math.max(0, nestedFenceDepth - 1);
      } else {
        nestedFenceDepth += 1;
      }
      bodyLines.push(lines[cursor]);
      cursor += 1;
    }

    if (!closed) {
      output.push(lines[index], ...bodyLines);
      index = cursor;
      continue;
    }

    let start = 0;
    let end = bodyLines.length;
    while (start < end && String(bodyLines[start] ?? '').trim() === '') {
      start += 1;
    }
    while (end > start && String(bodyLines[end - 1] ?? '').trim() === '') {
      end -= 1;
    }
    output.push(...bodyLines.slice(start, end));
    index = cursor;
  }

  return output.join('\n');
}

function applyTransformOutsideFencedBlocks(text, transform) {
  const source = String(text ?? '').replace(/\r\n/g, '\n');
  const lines = source.split('\n');
  const output = [];
  let plainBuffer = [];
  let inFence = false;
  let fenceMarker = '';
  let fenceLength = 0;

  const parseFence = (line) => {
    const matched = String(line ?? '').match(/^[ \t]{0,3}([`~]{3,})([^\r\n]*)$/);
    if (!matched) {
      return null;
    }
    return {
      marker: matched[1][0],
      length: matched[1].length,
      tail: String(matched[2] ?? '').trim()
    };
  };

  const flushPlain = () => {
    if (!plainBuffer.length) {
      return;
    }
    output.push(transform(plainBuffer.join('\n')));
    plainBuffer = [];
  };

  for (const line of lines) {
    const fence = parseFence(line);
    if (!inFence) {
      if (fence) {
        flushPlain();
        inFence = true;
        fenceMarker = fence.marker;
        fenceLength = fence.length;
        output.push(line);
      } else {
        plainBuffer.push(line);
      }
      continue;
    }

    if (fence && fence.marker === fenceMarker && fence.length >= fenceLength && fence.tail === '') {
      inFence = false;
      output.push(line);
    } else {
      output.push(line);
    }
  }
  flushPlain();
  return output.join('\n');
}

function normalizeLatexDelimiters(text) {
  return applyTransformOutsideFencedBlocks(text, (segment) => {
    let output = String(segment ?? '');
    output = output.replace(/\\tag\{([^{}]+)\}/g, (_match, label = '') => `\\qquad(${String(label).trim()})`);
    output = output.replace(/\\\[\s*([\s\S]*?)\s*\\\]/g, (_match, body = '') => `$$\n${String(body).trim()}\n$$`);
    output = output.replace(/\\\(([\s\S]*?)\\\)/g, (_match, body = '') => `$${String(body).trim()}$`);
    output = output.replace(/(^|\n)\[\s*\n([\s\S]*?)\n\s*\](?=\n|$)/g, (_match, prefix = '', body = '') => `${prefix}$$\n${String(body).trim()}\n$$`);
    output = output.replace(/\\begin\{([a-zA-Z*]+)\}[\s\S]*?\\end\{\1\}/g, (match = '', _envName = '', offset = 0, source = '') => {
      const before = String(source).slice(Math.max(0, Number(offset) - 6), Number(offset));
      const after = String(source).slice(Number(offset) + String(match).length, Number(offset) + String(match).length + 6);
      if (before.includes('$$') || after.includes('$$')) {
        return String(match);
      }
      return `$$\n${String(match).trim()}\n$$`;
    });
    return output;
  });
}

function stripDangerousHtml(text) {
  let output = String(text ?? '');
  for (const tag of DANGEROUS_HTML_TAGS) {
    const pairedTag = new RegExp(`<${tag}\\b[\\s\\S]*?<\\/${tag}\\s*>`, 'gi');
    const singleTag = new RegExp(`<${tag}\\b[^>]*\\/?>`, 'gi');
    output = output.replace(pairedTag, '');
    output = output.replace(singleTag, '');
  }
  output = output.replace(/\son[a-z]+\s*=\s*("[^"]*"|'[^']*'|[^\s>]+)/gi, '');
  output = output.replace(/\s(href|src)\s*=\s*("javascript:[^"]*"|'javascript:[^']*'|javascript:[^\s>]+)/gi, ' $1="#"');
  return output;
}

function sanitizeReplyText(sourceText) {
  const withoutToolBlocks = String(sourceText ?? '').replace(TOOL_BLOCK_REGEX, '').trim();
  const unwrapped = unwrapNestedMarkdownFences(withoutToolBlocks);
  const normalizedLatex = normalizeLatexDelimiters(unwrapped);
  const cleaned = stripDangerousHtml(normalizedLatex).trim();
  if (!cleaned) {
    return '';
  }
  if (cleaned.length <= MAX_RENDER_CHARS) {
    return cleaned;
  }
  return `${cleaned.slice(0, MAX_RENDER_CHARS)}\n\n…(内容过长，后续已省略)`;
}

function buildHtmlDocument(markdownText, styleBundle) {
  const renderedBody = markdown.render(markdownText);
  const customCss = `
    :root {
      color-scheme: dark;
      --vscode-bg: #1e1e1e;
      --vscode-card: #252526;
      --vscode-border: #3c3c3c;
      --vscode-text: #d4d4d4;
      --vscode-muted: #9da3b2;
      --vscode-link: #4fc1ff;
      --vscode-inline-code-bg: #2d2d2d;
      --vscode-inline-code-text: #ce9178;
      --vscode-quote: #608b4e;
      --vscode-table-head: #2a2d2e;
      --vscode-selection: #264f78;
    }
    * {
      box-sizing: border-box;
    }
    body {
      margin: 0;
      padding: 36px;
      background: radial-gradient(1200px 600px at 20% -10%, #2a2d2e 0%, #1e1e1e 55%, #171717 100%);
      color: var(--vscode-text);
      font-family: "Segoe UI", "Microsoft YaHei", "Noto Sans CJK SC", sans-serif;
      font-size: 17px;
      line-height: 1.7;
    }
    #card {
      width: 1080px;
      margin: 0 auto;
      background: linear-gradient(180deg, #252526 0%, #1f1f20 100%);
      border: 1px solid var(--vscode-border);
      border-radius: 16px;
      box-shadow: 0 24px 60px rgba(0, 0, 0, 0.45);
      padding: 30px 36px;
      overflow-wrap: break-word;
      word-break: normal;
    }
    #card > *:first-child {
      margin-top: 0 !important;
    }
    #card > *:last-child {
      margin-bottom: 0 !important;
    }
    a {
      color: var(--vscode-link);
    }
    p, ul, ol, blockquote, table, pre {
      margin: 0 0 14px 0;
    }
    ul, ol {
      padding-left: 26px;
    }
    h1, h2, h3, h4, h5, h6 {
      margin: 20px 0 12px 0;
      line-height: 1.35;
      font-weight: 600;
    }
    h1 {
      font-size: 34px;
      border-bottom: 1px solid #3f3f46;
      padding-bottom: 12px;
    }
    h2 {
      font-size: 28px;
    }
    h3 {
      font-size: 24px;
    }
    h4 {
      font-size: 21px;
    }
    code {
      font-family: "Cascadia Code", "Consolas", "JetBrains Mono", monospace;
      font-size: 0.9em;
      background: var(--vscode-inline-code-bg);
      color: var(--vscode-inline-code-text);
      padding: 0.2em 0.45em;
      border-radius: 6px;
      border: 1px solid #3a3a3a;
    }
    pre {
      background: #1a1a1a;
      border: 1px solid #333;
      border-radius: 12px;
      padding: 16px 18px;
      overflow-x: auto;
    }
    pre code {
      border: 0;
      background: transparent;
      color: inherit;
      padding: 0;
      font-size: 15px;
      line-height: 1.55;
    }
    blockquote {
      border-left: 4px solid #3794ff;
      background: #1f2a3a;
      color: #c5d8f3;
      margin-left: 0;
      padding: 10px 14px;
      border-radius: 0 10px 10px 0;
    }
    hr {
      border: 0;
      border-top: 1px solid #3c3c3c;
      margin: 18px 0;
    }
    table {
      border-collapse: collapse;
      width: 100%;
      border: 1px solid #3c3c3c;
      border-radius: 10px;
      overflow: hidden;
      font-size: 15px;
    }
    table thead tr {
      background: var(--vscode-table-head);
    }
    th, td {
      border: 1px solid #3c3c3c;
      padding: 8px 10px;
      text-align: left;
      vertical-align: top;
    }
    img {
      max-width: 100%;
      height: auto;
      display: block;
      margin: 12px auto;
      border: 1px solid #3a3a3a;
      border-radius: 10px;
      box-shadow: 0 8px 24px rgba(0, 0, 0, 0.35);
    }
    .img-fallback {
      margin: 12px auto;
      padding: 12px 14px;
      width: fit-content;
      max-width: 100%;
      color: #d7dbe5;
      font-size: 14px;
      line-height: 1.5;
      border: 1px dashed #4a4f5e;
      border-radius: 10px;
      background: rgba(38, 42, 52, 0.66);
      word-break: break-all;
    }
    .katex-display {
      margin: 14px 0;
      overflow-x: auto;
      overflow-y: visible;
      padding: 6px 0;
    }
    .katex-display > .katex {
      display: inline-block;
      min-width: max-content;
      padding: 2px 0 6px;
    }
    .katex, .katex * {
      box-sizing: content-box;
    }
    .meta {
      margin-top: 18px;
      padding-top: 12px;
      border-top: 1px solid #333;
      color: var(--vscode-muted);
      font-size: 13px;
      letter-spacing: 0.01em;
      text-transform: none;
      word-break: break-all;
    }
    details {
      margin: 14px 0;
      padding: 10px 12px;
      border: 1px solid #3a3f4a;
      border-radius: 10px;
      background: rgba(33, 38, 48, 0.5);
    }
    summary {
      cursor: default;
      font-weight: 600;
      color: #c8d8ff;
      margin-bottom: 8px;
    }
  `;

  return `<!doctype html>
<html lang="zh-CN">
<head>
  <meta charset="utf-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1" />
  <style>${styleBundle}</style>
  <style>${customCss}</style>
</head>
<body>
  <article id="card">
    ${renderedBody}
    <div class="meta">CainBot By DeterMination : https://github.com/DeterMination-Wind/CainBot-Rust</div>
  </article>
</body>
</html>`;
}

async function cleanupOldImages() {
  await fs.mkdir(RENDER_DIR, { recursive: true });
  const entries = await fs.readdir(RENDER_DIR, { withFileTypes: true });
  const files = [];
  for (const entry of entries) {
    if (!entry?.isFile?.() || !entry.name.startsWith('reply-') || !entry.name.endsWith('.png')) {
      continue;
    }
    const filePath = path.join(RENDER_DIR, entry.name);
    try {
      const stat = await fs.stat(filePath);
      files.push({
        path: filePath,
        mtimeMs: Number(stat.mtimeMs ?? 0) || 0
      });
    } catch {
      // ignore
    }
  }
  files.sort((a, b) => b.mtimeMs - a.mtimeMs);
  const now = Date.now();
  for (let index = 0; index < files.length; index += 1) {
    const item = files[index];
    const expired = now - item.mtimeMs > MAX_RENDER_AGE_MS;
    const overflow = index >= KEEP_RENDER_FILES;
    if (!expired && !overflow) {
      continue;
    }
    await fs.rm(item.path, { force: true }).catch(() => {});
  }
}

async function pathExists(filePath) {
  if (!filePath) {
    return false;
  }
  try {
    await fs.access(filePath);
    return true;
  } catch {
    return false;
  }
}

async function findWindowsPlaywrightChromium() {
  const localAppData = String(process.env.LOCALAPPDATA ?? '').trim();
  if (!localAppData) {
    return '';
  }
  const root = path.join(localAppData, 'ms-playwright');
  let entries = [];
  try {
    entries = await fs.readdir(root, { withFileTypes: true });
  } catch {
    return '';
  }
  const chromiumDirs = entries
    .filter((entry) => entry?.isDirectory?.() && entry.name.startsWith('chromium-'))
    .map((entry) => ({
      name: entry.name,
      revision: Number.parseInt(entry.name.slice('chromium-'.length), 10) || 0
    }))
    .sort((a, b) => b.revision - a.revision);
  for (const item of chromiumDirs) {
    const base = path.join(root, item.name);
    const candidates = [
      path.join(base, 'chrome-win64', 'chrome.exe'),
      path.join(base, 'chrome-win', 'chrome.exe')
    ];
    for (const candidate of candidates) {
      if (await pathExists(candidate)) {
        return candidate;
      }
    }
  }
  return '';
}

async function findFallbackChromiumExecutable() {
  const envCandidates = [
    process.env.CAIN_REPLY_RENDER_CHROMIUM_PATH,
    process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE_PATH,
    process.env.CHROMIUM_PATH
  ]
    .map((item) => String(item ?? '').trim())
    .filter(Boolean);
  for (const candidate of envCandidates) {
    if (await pathExists(candidate)) {
      return candidate;
    }
  }
  if (process.platform === 'win32') {
    const candidate = await findWindowsPlaywrightChromium();
    if (candidate) {
      return candidate;
    }
    return '';
  }
  const unixCandidates = [
    '/snap/bin/chromium',
    '/usr/bin/chromium',
    '/usr/bin/chromium-browser',
    '/usr/bin/google-chrome'
  ];
  for (const candidate of unixCandidates) {
    if (await pathExists(candidate)) {
      return candidate;
    }
  }
  return '';
}

async function launchBrowser() {
  const launchOptions = {
    headless: true,
    args: ['--no-sandbox', '--disable-setuid-sandbox']
  };
  try {
    return await chromium.launch(launchOptions);
  } catch (firstError) {
    const fallbackExecutable = await findFallbackChromiumExecutable();
    if (!fallbackExecutable) {
      throw firstError;
    }
    return await chromium.launch({
      ...launchOptions,
      executablePath: fallbackExecutable
    });
  }
}

function clearBrowserIdleCloseTimer() {
  if (browserHolder.idleTimer) {
    clearTimeout(browserHolder.idleTimer);
    browserHolder.idleTimer = null;
  }
}

function scheduleBrowserIdleClose() {
  clearBrowserIdleCloseTimer();
  if (browserHolder.activeRenderCount > 0) {
    return;
  }
  browserHolder.idleTimer = setTimeout(async () => {
    if (browserHolder.activeRenderCount > 0) {
      scheduleBrowserIdleClose();
      return;
    }
    const page = browserHolder.page;
    const browser = browserHolder.browser;
    browserHolder.page = null;
    browserHolder.browser = null;
    browserHolder.idleTimer = null;
    if (page) {
      await page.close().catch(() => {});
    }
    if (browser) {
      await browser.close().catch(() => {});
    }
  }, BROWSER_IDLE_CLOSE_MS);
  if (typeof browserHolder.idleTimer?.unref === 'function') {
    browserHolder.idleTimer.unref();
  }
}

async function getSharedBrowser() {
  if (browserHolder.browser) {
    return browserHolder.browser;
  }
  const browser = await launchBrowser();
  browserHolder.browser = browser;
  browserHolder.page = null;
  browser.on('disconnected', () => {
    if (browserHolder.browser === browser) {
      browserHolder.browser = null;
      browserHolder.page = null;
    }
  });
  return browser;
}

async function getSharedPage() {
  const browser = await getSharedBrowser();
  if (browserHolder.page && !browserHolder.page.isClosed()) {
    return browserHolder.page;
  }
  const page = await browser.newPage({
    viewport: { width: 1240, height: 1200 },
    deviceScaleFactor: 1.25
  });
  await page.route('**/*', async (route) => {
    const request = route.request();
    const url = request.url();
    const type = request.resourceType();
    if (url.startsWith('file:') || url.startsWith('data:') || url.startsWith('blob:') || url === 'about:blank') {
      await route.continue().catch(() => {});
      return;
    }
    if ((url.startsWith('http://') || url.startsWith('https://')) && type === 'image') {
      await route.continue().catch(() => {});
      return;
    }
    await route.abort().catch(() => {});
  });
  page.on('close', () => {
    if (browserHolder.page === page) {
      browserHolder.page = null;
    }
  });
  browserHolder.page = page;
  return page;
}

async function resetSharedBrowser() {
  clearBrowserIdleCloseTimer();
  const page = browserHolder.page;
  const browser = browserHolder.browser;
  browserHolder.page = null;
  browserHolder.browser = null;
  if (page) {
    await page.close().catch(() => {});
  }
  if (browser) {
    await browser.close().catch(() => {});
  }
}

function enqueueRender(task) {
  const current = renderQueue.then(task, task);
  renderQueue = current.catch(() => {});
  return current;
}

function isRecoverableBrowserClosedError(error) {
  const message = String(error?.message ?? error ?? '').toLowerCase();
  if (!message) {
    return false;
  }
  return (
    message.includes('target page, context or browser has been closed') ||
    message.includes('target closed') ||
    message.includes('page has been closed') ||
    message.includes('context has been closed') ||
    message.includes('browser has been closed') ||
    message.includes('browser has disconnected') ||
    message.includes('protocol error') && message.includes('closed')
  );
}

async function renderHtmlIntoImage(outputPath, html) {
  const page = await getSharedPage();
  await page.setContent(html, { waitUntil: 'domcontentloaded' });
  await page.evaluate(() => {
    document.querySelectorAll('details').forEach((node) => {
      if (!node.hasAttribute('open')) {
        node.setAttribute('open', '');
      }
    });
  });
  await waitForImageLoad(page, EXTERNAL_IMAGE_WAIT_MS);
  await page.evaluate(async () => {
    if (document?.fonts?.ready) {
      await document.fonts.ready;
    }
  });
  await page.waitForTimeout(15);
  const cardHeight = await page.evaluate(() => {
    const node = document.getElementById('card');
    if (!node) {
      return 480;
    }
    return Math.ceil(node.scrollHeight + 20);
  });
  const viewportHeight = Math.max(420, Math.min(MAX_RENDER_HEIGHT, Number(cardHeight) || 480));
  await page.setViewportSize({
    width: 1240,
    height: viewportHeight
  });
  await page.evaluate(async () => {
    if (document?.fonts?.ready) {
      await document.fonts.ready;
    }
  });
  const card = page.locator('#card');
  await card.screenshot({
    path: outputPath,
    type: 'png'
  });
}

async function waitForImageLoad(page, timeoutMs = EXTERNAL_IMAGE_WAIT_MS) {
  await page.evaluate(async ({ timeout, timeoutLabel, errorLabel }) => {
    const ensureFallback = (img, reason) => {
      if (!img || img.dataset.cainRenderFallback === '1') {
        return;
      }
      img.dataset.cainRenderFallback = '1';
      const hostText = (() => {
        try {
          const host = new URL(img.currentSrc || img.src || '').host;
          return host ? ` (${host})` : '';
        } catch {
          return '';
        }
      })();
      const fallback = document.createElement('div');
      fallback.className = 'img-fallback';
      fallback.textContent = `${reason}${hostText}`;
      img.replaceWith(fallback);
    };

    const images = Array.from(document.images || []);
    if (!images.length) {
      return;
    }
    const waitImage = (img) => {
      if (img.complete) {
        if ((img.naturalWidth || 0) <= 0 || (img.naturalHeight || 0) <= 0) {
          ensureFallback(img, errorLabel);
        }
        return Promise.resolve();
      }
      let settled = false;
      let resolveRef = null;
      const markSettled = () => {
        if (settled) {
          return;
        }
        settled = true;
        if (typeof resolveRef === 'function') {
          resolveRef();
        }
      };
      img.addEventListener('load', () => {
        if ((img.naturalWidth || 0) <= 0 || (img.naturalHeight || 0) <= 0) {
          ensureFallback(img, errorLabel);
        }
        markSettled();
      }, { once: true });
      img.addEventListener('error', () => {
        ensureFallback(img, errorLabel);
        markSettled();
      }, { once: true });
      return new Promise((resolve) => {
        resolveRef = resolve;
      });
    };
    const waiters = images.map(waitImage);
    await Promise.race([
      Promise.all(waiters),
      new Promise((resolve) => setTimeout(resolve, Math.max(120, Number(timeout) || 120)))
    ]);

    for (const img of images) {
      if (img.complete) {
        if ((img.naturalWidth || 0) <= 0 || (img.naturalHeight || 0) <= 0) {
          ensureFallback(img, errorLabel);
        }
        continue;
      }
      ensureFallback(img, timeoutLabel);
    }
  }, {
    timeout: timeoutMs,
    timeoutLabel: EXTERNAL_IMAGE_TIMEOUT_LABEL,
    errorLabel: EXTERNAL_IMAGE_ERROR_LABEL
  });
}

export async function renderReplyMarkdownImage(replyText) {
  return await enqueueRender(async () => {
    const normalized = sanitizeReplyText(replyText);
    if (!normalized) {
      return '';
    }
    await cleanupOldImages();
    const styleBundle = await getStyleBundle();
    const html = buildHtmlDocument(normalized, styleBundle);
    const hash = crypto.createHash('sha1').update(normalized).digest('hex').slice(0, 10);
    const outputPath = path.join(RENDER_DIR, `reply-${Date.now()}-${hash}.png`);

    browserHolder.activeRenderCount += 1;
    clearBrowserIdleCloseTimer();
    try {
      for (let attempt = 0; attempt < 2; attempt += 1) {
        try {
          await renderHtmlIntoImage(outputPath, html);
          return outputPath;
        } catch (error) {
          const recoverable = isRecoverableBrowserClosedError(error);
          await resetSharedBrowser();
          if (!recoverable || attempt >= 1) {
            throw error;
          }
        }
      }
      return '';
    } catch (error) {
      await resetSharedBrowser();
      throw error;
    } finally {
      browserHolder.activeRenderCount = Math.max(0, browserHolder.activeRenderCount - 1);
      scheduleBrowserIdleClose();
    }
  });
}
