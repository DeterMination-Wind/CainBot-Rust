import fs from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';

import { countMessageSegments, extractImageSegments, pathExists, plainTextFromMessage } from './utils.mjs';

const execFileAsync = promisify(execFile);

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

const TOOL_REQUEST_START = '<<<CAIN_CODEX_TOOL_START>>>';
const TOOL_REQUEST_END = '<<<CAIN_CODEX_TOOL_END>>>';
const SUPPORTED_TOOLS = new Set([
  'inspect_codex_project',
  'list_codex_directory',
  'search_codex_files',
  'read_codex_file',
  'subagent_codex_lookup',
  'read_bot_memory',
  'append_bot_memory',
  'send_prompt_image',
  'search_group_emotes',
  'send_group_emote',
  'read_recent_chat_messages',
  'read_group_chat_messages',
  'start_group_file_download',
  'run_bash_command',
  'run_python_script',
  'read_github_repo_releases',
  'read_github_repo_commits'
]);

function clampInteger(value, fallback, min, max) {
  const numeric = Number(value);
  if (!Number.isFinite(numeric)) {
    return fallback;
  }
  return Math.min(max, Math.max(min, Math.trunc(numeric)));
}

function toFiniteNumber(value, fallback = 0) {
  const numeric = Number(value);
  return Number.isFinite(numeric) ? numeric : fallback;
}

function formatEpochTime(seconds) {
  const numeric = toFiniteNumber(seconds, 0);
  if (numeric <= 0) {
    return '';
  }
  return new Date(numeric * 1000).toLocaleString('zh-CN', {
    hour12: false,
    timeZone: 'Asia/Shanghai'
  });
}

function messageOrderValue(message) {
  const realSeq = toFiniteNumber(message?.real_seq, 0);
  if (realSeq > 0) {
    return realSeq;
  }
  const time = toFiniteNumber(message?.time, 0);
  if (time > 0) {
    return time;
  }
  return toFiniteNumber(message?.message_seq, 0);
}

function buildMessageSummary(message) {
  const text = plainTextFromMessage(message?.message, message?.raw_message).replace(/\s+/g, ' ').trim();
  const imageCount = extractImageSegments(message?.message).length;
  const atCount = countMessageSegments(message?.message, 'at');
  const fileCount = countMessageSegments(message?.message, 'file');
  const faceCount = countMessageSegments(message?.message, 'face');
  const replyCount = countMessageSegments(message?.message, 'reply');
  const tags = [];
  if (imageCount > 0) {
    tags.push(`图片${imageCount}张`);
  }
  if (atCount > 0) {
    tags.push(`at${atCount}次`);
  }
  if (fileCount > 0) {
    tags.push(`文件${fileCount}个`);
  }
  if (faceCount > 0) {
    tags.push(`表情${faceCount}个`);
  }
  if (replyCount > 0) {
    tags.push(`引用${replyCount}次`);
  }

  const parts = [];
  if (text) {
    parts.push(text.slice(0, 320));
  }
  if (tags.length > 0) {
    parts.push(`[${tags.join('，')}]`);
  }
  return parts.join(' ').trim() || '(无可读文本，可能主要是图片、表情、文件或卡片)';
}

function stripCodeFence(text) {
  const source = String(text ?? '').trim();
  const fenced = source.match(/^```(?:json)?\s*([\s\S]+?)\s*```$/i);
  return fenced ? fenced[1].trim() : source;
}

function relativeDisplayPath(rootPath, targetPath) {
  const relativePath = path.relative(rootPath, targetPath);
  return relativePath || '.';
}

function buildSnippet(line, query) {
  const rawLine = String(line ?? '');
  const normalizedLine = rawLine.toLowerCase();
  const normalizedQuery = String(query ?? '').toLowerCase();
  const hitIndex = normalizedLine.indexOf(normalizedQuery);
  if (hitIndex < 0 || rawLine.length <= 180) {
    return rawLine.slice(0, 180);
  }

  const start = Math.max(0, hitIndex - 60);
  const end = Math.min(rawLine.length, hitIndex + normalizedQuery.length + 60);
  const prefix = start > 0 ? '...' : '';
  const suffix = end < rawLine.length ? '...' : '';
  return `${prefix}${rawLine.slice(start, end)}${suffix}`;
}

function splitQueryTokens(query) {
  return String(query ?? '')
    .toLowerCase()
    .split(/[^\p{L}\p{N}._-]+/u)
    .map((item) => item.trim())
    .filter(Boolean);
}

function extractLookupTerms(question, pathHint = '') {
  const raw = `${String(question ?? '')} ${String(pathHint ?? '')}`.replace(/[\\/]+/g, ' ').trim();
  if (!raw) {
    return [];
  }

  const synonymMap = new Map([
    ['放射性', ['radioactivity']],
    ['放电性', ['charge']],
    ['电性', ['charge']],
    ['爆炸性', ['explosiveness']],
    ['易燃性', ['flammability']],
    ['燃烧性', ['flammability']],
    ['硬度', ['hardness']],
    ['成本', ['cost']],
    ['建造血量', ['healthScaling']],
    ['血量缩放', ['healthScaling']],
    ['生产速度', ['craftTime', 'reload', 'drillTime']]
  ]);

  const stopwords = new Set([
    'mindustry',
    'mindustryx',
    'datapatch',
    'content',
    'compose',
    'mustread',
    'query',
    'file',
    'json',
    '字段',
    '类似',
    '所有',
    '一下',
    '这个',
    '那个',
    '继续',
    '查询',
    '读取',
    '内容',
    '文件',
    '目录',
    '问题',
    '影响',
    '修改',
    '什么',
    '会对',
    '所有类似'
  ]);

  const results = [];
  const seen = new Set();
  const pushToken = (value) => {
    const token = String(value ?? '').trim();
    if (!token) {
      return;
    }
    const normalized = token.toLowerCase();
    if (!normalized || seen.has(normalized) || stopwords.has(normalized)) {
      return;
    }
    if (/^\d+$/.test(normalized) && normalized.length < 2) {
      return;
    }
    seen.add(normalized);
    results.push(token);
  };

  for (const [key, aliases] of synonymMap.entries()) {
    if (!raw.includes(key)) {
      continue;
    }
    for (const alias of aliases) {
      pushToken(alias);
    }
  }

  const matches = raw.match(/[\p{Script=Han}]{2,}|[A-Za-z][A-Za-z0-9_.-]{1,}|\d+(?:\.\d+)?/gu) ?? [];
  for (const token of matches) {
    if (/^[\p{Script=Han}]+$/u.test(token) && token.length >= 4) {
      pushToken(token);
      for (let size = Math.min(4, token.length); size >= 2; size -= 1) {
        for (let index = 0; index + size <= token.length; index += 1) {
          pushToken(token.slice(index, index + size));
          if (results.length >= 28) {
            break;
          }
        }
        if (results.length >= 28) {
          break;
        }
      }
      continue;
    }
    pushToken(token);
    if (results.length >= 28) {
      break;
    }
  }

  return results.slice(0, 20);
}

function scoreLookupText(text, terms = []) {
  const searchable = String(text ?? '').toLowerCase();
  if (!searchable) {
    return {
      score: 0,
      matchedTerms: []
    };
  }

  let score = 0;
  const matchedTerms = [];
  for (const term of terms) {
    const normalized = String(term ?? '').trim().toLowerCase();
    if (!normalized) {
      continue;
    }
    let hitCount = 0;
    let searchFrom = 0;
    while (searchFrom < searchable.length) {
      const hitIndex = searchable.indexOf(normalized, searchFrom);
      if (hitIndex < 0) {
        break;
      }
      hitCount += 1;
      searchFrom = hitIndex + normalized.length;
      if (hitCount >= 6) {
        break;
      }
    }
    if (hitCount > 0) {
      matchedTerms.push(term);
      score += normalized.length >= 6 ? 18 : normalized.length >= 4 ? 12 : 8;
      score += Math.min(hitCount, 4) * (normalized.length >= 6 ? 5 : 3);
    }
  }

  return {
    score,
    matchedTerms
  };
}

function buildJsonLookupEntry(item, index, questionText, maxCharsPerResult) {
  const question = String(questionText ?? '').toLowerCase();
  const wantsFloat = question.includes('float') || question.includes('浮点');
  const fields = Array.isArray(item?.fields) ? item.fields : [];
  const typeName = String(item?.type ?? '').trim();
  const extendsName = String(item?.extends ?? '').trim();
  const typeNameLower = typeName.toLowerCase();
  const extendsNameLower = extendsName.toLowerCase();
  const itemTerms = extractLookupTerms(questionText, `${typeName} ${extendsName}`);
  let objectBoost = 0;
  for (const term of itemTerms) {
    const normalized = String(term ?? '').trim().toLowerCase();
    if (!normalized) {
      continue;
    }
    if (normalized === typeNameLower) {
      objectBoost += 180;
    } else if (typeNameLower.includes(normalized)) {
      objectBoost += 60;
    }
    if (normalized === extendsNameLower) {
      objectBoost += 110;
    } else if (extendsNameLower.includes(normalized)) {
      objectBoost += 36;
    }
  }

  const fieldMatches = fields
    .map((field, fieldIndex) => {
      const fieldScore = scoreLookupText(JSON.stringify(field), itemTerms).score;
      const typeName = String(field?.type ?? '').trim().toLowerCase();
      const name = String(field?.name ?? '').trim();
      const nameLower = name.toLowerCase();
      let score = fieldScore;
      if (wantsFloat && typeName === 'float') {
        score += 8;
      }
      if (name && typeName === 'float') {
        score += 4;
      }
      for (const term of itemTerms) {
        const normalized = String(term ?? '').trim().toLowerCase();
        if (!normalized) {
          continue;
        }
        if (normalized === nameLower) {
          score += 120;
        } else if (nameLower.includes(normalized)) {
          score += 30;
        }
        if (normalized === typeName) {
          score += 36;
        }
      }
      return {
        index: fieldIndex + 1,
        score,
        field
      };
    })
    .filter((entry) => entry.score > 0)
    .sort((left, right) => right.score - left.score || left.index - right.index)
    .slice(0, wantsFloat ? 24 : 12);

  const snippetValue = fieldMatches.length > 0
    ? {
      type: item?.type,
      extends: item?.extends,
      matched_fields: fieldMatches.map((entry) => ({
        index: entry.index,
        ...entry.field
      }))
    }
    : item;

  return {
    index,
    score: objectBoost
      + scoreLookupText(JSON.stringify(item), itemTerms).score
      + fieldMatches.slice(0, 8).reduce((total, entry) => total + entry.score, 0),
    matchedTerms: scoreLookupText(JSON.stringify(snippetValue), itemTerms).matchedTerms,
    snippet: trimText(JSON.stringify(snippetValue, null, 2), maxCharsPerResult),
    type: typeName,
    extends: extendsName
  };
}

