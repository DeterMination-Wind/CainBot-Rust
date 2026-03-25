import fs from 'node:fs/promises';
import path from 'node:path';
import { spawn } from 'node:child_process';

import { pathExists } from './utils.mjs';

const SKIP_DIRS = new Set([
  '.git',
  'node_modules',
  'dist',
  'build',
  'out',
  'bin',
  'obj',
  '.gradle',
  '.idea',
  '.next',
  '.cache',
  'coverage',
  'vendor',
  'Pods'
]);

const TEXT_FILE_EXTENSIONS = new Set([
  '.txt', '.md', '.markdown', '.json', '.jsonc', '.yaml', '.yml', '.toml', '.ini', '.cfg', '.conf',
  '.js', '.mjs', '.cjs', '.ts', '.tsx', '.jsx', '.java', '.kt', '.kts', '.gradle', '.properties',
  '.xml', '.html', '.css', '.scss', '.less', '.py', '.rb', '.php', '.go', '.rs', '.cpp', '.c', '.h',
  '.hpp', '.cs', '.sh', '.ps1', '.bat', '.cmd', '.sql', '.csv', '.env', '.gitignore', '.gitattributes',
  '.vue', '.svelte', '.lua'
]);

function clampInteger(value, fallback, min, max) {
  const numeric = Number(value);
  if (!Number.isFinite(numeric)) {
    return fallback;
  }
  return Math.min(max, Math.max(min, Math.trunc(numeric)));
}

function normalizeText(value) {
  return String(value ?? '').replace(/\s+/g, ' ').trim();
}

function escapeRegex(value) {
  return String(value ?? '').replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
}

function relativeDisplayPath(rootPath, targetPath) {
  const relativePath = path.relative(rootPath, targetPath);
  return relativePath || '.';
}

function splitQueryTokens(query) {
  const source = normalizeText(query);
  if (!source) {
    return [];
  }
  const rawTokens = source.match(/[\p{Script=Han}]{2,}|[A-Za-z0-9_.:/\\-]{2,}/gu) ?? [];
  const seen = new Set();
  const tokens = [];
  for (const token of rawTokens) {
    const normalized = token.toLowerCase();
    if (!normalized || seen.has(normalized)) {
      continue;
    }
    seen.add(normalized);
    tokens.push(normalized);
  }
  return tokens.sort((left, right) => right.length - left.length).slice(0, 10);
}

function scorePathMatch(relativePath, query, tokens) {
  const normalizedPath = String(relativePath ?? '').toLowerCase();
  const normalizedQuery = normalizeText(query).toLowerCase();
  const baseName = path.basename(normalizedPath);

  let score = 0;
  if (normalizedQuery && normalizedPath === normalizedQuery) {
    score += 320;
  }
  if (normalizedQuery && baseName === normalizedQuery) {
    score += 260;
  }
  if (normalizedQuery && normalizedPath.includes(normalizedQuery)) {
    score += 90;
  }
  if (normalizedQuery && baseName.includes(normalizedQuery)) {
    score += 120;
  }
  for (const token of tokens) {
    if (baseName.includes(token)) {
      score += 40;
    }
    if (normalizedPath.includes(token)) {
      score += 16;
    }
  }
  if (tokens.length > 1 && tokens.every((token) => normalizedPath.includes(token))) {
    score += 60;
  }
  score -= normalizedPath.length / 200;
  return score;
}

function scoreSnippet(snippet, query, tokens) {
  const normalized = normalizeText(snippet).toLowerCase();
  if (!normalized) {
    return 0;
  }
  let score = 0;
  const normalizedQuery = normalizeText(query).toLowerCase();
  if (normalizedQuery && normalized.includes(normalizedQuery)) {
    score += 120;
  }
  for (const token of tokens) {
    if (normalized.includes(token)) {
      score += Math.max(10, Math.min(36, token.length * 4));
    }
  }
  return score;
}

async function safeReadHead(filePath, maxLines = 40, maxChars = 900) {
  const content = await fs.readFile(filePath, 'utf8');
  const lines = content
    .split(/\r?\n/)
    .map((line) => line.trimEnd())
    .filter((line) => line.trim().length > 0)
    .slice(0, maxLines);
  let joined = lines.join(' ');
  if (joined.length > maxChars) {
    joined = `${joined.slice(0, maxChars - 1)}…`;
  }
  return joined;
}

export class LocalRagRetriever {
  constructor(config = {}, logger) {
    this.logger = logger;
    this.enabled = config?.rag?.enabled !== false;
    this.autoInject = config?.rag?.autoInject !== false;
    this.timeoutMs = clampInteger(config?.rag?.timeoutMs, 2500, 500, 15000);
    this.maxResults = clampInteger(config?.rag?.maxResults, 6, 1, 12);
    this.maxPathResults = clampInteger(config?.rag?.maxPathResults, 4, 0, 8);
    this.maxContentResults = clampInteger(config?.rag?.maxContentResults, 6, 0, 12);
    this.maxFileSizeBytes = clampInteger(config?.rag?.maxFileSizeBytes, 1024 * 1024, 4096, 8 * 1024 * 1024);
    this.maxPromptChars = clampInteger(config?.rag?.maxPromptChars, 4200, 512, 12000);
    this.roots = Array.isArray(config?.rag?.roots) ? config.rag.roots.map((item) => ({
      alias: normalizeText(item?.alias || path.basename(String(item?.path ?? '')) || 'knowledge'),
      path: path.resolve(String(item?.path ?? ''))
    })).filter((item) => item.path) : [];
  }

