import fs from 'node:fs/promises';
import path from 'node:path';

import { normalizeMessageSegments, pathExists } from './utils.mjs';

export const TEXT_FILE_EXTENSIONS = new Set([
  '.txt', '.md', '.markdown', '.json', '.jsonc', '.yaml', '.yml', '.toml', '.ini', '.cfg', '.conf',
  '.js', '.mjs', '.cjs', '.ts', '.tsx', '.jsx', '.java', '.kt', '.kts', '.gradle', '.properties',
  '.xml', '.html', '.css', '.scss', '.less', '.py', '.rb', '.php', '.go', '.rs', '.cpp', '.c', '.h',
  '.hpp', '.cs', '.sh', '.ps1', '.bat', '.cmd', '.sql', '.csv', '.env', '.gitignore', '.gitattributes',
  '.vue', '.svelte', '.lua', '.log'
]);

function clampInteger(value, fallback, min, max) {
  const numeric = Number(value);
  if (!Number.isFinite(numeric)) {
    return fallback;
  }
  return Math.min(max, Math.max(min, Math.trunc(numeric)));
}

function isTextContentType(contentType = '') {
  const normalized = String(contentType ?? '').toLowerCase();
  return normalized.startsWith('text/')
    || normalized.includes('json')
    || normalized.includes('xml')
    || normalized.includes('javascript')
    || normalized.includes('x-www-form-urlencoded');
}

function hasTextExtension(fileName = '') {
  const normalized = String(fileName ?? '').trim().toLowerCase();
  if (!normalized) {
    return false;
  }
  const extension = path.extname(normalized);
  return TEXT_FILE_EXTENSIONS.has(extension);
}

function isProbablyTextBuffer(buffer) {
  if (!buffer || buffer.length === 0) {
    return true;
  }
  const sample = buffer.subarray(0, Math.min(buffer.length, 4096));
  let suspicious = 0;
  for (const byte of sample) {
    if (byte === 0) {
      return false;
    }
    const isAllowedControl = byte === 9 || byte === 10 || byte === 13;
    const isPrintableAscii = byte >= 32 && byte <= 126;
    const isUtf8LeadOrTrail = byte >= 128;
    if (!isAllowedControl && !isPrintableAscii && !isUtf8LeadOrTrail) {
      suspicious += 1;
    }
  }
  return suspicious / sample.length < 0.08;
}

function decodeTextBuffer(buffer) {
  if (!buffer || buffer.length === 0) {
    return '';
  }
  if (buffer.length >= 3 && buffer[0] === 0xef && buffer[1] === 0xbb && buffer[2] === 0xbf) {
    return buffer.subarray(3).toString('utf8');
  }
  if (buffer.length >= 2 && buffer[0] === 0xff && buffer[1] === 0xfe) {
    return buffer.subarray(2).toString('utf16le');
  }
  if (buffer.length >= 2 && buffer[0] === 0xfe && buffer[1] === 0xff) {
    const swapped = Buffer.allocUnsafe(buffer.length - 2);
    for (let index = 2; index < buffer.length; index += 2) {
      swapped[index - 2] = buffer[index + 1] ?? 0;
      swapped[index - 1] = buffer[index] ?? 0;
    }
    return swapped.toString('utf16le');
  }
  return buffer.toString('utf8');
}

async function fetchFileBuffer(url, options = {}) {
  const timeoutMs = clampInteger(options.timeoutMs, 15000, 1000, 60000);
  const maxBytes = clampInteger(options.maxBytes, 512000, 4096, 4 * 1024 * 1024);
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), timeoutMs);
  try {
    const response = await fetch(url, {
      method: 'GET',
      headers: {
        Accept: 'text/plain, text/markdown, application/json, application/xml, text/*;q=0.9, */*;q=0.1'
      },
      signal: controller.signal,
      redirect: 'follow'
    });

    if (!response.ok) {
      throw new Error(`读取文件返回 HTTP ${response.status}`);
    }

    const contentLength = Number(response.headers.get('content-length') ?? 0);
    if (contentLength > maxBytes) {
      throw new Error(`文件过大（${contentLength} bytes），超过限制 ${maxBytes}`);
    }

    const arrayBuffer = await response.arrayBuffer();
    const buffer = Buffer.from(arrayBuffer);
    if (buffer.length > maxBytes) {
      throw new Error(`文件过大（${buffer.length} bytes），超过限制 ${maxBytes}`);
    }

    return {
      buffer,
      contentType: String(response.headers.get('content-type') ?? '').trim()
    };
  } finally {
    clearTimeout(timer);
  }
}