function tryBuildStructuredLookupResults(content, questionText, maxResults, maxCharsPerResult) {
  if (String(content ?? '').length > 5 * 1024 * 1024) {
    return null;
  }

  let parsed;
  try {
    parsed = JSON.parse(content);
  } catch {
    return null;
  }

  if (!Array.isArray(parsed) || parsed.length === 0) {
    return null;
  }

  const results = parsed
    .map((item, index) => buildJsonLookupEntry(item, index + 1, questionText, maxCharsPerResult))
    .filter((entry) => entry.score > 0)
    .sort((left, right) => right.score - left.score || left.index - right.index)
    .slice(0, maxResults)
    .map((entry) => ({
      mode: 'json-entry',
      entry_index: entry.index,
      type: entry.type,
      extends: entry.extends,
      score: entry.score,
      matched_terms: entry.matchedTerms,
      snippet: entry.snippet
    }));

  return results.length > 0 ? results : null;
}

function buildTextChunks(content, chunkChars = 2400, overlapChars = 320) {
  const text = String(content ?? '');
  if (!text) {
    return [];
  }

  const normalizedChunkChars = Math.max(600, chunkChars);
  const normalizedOverlapChars = Math.max(80, Math.min(overlapChars, Math.floor(normalizedChunkChars / 2)));
  const step = Math.max(200, normalizedChunkChars - normalizedOverlapChars);
  const chunks = [];

  for (let start = 0; start < text.length; start += step) {
    const end = Math.min(text.length, start + normalizedChunkChars);
    const snippet = text.slice(start, end);
    if (!snippet.trim()) {
      continue;
    }
    chunks.push({
      startChar: start + 1,
      endChar: end,
      content: snippet
    });
    if (end >= text.length) {
      break;
    }
  }

  return chunks;
}

function countLinesBeforeOffset(content, charIndex) {
  const safeOffset = Math.max(0, Math.min(String(content ?? '').length, charIndex));
  let count = 1;
  for (let index = 0; index < safeOffset; index += 1) {
    if (content[index] === '\n') {
      count += 1;
    }
  }
  return count;
}

function scorePathMatch(relativePath, query) {
  const normalizedPath = String(relativePath ?? '').toLowerCase();
  const normalizedQuery = String(query ?? '').toLowerCase();
  const tokens = splitQueryTokens(query);
  const baseName = path.basename(normalizedPath);

  let score = 0;
  if (baseName === normalizedQuery) {
    score += 200;
  }
  if (normalizedPath === normalizedQuery) {
    score += 300;
  }
  if (baseName.includes(normalizedQuery)) {
    score += 80;
  }
  if (normalizedPath.includes(normalizedQuery)) {
    score += 60;
  }
  for (const token of tokens) {
    if (baseName.includes(token)) {
      score += 25;
    }
    if (normalizedPath.includes(token)) {
      score += 12;
    }
  }
  if (tokens.length > 1 && tokens.every((token) => normalizedPath.includes(token))) {
    score += 40;
  }
  score -= normalizedPath.length / 200;
  return score;
}

function extractBalancedJsonObjects(text) {
  const source = stripCodeFence(text);
  const results = [];
  let depth = 0;
  let startIndex = -1;
  let inString = false;
  let escaped = false;

  for (let index = 0; index < source.length; index += 1) {
    const char = source[index];

    if (inString) {
      if (escaped) {
        escaped = false;
      } else if (char === '\\') {
        escaped = true;
      } else if (char === '"') {
        inString = false;
      }
      continue;
    }

    if (char === '"') {
      inString = true;
      continue;
    }

    if (char === '{') {
      if (depth === 0) {
        startIndex = index;
      }
      depth += 1;
      continue;
    }

    if (char === '}') {
      if (depth <= 0) {
        continue;
      }
      depth -= 1;
      if (depth === 0 && startIndex >= 0) {
        results.push(source.slice(startIndex, index + 1));
        startIndex = -1;
      }
    }
  }

  return results;
}