  async isEnabled() {
    if (!this.enabled || !this.autoInject || this.roots.length === 0) {
      return false;
    }
    for (const root of this.roots) {
      if (await pathExists(root.path)) {
        return true;
      }
    }
    return false;
  }

  getPromptInstructions() {
    if (!this.enabled) {
      return '';
    }
    return '系统可能会在用户提问前自动附带一段【本地知识库检索结果】。如果这段结果存在，请优先基于这些本地片段回答；若仍不足，再按需继续使用只读工具读取更具体文件。';
  }

  async retrieve(query) {
    const normalizedQuery = normalizeText(query);
    if (!(await this.isEnabled()) || !this.#looksLikeKnowledgeQuery(normalizedQuery)) {
      return {
        query: normalizedQuery,
        results: [],
        prompt: ''
      };
    }

    const availableRoots = [];
    for (const root of this.roots) {
      if (await pathExists(root.path)) {
        availableRoots.push(root);
      }
    }
    if (availableRoots.length === 0) {
      return { query: normalizedQuery, results: [], prompt: '' };
    }

    const tokens = splitQueryTokens(normalizedQuery);
    if (tokens.length === 0) {
      return { query: normalizedQuery, results: [], prompt: '' };
    }

    try {
      const [pathMatches, contentMatches] = await Promise.all([
        this.#searchPaths(availableRoots, normalizedQuery, tokens),
        this.#searchContent(availableRoots, normalizedQuery, tokens)
      ]);
      const results = this.#mergeResults(pathMatches, contentMatches).slice(0, this.maxResults);
      return {
        query: normalizedQuery,
        results,
        prompt: this.#formatPrompt(normalizedQuery, results)
      };
    } catch (error) {
      this.logger?.warn?.(`本地 RAG 检索失败：${error.message}`);
      return {
        query: normalizedQuery,
        results: [],
        prompt: ''
      };
    }
  }

  #looksLikeKnowledgeQuery(query) {
    if (!query) {
      return false;
    }
    if (query.length >= 80) {
      return true;
    }
    if (/[\\/]/.test(query)) {
      return true;
    }
    if (/\b[a-z0-9_.-]+\.(json|jsonc|ya?ml|toml|ini|properties|java|kt|kts|js|mjs|cjs|ts|tsx|jsx|cs|py|cpp|c|h|md|txt)\b/i.test(query)) {
      return true;
    }
    if (/[#{}();=<>]|=>|::/.test(query)) {
      return true;
    }
    return /(源码|代码|项目|仓库|文件|目录|路径|配置|compose|docker|json|yaml|yml|toml|接口|函数|方法|类|脚本|报错|错误|异常|日志|堆栈|mod|mindustry|地图|prompt|模型|技能|skill)/i.test(query);
  }

  async #searchPaths(roots, query, tokens) {
    const candidates = [];
    for (const root of roots) {
      await this.#walkPathCandidates(root, root.path, query, tokens, candidates);
    }
    const sorted = candidates
      .sort((left, right) => right.score - left.score || left.path.localeCompare(right.path, 'zh-CN'))
      .slice(0, this.maxPathResults);

