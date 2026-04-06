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

const markdown = markdownIt({
  html: false,
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
  idleTimer: null
};

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

function sanitizeReplyText(sourceText) {
  const withoutToolBlocks = String(sourceText ?? '').replace(TOOL_BLOCK_REGEX, '').trim();
  const cleaned = unwrapNestedMarkdownFences(withoutToolBlocks).trim();
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

function scheduleBrowserIdleClose() {
  if (browserHolder.idleTimer) {
    clearTimeout(browserHolder.idleTimer);
    browserHolder.idleTimer = null;
  }
  browserHolder.idleTimer = setTimeout(async () => {
    const browser = browserHolder.browser;
    browserHolder.browser = null;
    browserHolder.idleTimer = null;
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
  browser.on('disconnected', () => {
    if (browserHolder.browser === browser) {
      browserHolder.browser = null;
    }
  });
  return browser;
}

async function waitForImageLoad(page, timeoutMs = EXTERNAL_IMAGE_WAIT_MS) {
  await page.evaluate(async (timeout) => {
    const images = Array.from(document.images || []);
    if (!images.length) {
      return;
    }
    const waitImage = (img) => {
      if (img.complete) {
        return Promise.resolve();
      }
      return new Promise((resolve) => {
        const done = () => resolve();
        img.addEventListener('load', done, { once: true });
        img.addEventListener('error', done, { once: true });
      });
    };
    await Promise.race([
      Promise.all(images.map(waitImage)),
      new Promise((resolve) => setTimeout(resolve, Math.max(100, Number(timeout) || 100)))
    ]);
  }, timeoutMs);
}

export async function renderReplyMarkdownImage(replyText) {
  const normalized = sanitizeReplyText(replyText);
  if (!normalized) {
    return '';
  }
  await cleanupOldImages();
  const styleBundle = await getStyleBundle();
  const html = buildHtmlDocument(normalized, styleBundle);
  const hash = crypto.createHash('sha1').update(normalized).digest('hex').slice(0, 10);
  const outputPath = path.join(RENDER_DIR, `reply-${Date.now()}-${hash}.png`);

  const browser = await getSharedBrowser();
  let page = null;
  try {
    scheduleBrowserIdleClose();
    page = await browser.newPage({
      viewport: { width: 1240, height: 1200 },
      deviceScaleFactor: 1.5
    });
    await page.route('**/*', async (route) => {
      const request = route.request();
      const url = request.url();
      const type = request.resourceType();
      if (url.startsWith('file:') || url.startsWith('data:') || url === 'about:blank') {
        await route.continue().catch(() => {});
        return;
      }
      if ((url.startsWith('http://') || url.startsWith('https://')) && type === 'image') {
        await route.continue().catch(() => {});
        return;
      }
      await route.abort().catch(() => {});
    });
    await page.setContent(html, { waitUntil: 'domcontentloaded' });
    await waitForImageLoad(page, EXTERNAL_IMAGE_WAIT_MS);
    await page.evaluate(async () => {
      if (document?.fonts?.ready) {
        await document.fonts.ready;
      }
    });
    await page.waitForTimeout(20);
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
    return outputPath;
  } catch (error) {
    if (browserHolder.browser === browser) {
      browserHolder.browser = null;
    }
    await browser.close().catch(() => {});
    throw error;
  } finally {
    if (page) {
      await page.close().catch(() => {});
    }
    scheduleBrowserIdleClose();
  }
}