function parseGithubRepoSpecifier(input) {
  const source = String(input ?? '').trim();
  if (!source) {
    throw new Error('repo 不能为空，应为 owner/repo 或 GitHub 仓库链接');
  }

  const normalized = source
    .replace(/^git\+/i, '')
    .replace(/\.git$/i, '')
    .trim();

  let owner = '';
  let repo = '';

  const directMatch = normalized.match(/^([A-Za-z0-9_.-]+)\/([A-Za-z0-9_.-]+)$/);
  if (directMatch) {
    owner = directMatch[1];
    repo = directMatch[2];
  } else {
    try {
      const parsed = new URL(/^https?:\/\//i.test(normalized) ? normalized : `https://${normalized}`);
      const parts = parsed.pathname.split('/').map((item) => item.trim()).filter(Boolean);
      if ((parsed.hostname === 'github.com' || parsed.hostname === 'www.github.com') && parts.length >= 2) {
        [owner, repo] = parts;
      }
    } catch {
    }
  }

  owner = String(owner ?? '').trim();
  repo = String(repo ?? '').trim().replace(/\.git$/i, '');
  if (!owner || !repo) {
    throw new Error(`无法解析 GitHub 仓库：${source}`);
  }

  return {
    owner,
    repo,
    fullName: `${owner}/${repo}`,
    htmlUrl: `https://github.com/${owner}/${repo}`
  };
}

function normalizeGithubApiBaseUrl(baseUrl) {
  const normalized = String(baseUrl ?? '').trim() || 'https://api.github.com';
  return normalized.replace(/\/+$/g, '');
}

function trimText(value, maxChars, suffix = '\n...(已截断)') {
  const text = String(value ?? '').trim();
  if (!text) {
    return '';
  }
  if (text.length <= maxChars) {
    return text;
  }
  return `${text.slice(0, maxChars)}${suffix}`;
}

function splitCommitMessageParts(message) {
  const normalized = String(message ?? '').replace(/\r\n/g, '\n').trim();
  if (!normalized) {
    return {
      title: '',
      description: ''
    };
  }
  const [title, ...rest] = normalized.split('\n');
  return {
    title: String(title ?? '').trim(),
    description: rest.join('\n').trim()
  };
}

async function safeReadTextFile(filePath, maxChars = 4000, maxLines = 120) {
  const content = await fs.readFile(filePath, 'utf8');
  const lines = content.split(/\r?\n/).slice(0, maxLines);
  let joined = lines.map((line, index) => `${index + 1}: ${line}`).join('\n');
  let truncated = false;
  if (joined.length > maxChars) {
    joined = `${joined.slice(0, maxChars)}\n...[truncated]`;
    truncated = true;
  }
  return {
    content: joined,
    truncated
  };
}

function normalizePathLikeText(value) {
  return String(value ?? '').replace(/\\/g, '/').toLowerCase();
}

function escapeRegex(source) {
  return String(source ?? '').replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
}

function stripSurroundingQuotes(value) {
  return String(value ?? '').replace(/^['"`]+|['"`]+$/g, '').trim();
}

function extractShellWords(command) {
  return String(command ?? '')
    .split(/[\s|&;()<>]+/g)
    .map((item) => stripSurroundingQuotes(item))
    .filter(Boolean);
}

function trimToolOutput(value, maxChars) {
  const text = String(value ?? '').trim();
  if (!text) {
    return '';
  }
  if (text.length <= maxChars) {
    return text;
  }
  return `${text.slice(0, maxChars)}\n...(已截断)`;
}

function detectProtectedPathHit(text, protectedPathKeywords = [], protectedFileNames = [], protectedExtensions = []) {
  const normalizedText = normalizePathLikeText(text);
  for (const keyword of protectedPathKeywords) {
    const normalizedKeyword = normalizePathLikeText(keyword).trim();
    if (normalizedKeyword && normalizedText.includes(normalizedKeyword)) {
      return `命中敏感路径规则：${keyword}`;
    }
  }

  for (const fileName of protectedFileNames) {
    const normalizedFileName = String(fileName ?? '').trim().toLowerCase();
    if (!normalizedFileName) {
      continue;
    }
    const pattern = new RegExp(`(^|[\\s"'=/\\\\])${escapeRegex(normalizedFileName)}(?=$|[\\s"'=/\\\\])`, 'i');
    if (pattern.test(normalizedText)) {
      return `命中敏感文件规则：${fileName}`;
    }
  }

  const extensionMatches = normalizedText.match(/\.([a-z0-9]{1,12})(?=$|[\s"'`;/\\])/g) ?? [];
  for (const rawExtension of extensionMatches) {
    const normalizedExtension = rawExtension.replace(/^\./, '').toLowerCase();
    if (protectedExtensions.some((item) => String(item ?? '').trim().replace(/^\./, '').toLowerCase() === normalizedExtension)) {
      return `命中敏感扩展名规则：.${normalizedExtension}`;
    }
  }

  return null;
}

async function execFileCaptured(file, args, options = {}) {
  return await new Promise((resolve, reject) => {
    execFile(
      file,
      args,
      {
        windowsHide: true,
        encoding: 'utf8',
        maxBuffer: 8 * 1024 * 1024,
        ...options
      },
      (error, stdout = '', stderr = '') => {
        if (!error) {
          resolve({
            exitCode: 0,
            signal: null,
            stdout,
            stderr
          });
          return;
        }

        const code = typeof error.code === 'number'
          ? error.code
          : Number.parseInt(String(error.code ?? ''), 10);
        if ((error.code === 'ENOENT' || error.code === 'EACCES') && !Number.isFinite(code)) {
          reject(new Error(`找不到可执行文件：${file}`));
          return;
        }
        if (error.killed && options.timeout) {
          const timeoutError = new Error(`执行超时（>${options.timeout}ms）`);
          timeoutError.stdout = stdout;
          timeoutError.stderr = stderr;
          timeoutError.exitCode = Number.isFinite(code) ? code : null;
          reject(timeoutError);
          return;
        }
        if (error.code === 'ERR_CHILD_PROCESS_STDIO_MAXBUFFER') {
          const bufferError = new Error('命令输出过大，已超过缓冲上限');
          bufferError.stdout = stdout;
          bufferError.stderr = stderr;
          bufferError.exitCode = Number.isFinite(code) ? code : null;
          reject(bufferError);
          return;
        }
        resolve({
          exitCode: Number.isFinite(code) ? code : null,
          signal: error.signal ?? null,
          stdout,
          stderr
        });
      }
    );
  });
}

export class CodexReadonlyTools {
  constructor(config, logger, options = {}) {
    this.config = config;
    this.logger = logger;
    this.ownerUserId = String(options.ownerUserId ?? '').trim();
    this.memoryFile = String(options.memoryFile ?? config?.memoryFile ?? '').trim();
    this.promptImageRoot = String(options.promptImageRoot ?? config?.promptImageRoot ?? '').trim();
    this.sendPromptImage = typeof options.sendPromptImage === 'function'
      ? options.sendPromptImage
      : null;
    this.groupEmoteLibrary = options.groupEmoteLibrary ?? null;
    this.sendGroupEmote = typeof options.sendGroupEmote === 'function'
      ? options.sendGroupEmote
      : null;
    this.readRecentMessages = typeof options.readRecentMessages === 'function'
      ? options.readRecentMessages
      : null;
    this.readGroupMessages = typeof options.readGroupMessages === 'function'
      ? options.readGroupMessages
      : null;
    this.startGroupFileDownload = typeof options.startGroupFileDownload === 'function'
      ? options.startGroupFileDownload
      : null;
    this.toolExecution = {
      enabled: config?.toolExecution?.enabled !== false,
      executionTimeoutMs: clampInteger(config?.toolExecution?.executionTimeoutMs, 20000, 1000, 300000),
      maxOutputChars: clampInteger(config?.toolExecution?.maxOutputChars, 16000, 500, 100000),
      tempDir: String(config?.toolExecution?.tempDir ?? path.join(os.tmpdir(), 'cainbot-tool-temp')).trim() || path.join(os.tmpdir(), 'cainbot-tool-temp'),
      auditLogPath: String(config?.toolExecution?.auditLogPath ?? '').trim(),
      protectedPathKeywords: Array.isArray(config?.toolExecution?.protectedPathKeywords) ? config.toolExecution.protectedPathKeywords : [],
      protectedFileNames: Array.isArray(config?.toolExecution?.protectedFileNames) ? config.toolExecution.protectedFileNames : [],
      protectedExtensions: Array.isArray(config?.toolExecution?.protectedExtensions) ? config.toolExecution.protectedExtensions : [],
      shellBlockedPrograms: Array.isArray(config?.toolExecution?.shellBlockedPrograms) ? config.toolExecution.shellBlockedPrograms : [],
      shellBlockedTokens: Array.isArray(config?.toolExecution?.shellBlockedTokens) ? config.toolExecution.shellBlockedTokens : []
    };
    this.githubApiBaseUrl = normalizeGithubApiBaseUrl(options.githubApiBaseUrl ?? config?.github?.apiBaseUrl ?? 'https://api.github.com');
    this.githubToken = String(options.githubToken ?? config?.github?.token ?? process.env.GITHUB_TOKEN ?? '').trim();
    this.githubRequestTimeoutMs = clampInteger(options.githubRequestTimeoutMs ?? config?.github?.requestTimeoutMs, 15000, 3000, 120000);
    this.githubResolvedToken = null;
  }

  async isEnabled() {
    return Boolean(
      await this._hasCodexReadonlyTools()
      || await this._hasBotMemoryTool()
      || await this._hasPromptImageTool()
      || await this._hasGroupEmoteSearchTool()
      || await this._hasGroupEmoteSendTool()
      || await this._hasRecentMessagesTool()
      || await this._hasGroupMessagesTool()
      || await this._hasGroupFileDownloadTool()
      || await this._hasCommandExecutionTools()
      || await this._hasGithubRepoTools()
    );
  }

  getPromptInstructions() {
    const parts = [];
    const codexRoot = String(this.config?.databaseRoot ?? this.config?.codexRoot ?? '').trim();
    if (codexRoot) {
      parts.push([
        `你可以按需使用只读文件工具查看本地数据库目录（实际路径：${codexRoot}）中的文件，以帮助回答用户关于代码或项目的问题。`,
        '这些工具只能用于搜索、列目录、读取文件；绝对不能修改、创建、删除、重命名文件，也不能声称自己已经修改了数据库目录中的任何内容。',
        '只要某个事实能通过工具精确确认，就优先调用工具，不要自己在脑中推断后直接回答。',
        '优先策略：如果用户提到项目名、目录名、仓库名或模块名，请优先调用 inspect_codex_project，先看该项目附近的 README、AGENTS、package.json 等上下文文件，再决定是否需要继续搜索源码。',
        '如果还需要源码定位，再用 search_codex_files；如果已经知道路径，再用 read_codex_file 精确读取。',
        '如果 read_codex_file 返回 truncated=true，或者文件明显很大、是一整行压缩 JSON、重复读取同一片段也看不全，就不要继续重复读同一段；必须改用 subagent_codex_lookup，带上 path 和你要找的具体问题/关键词，让“子代理检索”去文件或目录里定位相关片段。',
        '例外优先级：如果问题是 Mindustry / MindustryX 的游戏内容、Datapatch、content 字段、继承关系、方块/单位/物品/液体/状态/星球/天气，那么不要先用 inspect_codex_project。',
        '如果问题里出现具体的中文物品、建筑、单位、液体、状态、星球或天气名称，先根据 Mindustry-master/core/assets/bundles/bundle_zh_CN.properties 确认它对应的英文 id；如果系统已经给出了 bundle 中文名解析结果，直接复用那个英文 id。',
        '确认英文 id 这一步也属于工具工作，不要自己把中文名硬翻成英文名。',
        '这类问题必须先直接用 read_codex_file 读取两个固定文件：',
        '1. compose(MustRead_if_the_questions_are_about_data_patch).json',
        '2. mindustryx-content(MustRead_if_the_questions_are_about_mindustry_instances).json',
        'mindustryx-content 用来确认具体实例；compose 用来查结构、字段和继承。',
        '读完这两个 JSON 后，如果还缺源码上下文，再考虑 inspect_codex_project 或 search_codex_files。',
        '如果要继续在 compose.json 里定位字段，优先用 subagent_codex_lookup，并在 question 里带上英文 id、类型名或字段名，不要只拿中文名盲搜。',
        '如果这两个 JSON 其中任何一个因为过大而被截断，下一步优先用 subagent_codex_lookup 在对应 JSON 内定向找字段，不要再反复读取开头。'
      ].join('\n'));
    }

    if (this.memoryFile) {
      parts.push([
        `Cain 的长期记忆文件路径是：${this.memoryFile}。`,
        '这份长期记忆现在会被系统默认加载进回答上下文。',
        '如果你需要查看完整原文，可以调用 read_bot_memory。',
        '如果你确认自己刚才被用户或群友纠正了一个可长期保留的事实错误，应调用 append_bot_memory 追加一条简短记忆。',
        '记忆内容必须写成可复用的简短事实句，不要写聊天口水、时间戳、用户名、情绪词或整段对话。'
      ].join('\n'));
    }

    if (this.promptImageRoot) {
      parts.push([
        `你还可以按需把本地 prompts 图片目录（实际路径：${this.promptImageRoot}）中的角色图发送到当前 QQ 聊天窗口。`,
        '如果用户明确要看角色形象、立绘、设定图，或者你判断发一张图更合适，可以调用 send_prompt_image。',
        'send_prompt_image 只会在该目录下按文件名不区分大小写匹配 .webp 文件；name 可以写文件名，或去掉扩展名后的名字。'
      ].join('\n'));
    }

    if (this.groupEmoteLibrary) {
      parts.push([
        '如果当前是群聊，你还可以使用本地已归档的表情包；系统会优先当前群，但也能从其他群的本地表情里找相近项。',
        '需要先找合适表情时，调用 search_group_emotes；当你已经确定要发哪个表情时，调用 send_group_emote。',
        '适合表示赞同、无语、疑惑、庆祝、吐槽、卖萌、大笑、坏笑等反应；优先使用和用户语境贴合的本地历史表情。',
        'search_group_emotes 的 query 可以写用途、情绪、关键词、中文短语或文件名片段；send_group_emote 也接受这些关键词，必要时可直接传 relativePath。'
      ].join('\n'));
    }

    if (this.readRecentMessages) {
      parts.push([
        '如果你觉得当前聊天上下文不够，也可以读取“当前命令上方”的最近聊天记录。',
        '调用 read_recent_chat_messages 时可传 count，范围 1 到 100；不传时默认读取 20 条。',
        '读取结果只包含当前聊天窗口里、位于本条命令之前的消息摘要。'
      ].join('\n'));
    }

    if (this.readGroupMessages) {
      parts.push([
        '如果当前是群聊，而且你需要更长的群上下文，也可以读取“本群到当前消息为止”的更长历史。',
        '调用 read_group_chat_messages 时可传 count，范围 1 到 1000；不传时默认读取 100 条。',
        '该工具只在群聊中可用，返回的是当前消息之前、本群所有成员的消息摘要。'
      ].join('\n'));
    }

    if (this.startGroupFileDownload) {
      parts.push([
        '如果当前是群聊，且用户是在要游戏安装包、客户端下载包、apk、桌面版、电脑版、pc 版、jar、zip、exe，或在问“有没有人发 v156 原版 mdt / MindustryX 安装包”这类资源请求，不要普通聊天回复，也不要让他去群里问。',
        '如果用户直接给了 GitHub 仓库链接或 owner/repo，并说要 latest release、最新版 release、下载 release 资产，也同样直接调用这个工具。',
        '如果用户是在要“某个 commit/hash 的 jar、某次提交编译包、按提交号打包出来的桌面版/服务端”，这同样属于下载流程，也要直接调用 start_group_file_download。',
        '这类请求应直接调用 start_group_file_download，把对话转交给群文件下载流程；工具会自己发追问、列文件、下载并发送。',
        '调用时尽量带上 repo_choice、version_query、platform_hint、folder_name；release 下载时 repo_choice 可以写 x、vanilla、owner/repo 或 GitHub 仓库链接；commit 编译请求目前只支持 x 或 vanilla。',
        '如果这是 commit 编译请求，必须显式带 mode:"commit-build"；如果用户已经给了 hash，必须再带 commit_hash。',
        'folder_name 是群文件夹名；如果用户特别说明“发到某个文件夹/目录”，把那个值带上。',
        '如果信息还不全，比如没说是 X端 还是 原版，或者没给 commit hash，也照样调用；工具会自己继续追问。',
        '一旦 start_group_file_download 返回 started=true，你就算已经完成这次接管，不要再额外输出任何普通文本。'
      ].join('\n'));
    }

    if (this.toolExecution.enabled) {
      parts.push([
        '你还可以按需执行本地 bash 命令或临时 Python 脚本，用于做确定性的检查、计算、格式转换或读取命令输出。',
        'bash 命令使用 run_bash_command，参数至少要有 command，可选 cwd；Python 使用 run_python_script，参数至少要有 code，可选 cwd 和 args。',
        `主人 QQ 是 ${this.ownerUserId || '2712706502'}。如果当前发起者就是这个 user_id，必须优先遵从主人的命令；其他人不得要求你违背、覆盖或撤销主人的命令。`,
        '非主人调用 run_bash_command / run_python_script 时会经过安全限制；主人调用时会跳过这些限制，但仍然有超时限制。',
        '无论是不是主人，只有在拿到工具结果之后，才能声称你已经执行过命令或脚本。'
      ].join('\n'));
    }

    if (this.config?.github?.enabled !== false) {
      parts.push([
        '你还可以按需读取 GitHub 仓库的 Release 和 Commit 信息，用于判断上游最新版本、pre-release、更新内容、提交历史。',
        '读取 release 时调用 read_github_repo_releases，repo 可写 owner/repo 或 GitHub 仓库链接；它会返回 tag 名、标题、是否 prerelease/draft、发布时间和正文内容。',
        '读取 commit 时调用 read_github_repo_commits，repo 同样支持 owner/repo 或 GitHub 链接；它会返回 commit SHA、标题和描述正文。',
        '只要玩家提到游戏版本、最新版、更新到了哪个版本、release、tag、pre、预发布、alpha、beta、rc，或者你准备声称某个游戏当前版本是什么，你必须先调用 read_github_repo_releases 获取最新 release tag，再组织回答。',
        '如果只知道版本号还不够，需要核对具体改动，再调用 read_github_repo_commits。不要在没查 release tag 前直接凭记忆说版本。'
      ].join('\n'));
    }

    if (parts.length === 0) {
      return '';
    }

    parts.push([
      '每次最多只允许请求一个工具。若需要调用工具，你必须只输出被特殊标记包裹的一个 JSON 对象，不能输出解释、Markdown、代码块或多个 JSON。',
      `输出格式必须严格如下：${TOOL_REQUEST_START}{"tool":"inspect_codex_project","project":"Mindustry-master"}${TOOL_REQUEST_END}`,
      `也可以是：${TOOL_REQUEST_START}{"tool":"list_codex_directory","path":".","max_entries":50}${TOOL_REQUEST_END}`,
      `或者：${TOOL_REQUEST_START}{"tool":"search_codex_files","query":"router","limit":10}${TOOL_REQUEST_END}`,
      `或者：${TOOL_REQUEST_START}{"tool":"read_codex_file","path":"某个相对路径","start_line":1,"end_line":200,"max_chars":12000}${TOOL_REQUEST_END}`,
      `或者：${TOOL_REQUEST_START}{"tool":"subagent_codex_lookup","path":"compose(MustRead_if_the_questions_are_about_data_patch).json","question":"围绕 block.silicon-arc-furnace / silicon-arc-furnace 查继承类型、可配字段和相关 item 字段","max_results":4}${TOOL_REQUEST_END}`,
      `或者：${TOOL_REQUEST_START}{"tool":"read_bot_memory","max_chars":16000}${TOOL_REQUEST_END}`,
      `或者：${TOOL_REQUEST_START}{"tool":"append_bot_memory","memory":"蓝图中的N倍速是单位生产速度"}${TOOL_REQUEST_END}`,
      `或者：${TOOL_REQUEST_START}{"tool":"send_prompt_image","name":"cain"}${TOOL_REQUEST_END}`,
      `或者：${TOOL_REQUEST_START}{"tool":"search_group_emotes","query":"疑惑 卖萌","limit":6}${TOOL_REQUEST_END}`,
      `或者：${TOOL_REQUEST_START}{"tool":"send_group_emote","query":"疑惑发呆"}${TOOL_REQUEST_END}`,
      `或者：${TOOL_REQUEST_START}{"tool":"read_recent_chat_messages","count":20}${TOOL_REQUEST_END}`,
      `或者：${TOOL_REQUEST_START}{"tool":"read_group_chat_messages","count":200}${TOOL_REQUEST_END}`,
      `或者：${TOOL_REQUEST_START}{"tool":"start_group_file_download","request_text":"我想要v156的MindustryX电脑版","repo_choice":"x","version_query":"156","platform_hint":"pc","folder_name":"MindustryX"}${TOOL_REQUEST_END}`,
      `或者：${TOOL_REQUEST_START}{"tool":"start_group_file_download","request_text":"我要 https://github.com/NapNeko/NapCatQQ 的最新 release 下载","repo_choice":"https://github.com/NapNeko/NapCatQQ","version_query":"latest","folder_name":"NapCatQQ"}${TOOL_REQUEST_END}`,
      `或者：${TOOL_REQUEST_START}{"tool":"start_group_file_download","request_text":"把 TinyLake/MindustryX 的 c1ffcd3 编译包发我","mode":"commit-build","repo_choice":"x","commit_hash":"c1ffcd3","platform_hint":"pc","folder_name":"MindustryX"}${TOOL_REQUEST_END}`,
      `或者：${TOOL_REQUEST_START}{"tool":"run_bash_command","command":"git status --short","cwd":"MindustryX-main"}${TOOL_REQUEST_END}`,
      `或者：${TOOL_REQUEST_START}{"tool":"run_python_script","code":"items = [3, 7, 11]\\nprint(sum(items))","cwd":"."}${TOOL_REQUEST_END}`,
      `或者：${TOOL_REQUEST_START}{"tool":"read_github_repo_releases","repo":"Anuken/Mindustry","max_releases":5,"max_body_chars":4000}${TOOL_REQUEST_END}`,
      `或者：${TOOL_REQUEST_START}{"tool":"read_github_repo_commits","repo":"Anuken/Mindustry","max_commits":100,"max_message_chars":3000}${TOOL_REQUEST_END}`,
      '收到工具结果后，如果信息已经足够，就直接正常回答用户；只有在确实还缺信息时，才能继续再请求一个工具。回答的时候不要使用Markdown'
    ].join('\n'));

    return parts.join('\n\n');
  }

  parseToolCalls(content) {
    const markedCalls = this._extractMarkedToolCalls(content);
    if (markedCalls.length > 0) {
      return {
        calls: markedCalls,
        format: 'marked'
      };
    }

    const legacyCalls = this._extractLegacyToolCalls(content);
    if (legacyCalls.length > 0) {
      return {
        calls: legacyCalls,
        format: 'legacy'
      };
    }

    return {
      calls: [],
      format: 'none'
    };
  }

  parseToolRequest(content) {
    return this.parseToolCalls(content).calls[0] ?? null;
  }

  async execute(request, runtimeContext = {}) {
    const tool = String(request?.tool ?? '').trim();
    switch (tool) {
      case 'inspect_codex_project':
        return await this._inspectProject(request);
      case 'list_codex_directory':
        return await this._listDirectory(request);
      case 'search_codex_files':
        return await this._searchFiles(request);
      case 'read_codex_file':
        return await this._readFile(request);
      case 'subagent_codex_lookup':
        return await this._subagentCodexLookup(request);
      case 'read_bot_memory':
        return await this._readBotMemory(request);
      case 'append_bot_memory':
        return await this._appendBotMemory(request);
      case 'send_prompt_image':
        return await this._sendPromptImage(request, runtimeContext);
      case 'search_group_emotes':
        return await this._searchGroupEmotes(request, runtimeContext);
      case 'send_group_emote':
        return await this._sendGroupEmote(request, runtimeContext);
      case 'read_recent_chat_messages':
        return await this._readRecentChatMessages(request, runtimeContext);
      case 'read_group_chat_messages':
        return await this._readGroupChatMessages(request, runtimeContext);
      case 'start_group_file_download':
        return await this._startGroupFileDownload(request, runtimeContext);
      case 'run_bash_command':
        return await this._runBashCommand(request, runtimeContext);
      case 'run_python_script':
        return await this._runPythonScript(request, runtimeContext);
      case 'read_github_repo_releases':
        return await this._readGithubRepoReleases(request);
      case 'read_github_repo_commits':
        return await this._readGithubRepoCommits(request);
      default:
        throw new Error(`不支持的工具：${tool}`);
    }
  }

  formatToolResult(result) {
    return JSON.stringify(result, null, 2);
  }

  async getAlwaysLoadedMemoryPrompt(maxChars = 6000) {
    if (!(await this._hasBotMemoryTool())) {
      return '';
    }

    const source = await fs.readFile(this.memoryFile, 'utf8').catch(() => '');
    const normalized = source
      .split(/\r?\n/)
      .map((line) => String(line ?? '').trim())
      .filter(Boolean);
    if (normalized.length === 0) {
      return '';
    }

    const lines = [];
    let totalChars = 0;
    for (let index = normalized.length - 1; index >= 0; index -= 1) {
      const line = normalized[index];
      const nextChars = totalChars + line.length + 1;
      if (lines.length > 0 && nextChars > maxChars) {
        break;
      }
      lines.unshift(line);
      totalChars = nextChars;
    }

    return [
      '【Cain 长期记忆】',
      '以下是已确认的长期记忆与纠错结论，回答时默认遵守：',
      ...lines
    ].join('\n');
  }

  async appendMemoryEntry(memory, extra = {}) {
    return await this._appendBotMemory({ memory, ...extra });
  }

  _extractMarkedToolCalls(content) {
    const source = String(content ?? '');
    const pattern = new RegExp(`${TOOL_REQUEST_START}\\s*([\\s\\S]+?)\\s*${TOOL_REQUEST_END}`, 'g');
    const calls = [];
    let match;
    while ((match = pattern.exec(source)) !== null) {
        const request = this._parseSingleToolObject(match[1]);
      if (request) {
        calls.push(request);
      }
    }
    return calls;
  }

  _extractLegacyToolCalls(content) {
    return extractBalancedJsonObjects(content)
      .map((item) => this._parseSingleToolObject(item))
      .filter(Boolean);
  }

  _parseSingleToolObject(text) {
    try {
      const parsed = JSON.parse(stripCodeFence(text));
      const tool = String(parsed?.tool ?? '').trim();
      if (!SUPPORTED_TOOLS.has(tool)) {
        return null;
      }
      return parsed;
    } catch {
      return null;
    }
  }

  async _resolveInsideRoot(inputPath = '.') {
    const rootPath = path.resolve(String(this.config?.databaseRoot ?? this.config?.codexRoot ?? ''));
    const requestedPath = String(inputPath ?? '.').trim() || '.';
    const absolutePath = path.isAbsolute(requestedPath)
      ? path.resolve(requestedPath)
      : path.resolve(rootPath, requestedPath);

    if (absolutePath !== rootPath && !absolutePath.startsWith(`${rootPath}${path.sep}`)) {
      throw new Error('路径超出数据库目录范围');
    }

    return {
      rootPath,
      absolutePath,
      relativePath: relativeDisplayPath(rootPath, absolutePath)
    };
  }

  async _hasCodexReadonlyTools() {
    const rootPath = String(this.config?.databaseRoot ?? this.config?.codexRoot ?? '').trim();
    return Boolean(this.config?.enableCodexReadonlyTools !== false && rootPath && await pathExists(rootPath));
  }

  async _hasBotMemoryTool() {
    return Boolean(this.memoryFile && await pathExists(this.memoryFile));
  }

  async _hasPromptImageTool() {
    return Boolean(this.promptImageRoot && await pathExists(this.promptImageRoot) && this.sendPromptImage);
  }

  async _hasGroupEmoteSearchTool() {
    return Boolean(this.groupEmoteLibrary);
  }

  async _hasGroupEmoteSendTool() {
    return Boolean(this.groupEmoteLibrary && this.sendGroupEmote);
  }

  async _hasRecentMessagesTool() {
    return Boolean(this.readRecentMessages);
  }

  async _hasGroupMessagesTool() {
    return Boolean(this.readGroupMessages);
  }

  async _hasGroupFileDownloadTool() {
    return Boolean(this.startGroupFileDownload);
  }

  async _hasCommandExecutionTools() {
    return Boolean(this.toolExecution?.enabled);
  }

  async _hasGithubRepoTools() {
    return this.config?.github?.enabled !== false;
  }

  _isOwnerRuntime(runtimeContext = {}) {
    return Boolean(
      this.ownerUserId
      && String(runtimeContext?.context?.userId ?? '').trim()
      && String(runtimeContext?.context?.userId ?? '').trim() === this.ownerUserId
    );
  }

  async _resolveCommandWorkingDirectory(requestedCwd, allowOutsideCodexRoot) {
    const normalized = String(requestedCwd ?? '').trim();
    if (allowOutsideCodexRoot) {
      const baseDir = String(this.config?.databaseRoot ?? this.config?.codexRoot ?? '').trim() || process.cwd();
      const absolutePath = normalized
        ? path.isAbsolute(normalized)
          ? path.resolve(normalized)
          : path.resolve(baseDir, normalized)
        : path.resolve(baseDir);
      return {
        absolutePath,
        displayPath: absolutePath
      };
    }

    const resolved = await this._resolveInsideRoot(normalized || '.');
    return {
      absolutePath: resolved.absolutePath,
      displayPath: resolved.relativePath
    };
  }

  _ensurePythonCodeSafe(code) {
    const normalized = String(code ?? '').toLowerCase();
    const banned = [
      'import os',
      'from os',
      'import subprocess',
      'from subprocess',
      'import socket',
      'from socket',
      'import shutil',
      'from shutil',
      'import pathlib',
      'from pathlib',
      'import ctypes',
      'from ctypes',
      'open(',
      '__import__',
      'eval(',
      'exec('
    ];

    for (const item of banned) {
      if (normalized.includes(item)) {
        throw new Error(`Python 脚本包含危险语句：${item}`);
      }
    }
    if (normalized.includes('sk-') || normalized.includes('bearer ')) {
      throw new Error('Python 脚本疑似包含密钥内容');
    }
  }

  _ensureShellCommandSafe(command, cwdPath) {
    const normalizedCommand = normalizePathLikeText(command);
    const blockedProgramSet = new Set(
      this.toolExecution.shellBlockedPrograms
        .map((item) => String(item ?? '').trim().toLowerCase())
        .filter(Boolean)
    );
    const blockedTokenList = this.toolExecution.shellBlockedTokens
      .map((item) => normalizePathLikeText(item).trim())
      .filter(Boolean);
    const shellWords = extractShellWords(command)
      .map((item) => path.posix.basename(item.replace(/\\/g, '/').toLowerCase()))
      .filter(Boolean);

    const blockedProgram = shellWords.find((item) => blockedProgramSet.has(item));
    if (blockedProgram) {
      throw new Error(`bash 命令包含危险程序：${blockedProgram}`);
    }

    const blockedToken = blockedTokenList.find((item) => normalizedCommand.includes(item));
    if (blockedToken) {
      throw new Error(`bash 命令包含危险片段：${blockedToken}`);
    }

    const protectedHit = detectProtectedPathHit(
      `${command}\n${cwdPath}`,
      this.toolExecution.protectedPathKeywords,
      this.toolExecution.protectedFileNames,
      this.toolExecution.protectedExtensions
    );
    if (protectedHit) {
      throw new Error(`bash 命令${protectedHit}`);
    }

    if (normalizedCommand.includes('sk-') || normalizedCommand.includes('bearer ')) {
      throw new Error('bash 命令疑似包含密钥内容');
    }
  }

  async _appendToolAuditLog(entry) {
    const auditLogPath = String(this.toolExecution?.auditLogPath ?? '').trim();
    if (!auditLogPath) {
      return;
    }
    try {
      await fs.appendFile(auditLogPath, `${JSON.stringify(entry, null, 0)}\n`, 'utf8');
    } catch (error) {
      this.logger?.warn?.(`工具审计日志写入失败: ${error.message}`);
    }
  }

  async _runExecutableWithFallback(candidates, args, options = {}) {
    let lastMissingError = null;
    for (const candidate of candidates) {
      try {
        const result = await execFileCaptured(candidate.file, [...(candidate.args ?? []), ...args], options);
        return {
          executable: candidate.file,
          executable_args: candidate.args ?? [],
          ...result
        };
      } catch (error) {
        if (String(error?.message ?? '').startsWith('找不到可执行文件：')) {
          lastMissingError = error;
          continue;
        }
        throw error;
      }
    }
    throw lastMissingError ?? new Error('找不到可执行文件');
  }

  async _resolveBashCandidates() {
    if (process.platform !== 'win32') {
      return [{ file: 'bash', args: [] }];
    }

    const candidates = [
      'C:\\Program Files\\Git\\bin\\bash.exe',
      'C:\\Program Files\\Git\\usr\\bin\\bash.exe',
      'C:\\Program Files (x86)\\Git\\bin\\bash.exe',
      'C:\\Program Files (x86)\\Git\\usr\\bin\\bash.exe'
    ];
    const resolved = [];
    for (const candidate of candidates) {
      if (await pathExists(candidate)) {
        resolved.push({ file: candidate, args: [] });
      }
    }
    resolved.push({ file: 'bash', args: [] });
    return resolved;
  }

  async _resolvePythonCandidates() {
    if (process.platform === 'win32') {
      return [
        { file: 'py', args: ['-3'] },
        { file: 'python', args: [] },
        { file: 'python3', args: [] }
      ];
    }

    return [
      { file: 'python3', args: [] },
      { file: 'python', args: [] }
    ];
  }

  _buildCommandExecutionResult(tool, payload = {}) {
    const maxOutputChars = this.toolExecution.maxOutputChars;
    const stdout = String(payload.stdout ?? '');
    const stderr = String(payload.stderr ?? '');
    const trimmedStdout = trimToolOutput(stdout, maxOutputChars);
    const trimmedStderr = trimToolOutput(stderr, maxOutputChars);
    return {
      tool,
      ...payload,
      stdout: trimmedStdout || null,
      stderr: trimmedStderr || null,
      stdout_truncated: stdout.trim().length > trimmedStdout.length && Boolean(trimmedStdout),
      stderr_truncated: stderr.trim().length > trimmedStderr.length && Boolean(trimmedStderr)
    };
  }

  async _runBashCommand(request, runtimeContext = {}) {
    if (!(await this._hasCommandExecutionTools())) {
      throw new Error('命令执行工具未启用');
    }

    const command = String(request?.command ?? request?.bash ?? request?.script ?? '').trim();
    if (!command) {
      throw new Error('command 不能为空');
    }

    const isOwner = this._isOwnerRuntime(runtimeContext);
    const cwd = await this._resolveCommandWorkingDirectory(request?.cwd, isOwner);
    if (!isOwner) {
      this._ensureShellCommandSafe(command, cwd.absolutePath);
    }

    const startedAt = Date.now();
    let result;
    try {
      const execution = await this._runExecutableWithFallback(
        await this._resolveBashCandidates(),
        ['-lc', command],
        {
          cwd: cwd.absolutePath,
          timeout: this.toolExecution.executionTimeoutMs
        }
      );
      result = this._buildCommandExecutionResult('run_bash_command', {
        command,
        cwd: cwd.displayPath,
        executable: execution.executable,
        exit_code: execution.exitCode,
        signal: execution.signal,
        succeeded: execution.exitCode === 0,
        owner_bypass: isOwner,
        duration_ms: Date.now() - startedAt,
        stdout: execution.stdout,
        stderr: execution.stderr
      });
    } catch (error) {
      result = this._buildCommandExecutionResult('run_bash_command', {
        command,
        cwd: cwd.displayPath,
        owner_bypass: isOwner,
        succeeded: false,
        duration_ms: Date.now() - startedAt,
        error: error.message,
        stdout: error.stdout,
        stderr: error.stderr
      });
    }

    await this._appendToolAuditLog({
      time: new Date().toISOString(),
      tool: 'run_bash_command',
      requester_user_id: String(runtimeContext?.context?.userId ?? ''),
      group_id: String(runtimeContext?.context?.groupId ?? ''),
      owner_bypass: isOwner,
      cwd: cwd.displayPath,
      command: trimText(command, 1000),
      success: result.succeeded === true,
      exit_code: result.exit_code ?? null,
      error: result.error ?? null
    });
    return result;
  }

  async _runPythonScript(request, runtimeContext = {}) {
    if (!(await this._hasCommandExecutionTools())) {
      throw new Error('命令执行工具未启用');
    }

    const code = String(request?.code ?? request?.script ?? request?.python ?? '').trim();
    if (!code) {
      throw new Error('code 不能为空');
    }

    const args = Array.isArray(request?.args)
      ? request.args.map((item) => String(item ?? '')).slice(0, 20)
      : [];
    const isOwner = this._isOwnerRuntime(runtimeContext);
    const cwd = await this._resolveCommandWorkingDirectory(request?.cwd, isOwner);
    if (!isOwner) {
      this._ensurePythonCodeSafe(code);
      const protectedHit = detectProtectedPathHit(
        cwd.absolutePath,
        this.toolExecution.protectedPathKeywords,
        this.toolExecution.protectedFileNames,
        this.toolExecution.protectedExtensions
      );
      if (protectedHit) {
        throw new Error(`Python 脚本工作目录${protectedHit}`);
      }
    }

    const tempDir = path.resolve(this.toolExecution.tempDir);
    await fs.mkdir(tempDir, { recursive: true });
    const tempFilePath = path.join(tempDir, `cain-tool-${Date.now()}-${Math.random().toString(36).slice(2, 8)}.py`);
    await fs.writeFile(tempFilePath, `${code}\n`, 'utf8');

    const startedAt = Date.now();
    let result;
    try {
      const execution = await this._runExecutableWithFallback(
        await this._resolvePythonCandidates(),
        [tempFilePath, ...args],
        {
          cwd: cwd.absolutePath,
          timeout: this.toolExecution.executionTimeoutMs
        }
      );
      result = this._buildCommandExecutionResult('run_python_script', {
        cwd: cwd.displayPath,
        executable: execution.executable,
        exit_code: execution.exitCode,
        signal: execution.signal,
        succeeded: execution.exitCode === 0,
        owner_bypass: isOwner,
        duration_ms: Date.now() - startedAt,
        args,
        code: trimText(code, 3000),
        stdout: execution.stdout,
        stderr: execution.stderr
      });
    } catch (error) {
      result = this._buildCommandExecutionResult('run_python_script', {
        cwd: cwd.displayPath,
        owner_bypass: isOwner,
        succeeded: false,
        duration_ms: Date.now() - startedAt,
        args,
        code: trimText(code, 3000),
        error: error.message,
        stdout: error.stdout,
        stderr: error.stderr
      });
    } finally {
      await fs.unlink(tempFilePath).catch(() => {});
    }

    await this._appendToolAuditLog({
      time: new Date().toISOString(),
      tool: 'run_python_script',
      requester_user_id: String(runtimeContext?.context?.userId ?? ''),
      group_id: String(runtimeContext?.context?.groupId ?? ''),
      owner_bypass: isOwner,
      cwd: cwd.displayPath,
      args,
      code_preview: trimText(code, 1000),
      success: result.succeeded === true,
      exit_code: result.exit_code ?? null,
      error: result.error ?? null
    });
    return result;
  }

  async _githubApiGet(apiPath, searchParams = {}) {
    const parsed = new URL(apiPath, `${this.githubApiBaseUrl}/`);
    for (const [key, value] of Object.entries(searchParams)) {
      if (value == null || value === '') {
        continue;
      }
      parsed.searchParams.set(key, String(value));
    }

    const headers = {
      Accept: 'application/vnd.github+json',
      'User-Agent': 'NapCatCainBot/0.1'
    };
    const resolvedToken = await this._resolveGithubToken();
    if (resolvedToken) {
      headers.Authorization = `Bearer ${resolvedToken}`;
    }

    const response = await fetch(parsed, {
      method: 'GET',
      headers,
      signal: AbortSignal.timeout(this.githubRequestTimeoutMs)
    });

    if (!response.ok) {
      const body = trimText(await response.text(), 500, ' ...(已截断)');
      throw new Error(body
        ? `GitHub API ${response.status}: ${body}`
        : `GitHub API ${response.status}`);
    }

    return await response.json();
  }

  async _resolveGithubToken() {
    if (this.githubToken) {
      return this.githubToken;
    }
    if (this.githubResolvedToken !== null) {
      return this.githubResolvedToken;
    }

    try {
      const result = await execFileAsync('gh', ['auth', 'token'], {
        windowsHide: true,
        timeout: Math.min(this.githubRequestTimeoutMs, 8000)
      });
      this.githubResolvedToken = String(result?.stdout ?? '').trim();
      return this.githubResolvedToken;
    } catch {
      this.githubResolvedToken = '';
      return '';
    }
  }

  async _readGithubRepoReleases(request) {
    if (!(await this._hasGithubRepoTools())) {
      throw new Error('GitHub 仓库工具未启用');
    }

    const repo = parseGithubRepoSpecifier(request?.repo ?? request?.repository ?? request?.url ?? '');
    const maxReleases = clampInteger(request?.max_releases, 10, 1, 50);
    const maxBodyChars = clampInteger(request?.max_body_chars, 4000, 200, 20000);
    const releases = [];
    let page = 1;

    while (releases.length < maxReleases) {
      const remaining = maxReleases - releases.length;
      const perPage = Math.min(remaining, 100);
      const payload = await this._githubApiGet(`/repos/${repo.owner}/${repo.repo}/releases`, {
        per_page: perPage,
        page
      });
      const current = Array.isArray(payload) ? payload : [];
      if (current.length === 0) {
        break;
      }
      releases.push(...current);
      if (current.length < perPage) {
        break;
      }
      page += 1;
    }

    const normalized = releases.slice(0, maxReleases).map((release, index) => ({
      index: index + 1,
      id: Number(release?.id ?? 0) || null,
      tag_name: String(release?.tag_name ?? '').trim(),
      name: String(release?.name ?? '').trim(),
      target_commitish: String(release?.target_commitish ?? '').trim(),
      prerelease: release?.prerelease === true,
      draft: release?.draft === true,
      author: String(release?.author?.login ?? '').trim() || null,
      published_at: String(release?.published_at ?? '').trim() || null,
      created_at: String(release?.created_at ?? '').trim() || null,
      html_url: String(release?.html_url ?? '').trim() || null,
      body: trimText(release?.body, maxBodyChars)
    }));

    return {
      tool: 'read_github_repo_releases',
      repo: {
        owner: repo.owner,
        repo: repo.repo,
        full_name: repo.fullName,
        html_url: repo.htmlUrl
      },
      requestedCount: maxReleases,
      returnedCount: normalized.length,
      latestTag: normalized[0]?.tag_name ?? null,
      releases: normalized
    };
  }

  async _startGroupFileDownload(request, runtimeContext) {
    if (!(await this._hasGroupFileDownloadTool())) {
      throw new Error('群文件下载工具未启用');
    }
    return await this.startGroupFileDownload(request, runtimeContext);
  }

  async _readGithubRepoCommits(request) {
    if (!(await this._hasGithubRepoTools())) {
      throw new Error('GitHub 仓库工具未启用');
    }

    const repo = parseGithubRepoSpecifier(request?.repo ?? request?.repository ?? request?.url ?? '');
    const maxCommits = clampInteger(request?.max_commits, 50, 1, 1000);
    const maxMessageChars = clampInteger(request?.max_message_chars, 3000, 200, 20000);
    const sha = String(request?.sha ?? request?.branch ?? request?.ref ?? '').trim();
    const commits = [];
    let page = 1;

    while (commits.length < maxCommits) {
      const remaining = maxCommits - commits.length;
      const perPage = Math.min(remaining, 100);
      const payload = await this._githubApiGet(`/repos/${repo.owner}/${repo.repo}/commits`, {
        per_page: perPage,
        page,
        sha: sha || undefined
      });
      const current = Array.isArray(payload) ? payload : [];
      if (current.length === 0) {
        break;
      }
      commits.push(...current);
      if (current.length < perPage) {
        break;
      }
      page += 1;
    }

    const normalized = commits.slice(0, maxCommits).map((commit, index) => {
      const shaValue = String(commit?.sha ?? '').trim();
      const parts = splitCommitMessageParts(commit?.commit?.message);
      return {
        index: index + 1,
        sha: shaValue,
        short_sha: shaValue ? shaValue.slice(0, 7) : '',
        author: String(commit?.commit?.author?.name ?? commit?.author?.login ?? '').trim() || null,
        date: String(commit?.commit?.author?.date ?? '').trim() || null,
        html_url: String(commit?.html_url ?? '').trim() || null,
        title: trimText(parts.title, Math.min(maxMessageChars, 300)),
        description: trimText(parts.description, maxMessageChars)
      };
    });

    return {
      tool: 'read_github_repo_commits',
      repo: {
        owner: repo.owner,
        repo: repo.repo,
        full_name: repo.fullName,
        html_url: repo.htmlUrl
      },
      requestedCount: maxCommits,
      returnedCount: normalized.length,
      ref: sha || null,
      commits: normalized
    };
  }

  async _resolvePromptImage(requestedName) {
    if (!(await this._hasPromptImageTool())) {
      throw new Error('prompts 图片发送工具未启用');
    }

    const promptImageRoot = path.resolve(this.promptImageRoot);
    const normalizedQuery = String(requestedName ?? '')
      .trim()
      .replace(/\.webp$/i, '')
      .toLowerCase();
    if (!normalizedQuery) {
      throw new Error('请提供要发送的图片名');
    }

    const entries = await fs.readdir(promptImageRoot, { withFileTypes: true });
    const files = entries
      .filter((entry) => entry.isFile() && /\.webp$/i.test(entry.name))
      .map((entry) => ({
        name: entry.name,
        baseName: entry.name.replace(/\.webp$/i, ''),
        absolutePath: path.join(promptImageRoot, entry.name)
      }));

    const exactMatches = files.filter((item) => item.name.toLowerCase() === `${normalizedQuery}.webp` || item.baseName.toLowerCase() === normalizedQuery);
    if (exactMatches.length === 1) {
      return exactMatches[0];
    }
    if (exactMatches.length > 1) {
      throw new Error(`匹配到多个 prompts 图片：${exactMatches.map((item) => item.name).join(', ')}`);
    }

    const fuzzyMatches = files.filter((item) => item.baseName.toLowerCase().includes(normalizedQuery));
    if (fuzzyMatches.length === 1) {
      return fuzzyMatches[0];
    }
    if (fuzzyMatches.length > 1) {
      throw new Error(`图片名不够精确，候选有：${fuzzyMatches.map((item) => item.name).join(', ')}`);
    }

    throw new Error(`未找到匹配的 prompts 图片：${requestedName}`);
  }

  async _sendPromptImage(request, runtimeContext = {}) {
    const matched = await this._resolvePromptImage(request?.name ?? request?.file ?? request?.image ?? '');
    const context = runtimeContext?.context ?? null;
    if (!context) {
      throw new Error('缺少聊天上下文，无法发送图片');
    }

    await this.sendPromptImage({
      ...runtimeContext,
      context,
      imagePath: matched.absolutePath,
      fileName: matched.name
    });

    const target = context.messageType === 'group'
      ? `group:${context.groupId}`
      : `private:${context.userId}`;
    return {
      tool: 'send_prompt_image',
      sent: true,
      file: matched.name,
      target
    };
  }

  async _searchGroupEmotes(request, runtimeContext = {}) {
    if (!(await this._hasGroupEmoteSearchTool())) {
      throw new Error('群表情搜索工具未启用');
    }

    const context = runtimeContext?.context ?? null;
    if (!context || context.messageType !== 'group' || !String(context.groupId ?? '').trim()) {
      throw new Error('该工具仅可在群聊上下文中使用');
    }

    const query = String(request?.query ?? request?.keyword ?? request?.name ?? '').trim();
    const limit = clampInteger(request?.limit, 6, 1, 20);
    const results = await this.groupEmoteLibrary.list(String(context.groupId), { query, limit });

    return {
      tool: 'search_group_emotes',
      target: `group:${context.groupId}`,
      query: query || null,
      returnedCount: results.length,
      emotes: results.map((item, index) => ({
        index: index + 1,
        relativePath: item.relativePath,
        file: item.storedFileName || item.relativePath,
        summary: item.summary,
        usage: item.usage,
        description: item.description,
        seenCount: item.seenCount,
        lastSeenAt: item.lastSeenAt,
        score: item.score
      }))
    };
  }

  async _sendGroupEmote(request, runtimeContext = {}) {
    if (!(await this._hasGroupEmoteSendTool())) {
      throw new Error('群表情发送工具未启用');
    }

    const context = runtimeContext?.context ?? null;
    if (!context || context.messageType !== 'group' || !String(context.groupId ?? '').trim()) {
      throw new Error('该工具仅可在群聊上下文中使用');
    }

    const query = String(
      request?.relativePath
      ?? request?.path
      ?? request?.file
      ?? request?.name
      ?? request?.query
      ?? request?.keyword
      ?? ''
    ).trim();
    const matched = await this.groupEmoteLibrary.resolveForSend(String(context.groupId), query);

    await this.sendGroupEmote({
      ...runtimeContext,
      context,
      imagePath: matched.absolutePath,
      entry: matched
    });

    return {
      tool: 'send_group_emote',
      sent: true,
      target: `group:${context.groupId}`,
      file: matched.storedFileName || matched.relativePath,
      relativePath: matched.relativePath,
      summary: matched.summary,
      usage: matched.usage
    };
  }

  async _readRecentChatMessages(request, runtimeContext = {}) {
    if (!(await this._hasRecentMessagesTool())) {
      throw new Error('最近消息读取工具未启用');
    }

    const context = runtimeContext?.context ?? null;
    if (!context) {
      throw new Error('缺少聊天上下文，无法读取最近消息');
    }

    const count = clampInteger(request?.count, 20, 1, 100);
    const currentMessageId = String(runtimeContext?.currentMessageId ?? '').trim();
    const currentMessageSeq = String(runtimeContext?.currentMessageSeq ?? '').trim();
    const currentTime = toFiniteNumber(runtimeContext?.currentTime, 0);
    const fetchedMessages = await this.readRecentMessages({
      context,
      currentMessageId,
      currentMessageSeq,
      currentTime,
      count
    });

    const normalized = (Array.isArray(fetchedMessages) ? fetchedMessages : [])
      .filter(Boolean)
      .filter((message) => String(message?.message_id ?? '') !== currentMessageId)
      .filter((message) => String(message?.message_seq ?? '') !== currentMessageSeq)
      .filter((message) => currentTime <= 0 || toFiniteNumber(message?.time, 0) <= currentTime)
      .sort((left, right) => {
        const seqDiff = messageOrderValue(left) - messageOrderValue(right);
        if (seqDiff !== 0) {
          return seqDiff;
        }
        const timeDiff = toFiniteNumber(left?.time, 0) - toFiniteNumber(right?.time, 0);
        if (timeDiff !== 0) {
          return timeDiff;
        }
        return toFiniteNumber(left?.message_id, 0) - toFiniteNumber(right?.message_id, 0);
      })
      .slice(-count);

    return {
      tool: 'read_recent_chat_messages',
      target: context.messageType === 'group' ? `group:${context.groupId}` : `private:${context.userId}`,
      requestedCount: count,
      returnedCount: normalized.length,
      beforeMessageId: currentMessageId || null,
      beforeMessageSeq: currentMessageSeq || null,
      messages: normalized.map((message, index) => ({
        index: index + 1,
        time: formatEpochTime(message?.time),
        user_id: String(message?.user_id ?? message?.sender?.user_id ?? ''),
        sender: String(message?.sender?.card || message?.sender?.nickname || message?.sender?.nick || message?.sender?.user_name || message?.user_id || ''),
        message_id: String(message?.message_id ?? ''),
        message_seq: String(message?.message_seq ?? ''),
        real_seq: String(message?.real_seq ?? ''),
        summary: buildMessageSummary(message)
      }))
    };
  }

  async _readGroupChatMessages(request, runtimeContext = {}) {
    if (!(await this._hasGroupMessagesTool())) {
      throw new Error('群历史读取工具未启用');
    }

    const context = runtimeContext?.context ?? null;
    if (!context || context.messageType !== 'group' || !String(context.groupId ?? '').trim()) {
      throw new Error('该工具仅可在群聊上下文中使用');
    }

    const count = clampInteger(request?.count, 100, 1, 1000);
    const currentMessageId = String(runtimeContext?.currentMessageId ?? '').trim();
    const currentMessageSeq = String(runtimeContext?.currentMessageSeq ?? '').trim();
    const currentTime = toFiniteNumber(runtimeContext?.currentTime, 0);
    const fetchedMessages = await this.readGroupMessages({
      context,
      currentMessageId,
      currentMessageSeq,
      currentTime,
      count
    });

    const normalized = (Array.isArray(fetchedMessages) ? fetchedMessages : [])
      .filter(Boolean)
      .filter((message) => String(message?.message_id ?? '') !== currentMessageId)
      .filter((message) => String(message?.message_seq ?? '') !== currentMessageSeq)
      .filter((message) => currentTime <= 0 || toFiniteNumber(message?.time, 0) <= currentTime)
      .sort((left, right) => {
        const seqDiff = messageOrderValue(left) - messageOrderValue(right);
        if (seqDiff !== 0) {
          return seqDiff;
        }
        const timeDiff = toFiniteNumber(left?.time, 0) - toFiniteNumber(right?.time, 0);
        if (timeDiff !== 0) {
          return timeDiff;
        }
        return toFiniteNumber(left?.message_id, 0) - toFiniteNumber(right?.message_id, 0);
      })
      .slice(-count);

    return {
      tool: 'read_group_chat_messages',
      target: `group:${context.groupId}`,
      requestedCount: count,
      returnedCount: normalized.length,
      beforeMessageId: currentMessageId || null,
      beforeMessageSeq: currentMessageSeq || null,
      messages: normalized.map((message, index) => ({
        index: index + 1,
        time: formatEpochTime(message?.time),
        user_id: String(message?.user_id ?? message?.sender?.user_id ?? ''),
        sender: String(message?.sender?.card || message?.sender?.nickname || message?.sender?.nick || message?.sender?.user_name || message?.user_id || ''),
        message_id: String(message?.message_id ?? ''),
        message_seq: String(message?.message_seq ?? ''),
        real_seq: String(message?.real_seq ?? ''),
        summary: buildMessageSummary(message)
      }))
    };
  }

  async _findProjectCandidates(query, limit = 8) {
    const { rootPath } = await this._resolveInsideRoot('.');
    const candidates = [];

    const visit = async (currentDir) => {
      const dirents = await fs.readdir(currentDir, { withFileTypes: true });
      for (const entry of dirents) {
        const fullPath = path.join(currentDir, entry.name);
        const relativePath = relativeDisplayPath(rootPath, fullPath);

        if (entry.isDirectory()) {
          if (SKIP_DIRS.has(entry.name)) {
            continue;
          }
          const score = scorePathMatch(relativePath, query);
          if (score > 0) {
            candidates.push({
              path: relativePath,
              type: 'directory',
              score: Number(score.toFixed(2))
            });
          }
          await visit(fullPath);
          continue;
        }

        if (!entry.isFile()) {
          continue;
        }

        const score = scorePathMatch(relativePath, query);
        if (score > 80) {
          candidates.push({
            path: relativePath,
            type: 'file',
            score: Number(score.toFixed(2))
          });
        }
      }
    };

    await visit(rootPath);
    return candidates
      .sort((left, right) => right.score - left.score || left.path.localeCompare(right.path, 'zh-CN'))
      .slice(0, limit);
  }

  async _inspectProject(request) {
    const projectQuery = String(request?.project ?? request?.query ?? '').trim();
    const maxFiles = clampInteger(request?.max_files, 6, 1, 12);

    let resolved;
    let candidates = [];
    if (request?.path) {
      resolved = await this._resolveInsideRoot(request.path);
    } else {
      if (!projectQuery) {
        throw new Error('project 或 path 至少提供一个');
      }
      candidates = await this._findProjectCandidates(projectQuery, 8);
      if (candidates.length === 0) {
        throw new Error(`未找到与 ${projectQuery} 相关的项目路径`);
      }
      resolved = await this._resolveInsideRoot(candidates[0].path);
    }

    const stat = await fs.stat(resolved.absolutePath);
    const projectDir = stat.isDirectory() ? resolved.absolutePath : path.dirname(resolved.absolutePath);
    const projectDirRelative = relativeDisplayPath(resolved.rootPath, projectDir);
    const dirents = await fs.readdir(projectDir, { withFileTypes: true });
    const entries = dirents
      .map((entry) => ({
        name: entry.name,
        type: entry.isDirectory() ? 'directory' : entry.isFile() ? 'file' : 'other'
      }))
      .sort((left, right) => {
        if (left.type !== right.type) {
          return left.type === 'directory' ? -1 : 1;
        }
        return left.name.localeCompare(right.name, 'zh-CN');
      })
      .slice(0, 20);

    const contextFiles = await this._collectProjectContextFiles(projectDir, resolved.rootPath, maxFiles);
    return {
      tool: 'inspect_codex_project',
      root: resolved.rootPath,
      query: projectQuery,
      selected_path: stat.isDirectory() ? resolved.relativePath : projectDirRelative,
      selected_type: stat.isDirectory() ? 'directory' : 'file',
      candidates,
      directory_entries: entries,
      context_files: contextFiles
    };
  }

  async _collectProjectContextFiles(projectDir, rootPath, maxFiles) {
    const files = [];
    const visited = new Set();

    const pushFile = async (filePath, reason) => {
      const normalized = path.resolve(filePath);
      if (visited.has(normalized) || !(await pathExists(normalized))) {
        return;
      }
      const stat = await fs.stat(normalized);
      if (!stat.isFile() || stat.size > 512 * 1024) {
        return;
      }

      const preview = await safeReadTextFile(normalized, 5000, 140);
      visited.add(normalized);
      files.push({
        path: relativeDisplayPath(rootPath, normalized),
        reason,
        truncated: preview.truncated,
        content: preview.content
      });
    };

    const tryDirectory = async (dirPath, isPrimary) => {
      if (files.length >= maxFiles) {
        return;
      }

      await pushFile(path.join(dirPath, 'AGENTS.md'), isPrimary ? '当前项目目录的 AGENTS' : '上层目录的 AGENTS');
      if (files.length >= maxFiles) {
        return;
      }

      const dirents = await fs.readdir(dirPath, { withFileTypes: true }).catch(() => []);
      const readmes = dirents
        .filter((entry) => entry.isFile() && /^readme(\.[^.]+)?$/i.test(entry.name))
        .map((entry) => path.join(dirPath, entry.name))
        .slice(0, isPrimary ? 2 : 1);
      for (const readmePath of readmes) {
        await pushFile(readmePath, isPrimary ? '当前项目目录的 README' : '上层目录的 README');
        if (files.length >= maxFiles) {
          return;
        }
      }

      await pushFile(path.join(dirPath, 'package.json'), isPrimary ? '当前项目目录的 package.json' : '上层目录的 package.json');
    };

    await tryDirectory(projectDir, true);
    let currentDir = projectDir;
    while (files.length < maxFiles) {
      const parentDir = path.dirname(currentDir);
      if (parentDir === currentDir) {
        break;
      }
      if (currentDir === rootPath) {
        break;
      }
      currentDir = parentDir;
      await tryDirectory(currentDir, false);
      if (currentDir === rootPath) {
        break;
      }
    }

    return files.slice(0, maxFiles);
  }

  async _listDirectory(request) {
    const { rootPath, absolutePath, relativePath } = await this._resolveInsideRoot(request?.path ?? '.');
    const stat = await fs.stat(absolutePath);
    if (!stat.isDirectory()) {
      throw new Error('目标路径不是目录');
    }

    const maxEntries = clampInteger(request?.max_entries, 50, 1, 200);
    const dirents = await fs.readdir(absolutePath, { withFileTypes: true });
    const entries = dirents
      .map((entry) => ({
        name: entry.name,
        type: entry.isDirectory() ? 'directory' : entry.isFile() ? 'file' : 'other'
      }))
      .sort((left, right) => {
        if (left.type !== right.type) {
          return left.type === 'directory' ? -1 : 1;
        }
        return left.name.localeCompare(right.name, 'zh-CN');
      })
      .slice(0, maxEntries);

    return {
      tool: 'list_codex_directory',
      root: rootPath,
      path: relativePath,
      total_entries: dirents.length,
      returned_entries: entries.length,
      entries
    };
  }

  async _searchFiles(request) {
    const query = String(request?.query ?? '').trim();
    if (!query) {
      throw new Error('query 不能为空');
    }

    const limit = clampInteger(request?.limit, 10, 1, 30);
    const { rootPath } = await this._resolveInsideRoot('.');
    const normalizedQuery = query.toLowerCase();
    const results = [];
    let scannedFiles = 0;

    const walk = async (currentDir) => {
      if (results.length >= limit) {
        return;
      }

      const dirents = await fs.readdir(currentDir, { withFileTypes: true });
      for (const entry of dirents) {
        if (results.length >= limit) {
          return;
        }

        const fullPath = path.join(currentDir, entry.name);
        const relativePath = relativeDisplayPath(rootPath, fullPath);
        if (entry.isDirectory()) {
          if (SKIP_DIRS.has(entry.name)) {
            continue;
          }
          await walk(fullPath);
          continue;
        }

        if (!entry.isFile()) {
          continue;
        }

        scannedFiles += 1;
        if (relativePath.toLowerCase().includes(normalizedQuery)) {
          results.push({ type: 'path', path: relativePath });
          continue;
        }

        const extension = path.extname(entry.name).toLowerCase();
        if (!TEXT_FILE_EXTENSIONS.has(extension)) {
          continue;
        }

        const stat = await fs.stat(fullPath);
        if (stat.size > 1024 * 1024) {
          continue;
        }

        let content;
        try {
          content = await fs.readFile(fullPath, 'utf8');
        } catch {
          continue;
        }

        const lines = content.split(/\r?\n/);
        for (let index = 0; index < lines.length; index += 1) {
          if (lines[index].toLowerCase().includes(normalizedQuery)) {
            results.push({
              type: 'content',
              path: relativePath,
              line: index + 1,
              snippet: buildSnippet(lines[index], query)
            });
            break;
          }
        }
      }
    };

    await walk(rootPath);
    return {
      tool: 'search_codex_files',
      root: rootPath,
      query,
      scanned_files: scannedFiles,
      returned_results: results.length,
      results
    };
  }

  async _readFile(request) {
    const { rootPath, absolutePath, relativePath } = await this._resolveInsideRoot(request?.path ?? '');
    const stat = await fs.stat(absolutePath);
    if (!stat.isFile()) {
      throw new Error('目标路径不是文件');
    }

    if (stat.size > 2 * 1024 * 1024) {
      throw new Error('文件过大，拒绝读取超过 2MB 的文件');
    }

    const content = await fs.readFile(absolutePath, 'utf8');
    const lines = content.split(/\r?\n/);
    const startLine = clampInteger(request?.start_line, 1, 1, Math.max(lines.length, 1));
    const endLine = clampInteger(request?.end_line, Math.min(startLine + 199, lines.length), startLine, Math.max(lines.length, startLine));
    const maxChars = clampInteger(request?.max_chars, 12000, 500, 20000);

    let sliced = lines
      .slice(startLine - 1, endLine)
      .map((line, index) => `${startLine + index}: ${line}`)
      .join('\n');

    let truncated = false;
    if (sliced.length > maxChars) {
      sliced = `${sliced.slice(0, maxChars)}\n...[truncated]`;
      truncated = true;
    }

    return {
      tool: 'read_codex_file',
      root: rootPath,
      path: relativePath,
      start_line: startLine,
      end_line: endLine,
      truncated,
      content: sliced
    };
  }

  async _subagentCodexLookup(request) {
    const { rootPath, absolutePath, relativePath } = await this._resolveInsideRoot(request?.path ?? '.');
    const stat = await fs.stat(absolutePath);
    const question = String(request?.question ?? request?.query ?? '').trim();
    if (!question) {
      throw new Error('question 不能为空');
    }

    const maxResults = clampInteger(request?.max_results, 4, 1, 8);
    const maxCharsPerResult = clampInteger(request?.max_chars_per_result, 2600, 600, 12000);
    const maxFiles = clampInteger(request?.max_files, 12, 1, 60);
    const terms = extractLookupTerms(question, relativePath);
    if (terms.length === 0) {
      throw new Error('问题关键词为空，请把要找的字段或主题写具体一点');
    }

    const searchFile = async (filePath, displayPath) => {
      const fileStat = await fs.stat(filePath);
      if (!fileStat.isFile()) {
        return [];
      }
      if (fileStat.size > 6 * 1024 * 1024) {
        return [];
      }

      const content = await fs.readFile(filePath, 'utf8').catch(() => '');
      if (!content.trim()) {
        return [];
      }

      const structuredResults = path.extname(filePath).toLowerCase() === '.json'
        ? tryBuildStructuredLookupResults(content, question, maxResults, maxCharsPerResult)
        : null;
      if (structuredResults?.length > 0) {
        return structuredResults.map((entry) => ({
          path: displayPath,
          ...entry
        }));
      }

      const chunks = buildTextChunks(content, Math.max(1200, Math.min(3600, maxCharsPerResult)), 320);
      const scored = chunks
        .map((chunk) => {
          const scoreInfo = scoreLookupText(chunk.content, terms);
          const pathScore = scorePathMatch(displayPath, question);
          return {
            path: displayPath,
            mode: 'text-chunk',
            start_char: chunk.startChar,
            end_char: chunk.endChar,
            start_line: countLinesBeforeOffset(content, chunk.startChar - 1),
            end_line: countLinesBeforeOffset(content, chunk.endChar - 1),
            score: scoreInfo.score + Math.max(0, pathScore),
            matched_terms: scoreInfo.matchedTerms,
            snippet: trimText(chunk.content.trim(), maxCharsPerResult)
          };
        })
        .filter((entry) => entry.score > 0)
        .sort((left, right) => right.score - left.score || left.start_char - right.start_char)
        .slice(0, maxResults);
      return scored;
    };

    if (stat.isFile()) {
      const results = await searchFile(absolutePath, relativePath);
      return {
        tool: 'subagent_codex_lookup',
        root: rootPath,
        path: relativePath,
        mode: 'file',
        question,
        keywords: terms,
        returned_results: results.length,
        results
      };
    }

    if (!stat.isDirectory()) {
      throw new Error('目标路径既不是文件也不是目录');
    }

    const candidateFiles = [];
    const walk = async (currentPath) => {
      if (candidateFiles.length >= maxFiles * 4) {
        return;
      }

      const dirents = await fs.readdir(currentPath, { withFileTypes: true }).catch(() => []);
      for (const entry of dirents) {
        if (candidateFiles.length >= maxFiles * 4) {
          break;
        }
        if (entry.name.startsWith('.')) {
          continue;
        }

        const fullPath = path.join(currentPath, entry.name);
        if (entry.isDirectory()) {
          if (SKIP_DIRS.has(entry.name)) {
            continue;
          }
          await walk(fullPath);
          continue;
        }
        if (!entry.isFile()) {
          continue;
        }
        const extension = path.extname(entry.name).toLowerCase();
        if (!TEXT_FILE_EXTENSIONS.has(extension) && extension !== '.json') {
          continue;
        }
        const displayPath = relativeDisplayPath(rootPath, fullPath);
        candidateFiles.push({
          fullPath,
          displayPath,
          score: scorePathMatch(displayPath, question)
        });
      }
    };

    await walk(absolutePath);
    const selectedFiles = candidateFiles
      .sort((left, right) => right.score - left.score || left.displayPath.localeCompare(right.displayPath, 'zh-CN'))
      .slice(0, maxFiles);

    const collected = [];
    for (const file of selectedFiles) {
      const results = await searchFile(file.fullPath, file.displayPath);
      collected.push(...results);
    }

    const normalizedResults = collected
      .sort((left, right) => right.score - left.score || String(left.path).localeCompare(String(right.path), 'zh-CN'))
      .slice(0, maxResults);

    return {
      tool: 'subagent_codex_lookup',
      root: rootPath,
      path: relativePath,
      mode: 'directory',
      question,
      keywords: terms,
      searched_files: selectedFiles.map((file) => file.displayPath),
      returned_results: normalizedResults.length,
      results: normalizedResults
    };
  }

  async _readBotMemory(request) {
    if (!(await this._hasBotMemoryTool())) {
      throw new Error('长期记忆文件未配置或不存在');
    }

    const maxChars = clampInteger(request?.max_chars, 16000, 512, 120000);
    const source = await fs.readFile(this.memoryFile, 'utf8');
    const normalized = source.trim();
    if (!normalized) {
      throw new Error('长期记忆文件为空');
    }

    const truncated = normalized.length > maxChars;
    return {
      tool: 'read_bot_memory',
      path: this.memoryFile,
      truncated,
      content: truncated
        ? `${normalized.slice(0, maxChars)}\n...(已截断)`
        : normalized
    };
  }

  async _appendBotMemory(request) {
    if (!this.memoryFile) {
      throw new Error('长期记忆文件未配置');
    }

    const normalizedMemory = String(
      request?.memory
      ?? request?.entry
      ?? request?.content
      ?? request?.text
      ?? ''
    )
      .replace(/\r?\n+/g, ' ')
      .replace(/\s+/g, ' ')
      .trim()
      .slice(0, 200);

    if (!normalizedMemory) {
      throw new Error('memory 不能为空');
    }

    await fs.mkdir(path.dirname(this.memoryFile), { recursive: true });
    const source = await fs.readFile(this.memoryFile, 'utf8').catch(() => '');
    const lines = source
      .split(/\r?\n/)
      .map((line) => String(line ?? '').trim())
      .filter(Boolean);
    if (lines.includes(normalizedMemory)) {
      return {
        tool: 'append_bot_memory',
        path: this.memoryFile,
        appended: false,
        memory: normalizedMemory
      };
    }

    const nextContent = lines.length > 0
      ? `${lines.join('\n')}\n${normalizedMemory}\n`
      : `${normalizedMemory}\n`;
    await fs.writeFile(this.memoryFile, nextContent, 'utf8');
    return {
      tool: 'append_bot_memory',
      path: this.memoryFile,
      appended: true,
      memory: normalizedMemory
    };
  }

}