    const results = [];
    for (const item of sorted) {
      let preview = '';
      try {
        const extension = path.extname(item.absolutePath).toLowerCase();
        if (TEXT_FILE_EXTENSIONS.has(extension)) {
          preview = await safeReadHead(item.absolutePath, 28, 700);
        }
      } catch {}
      results.push({
        source: 'path',
        alias: item.alias,
        path: item.path,
        line: null,
        score: item.score,
        snippet: preview || `命中文件路径：${item.path}`
      });
    }
    return results;
  }

  async #walkPathCandidates(root, currentDir, query, tokens, results) {
    const dirents = await fs.readdir(currentDir, { withFileTypes: true }).catch(() => []);
    for (const entry of dirents) {
      const fullPath = path.join(currentDir, entry.name);
      const relativePath = relativeDisplayPath(root.path, fullPath);
      if (entry.isDirectory()) {
        if (SKIP_DIRS.has(entry.name)) {
          continue;
        }
        const dirScore = scorePathMatch(relativePath, query, tokens);
        if (dirScore >= 80) {
          results.push({
            alias: root.alias,
            absolutePath: fullPath,
            path: relativePath,
            score: dirScore,
            type: 'directory'
          });
        }
        await this.#walkPathCandidates(root, fullPath, query, tokens, results);
        continue;
      }
      if (!entry.isFile()) {
        continue;
      }
      const pathScore = scorePathMatch(relativePath, query, tokens);
      if (pathScore >= 70) {
        results.push({
          alias: root.alias,
          absolutePath: fullPath,
          path: relativePath,
          score: pathScore,
          type: 'file'
        });
      }
    }
  }

  async #searchContent(roots, query, tokens) {
    if (this.maxContentResults <= 0) {
      return [];
    }
    const pattern = tokens.map((token) => escapeRegex(token)).join('|');
    if (!pattern) {
      return [];
    }

    const args = [
      '--json',
      '-n',
      '-S',
      '-m', '1',
      '--max-filesize', `${this.maxFileSizeBytes}`,
      '-g', '!**/.git/**',
      '-g', '!**/node_modules/**',
      '-g', '!**/dist/**',
      '-g', '!**/build/**',
      '-g', '!**/out/**',
      '-g', '!**/.next/**',
      pattern,
      ...roots.map((item) => item.path)
    ];

    const stdout = await this.#runRipgrep(args);
    if (!stdout.trim()) {
      return [];
    }

    const rootByPath = new Map(roots.map((item) => [item.path, item]));
    const results = [];
    for (const line of stdout.split(/\r?\n/)) {
      if (!line.trim()) {
        continue;
      }
      let parsed;
      try {
        parsed = JSON.parse(line);
      } catch {
        continue;
      }
      if (parsed?.type !== 'match') {
        continue;
      }
      const absolutePath = String(parsed?.data?.path?.text ?? '');
      if (!absolutePath) {
        continue;
      }
      const root = roots.find((item) => absolutePath === item.path || absolutePath.startsWith(`${item.path}${path.sep}`)) ?? rootByPath.get(path.dirname(absolutePath));
      if (!root) {
        continue;
      }
      const snippet = normalizeText(parsed?.data?.lines?.text ?? '');
      const score = scoreSnippet(snippet, query, tokens) + scorePathMatch(relativeDisplayPath(root.path, absolutePath), query, tokens);
      results.push({
        source: 'content',
        alias: root.alias,
        path: relativeDisplayPath(root.path, absolutePath),
        line: Number(parsed?.data?.line_number ?? 0) || null,
        score,
        snippet: snippet.length > 220 ? `${snippet.slice(0, 219)}…` : snippet
      });
    }

    return results
      .sort((left, right) => right.score - left.score || left.path.localeCompare(right.path, 'zh-CN'))
      .slice(0, this.maxContentResults);
  }

  async #runRipgrep(args) {
    return await new Promise((resolve, reject) => {
      const child = spawn('rg', args, {
        windowsHide: true,
        stdio: ['ignore', 'pipe', 'pipe']
      });
      let stdout = '';
      let stderr = '';
      let settled = false;
      const finish = (handler) => (value) => {
        if (settled) {
          return;
        }
        settled = true;
        clearTimeout(timer);
        handler(value);
      };
      const resolveOnce = finish(resolve);
      const rejectOnce = finish(reject);
      const timer = setTimeout(() => {
        try {
          child.kill();
        } catch {}
        resolveOnce(stdout);
      }, this.timeoutMs);

      child.on('error', (error) => {
        if (error?.code === 'ENOENT') {
          resolveOnce('');
          return;
        }
        rejectOnce(error);
      });
      child.stdout.on('data', (chunk) => {
        stdout += chunk.toString();
      });
      child.stderr.on('data', (chunk) => {
        stderr += chunk.toString();
      });
      child.on('close', (code) => {
        if (code === 0 || code === 1) {
          resolveOnce(stdout);
          return;
        }
        rejectOnce(new Error(stderr.trim() || `rg exited with code ${code ?? 'unknown'}`));
      });
    });
  }

  #mergeResults(pathMatches, contentMatches) {
    const merged = [];
    const seen = new Set();
    for (const item of [...contentMatches, ...pathMatches]) {
      const key = `${item.source}|${item.alias}|${item.path}|${item.line ?? 0}`;
      if (seen.has(key)) {
        continue;
      }
      seen.add(key);
      merged.push(item);
    }
    return merged.sort((left, right) => right.score - left.score || left.path.localeCompare(right.path, 'zh-CN'));
  }

  #formatPrompt(query, results) {
    if (!Array.isArray(results) || results.length === 0) {
      return '';
    }
    const lines = [
      `【本地知识库检索结果】（针对问题：${query}）`,
      '以下内容来自本地代码、配置或文档目录，只读参考，可能不完整：'
    ];
    let totalChars = lines.join('\n').length;
    for (let index = 0; index < results.length; index += 1) {
      const item = results[index];
      const header = `${index + 1}. [${item.alias}] ${item.path}${item.line ? `:${item.line}` : ''}`;
      const body = item.snippet || '(无摘要)';
      const block = `${header}\n${body}`;
      if (totalChars + block.length > this.maxPromptChars) {
        break;
      }
      lines.push(block);
      totalChars += block.length;
    }
    lines.push('如果这些信息已经足够，请直接基于它们回答；如果还不够，再继续使用只读工具读取更具体文件。');
    return lines.join('\n\n');
  }
}