function truncateText(text, maxChars) {
  const normalized = String(text ?? '');
  if (normalized.length <= maxChars) {
    return { text: normalized, truncated: false };
  }
  return {
    text: `${normalized.slice(0, maxChars)}\n...(已截断)`,
    truncated: true
  };
}

export function extractFileSegments(message) {
  return normalizeMessageSegments(message)
    .filter((segment) => segment?.type === 'file' && segment?.data)
    .map((segment) => ({ ...segment.data }));
}

export async function readTextFileFromSegment(napcatClient, fileSegment, options = {}) {
  const fileId = String(fileSegment?.file_id ?? '').trim();
  const fallbackFileName = String(fileSegment?.name ?? fileSegment?.file ?? '').trim();
  const maxChars = clampInteger(options.maxChars, 12000, 256, 120000);
  const maxBytes = clampInteger(options.maxBytes, 512000, 4096, 4 * 1024 * 1024);

  if (!fileId) {
    throw new Error('该消息文件缺少 file_id，无法读取');
  }

  const fileInfo = await napcatClient.getFile(fileId);
  const fileName = String(fileInfo?.file_name ?? fallbackFileName ?? '').trim() || '未命名文件';
  const contentType = String(fileInfo?.content_type ?? '').trim();

  let buffer = null;
  if (typeof fileInfo?.base64 === 'string' && fileInfo.base64.length > 0) {
    buffer = Buffer.from(fileInfo.base64, 'base64');
  } else if (isProbablyTextPath(fileInfo?.file) && await pathExists(fileInfo.file)) {
    buffer = await fs.readFile(fileInfo.file);
  } else if (typeof fileInfo?.url === 'string' && fileInfo.url.trim()) {
    const fetched = await fetchFileBuffer(fileInfo.url, { maxBytes, timeoutMs: options.timeoutMs });
    buffer = fetched.buffer;
  }

  if (!buffer) {
    throw new Error('NapCat 没有返回可读取的文件内容');
  }

  if (buffer.length > maxBytes) {
    throw new Error(`文件过大（${buffer.length} bytes），超过限制 ${maxBytes}`);
  }

  if (!hasTextExtension(fileName) && !isTextContentType(contentType) && !isProbablyTextBuffer(buffer)) {
    throw new Error(`只支持纯文本文件，当前文件看起来不是文本：${fileName}`);
  }

  const decoded = decodeTextBuffer(buffer);
  const normalizedText = decoded.replace(/\u0000/g, '').trim();
  if (!normalizedText) {
    throw new Error(`文件为空，或无法解码为可读文本：${fileName}`);
  }

  const truncated = truncateText(normalizedText, maxChars);
  return {
    fileId,
    fileName,
    fileSize: Number(fileInfo?.file_size ?? buffer.length ?? 0),
    text: truncated.text,
    truncated: truncated.truncated
  };
}

function isProbablyTextPath(filePath) {
  const normalized = String(filePath ?? '').trim();
  return Boolean(normalized && path.isAbsolute(normalized));
}

export async function readTextFilesFromMessage(napcatClient, message, options = {}) {
  const fileSegments = extractFileSegments(message);
  const maxFiles = clampInteger(options.maxFiles, 3, 1, 8);
  const results = [];
  const errors = [];

  for (const fileSegment of fileSegments.slice(0, maxFiles)) {
    try {
      results.push(await readTextFileFromSegment(napcatClient, fileSegment, options));
    } catch (error) {
      errors.push(String(error?.message ?? error));
    }
  }

  return {
    files: results,
    errors
  };
}

