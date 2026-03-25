import crypto from 'node:crypto';
import fs from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const MIME_BY_EXTENSION = new Map([
  ['.png', 'image/png'],
  ['.jpg', 'image/jpeg'],
  ['.jpeg', 'image/jpeg'],
  ['.gif', 'image/gif'],
  ['.webp', 'image/webp'],
  ['.bmp', 'image/bmp'],
  ['.svg', 'image/svg+xml']
]);

export function escapeRegExp(value) {
  return String(value).replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
}

export function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

export async function ensureDir(dirPath) {
  await fs.mkdir(dirPath, { recursive: true });
}

export function sha1(value) {
  return crypto.createHash('sha1').update(String(value)).digest('hex');
}

export function normalizePath(value) {
  return path.normalize(value);
}

export function resolveMaybeRelative(baseDir, targetPath) {
  if (!targetPath) {
    return targetPath;
  }
  return path.isAbsolute(targetPath)
    ? normalizePath(targetPath)
    : normalizePath(path.resolve(baseDir, targetPath));
}

export function stripCqCodes(text) {
  return String(text ?? '').replace(/\[CQ:[^\]]+]/g, '');
}

export function plainTextFromMessage(message, rawMessage = '') {
  if (Array.isArray(message)) {
    const text = message
      .filter((segment) => segment?.type === 'text')
      .map((segment) => segment?.data?.text ?? '')
      .join('')
      .trim();
    if (text) {
      return text;
    }
  }

  if (message && typeof message === 'object' && message.type === 'text') {
    const text = String(message?.data?.text ?? '').trim();
    if (text) {
      return text;
    }
  }

  if (typeof message === 'string' && message.trim()) {
    return stripCqCodes(message).trim();
  }

  return stripCqCodes(String(rawMessage ?? '')).trim();
}

export function extractReplyId(message, rawMessage = '') {
  if (Array.isArray(message)) {
    const reply = message.find((segment) => segment?.type === 'reply' && segment?.data?.id != null);
    if (reply) {
      return String(reply.data.id);
    }
  }

  if (message && typeof message === 'object' && message.type === 'reply' && message?.data?.id != null) {
    return String(message.data.id);
  }

  const match = String(rawMessage ?? '').match(/\[CQ:reply,id=([^,\]]+)/i);
  return match ? match[1] : null;
}

export function normalizeMessageSegments(message) {
  if (Array.isArray(message)) {
    return message.filter(Boolean);
  }
  if (message && typeof message === 'object') {
    return [message];
  }
  return [];
}

export function extractImageSegments(message) {
  return normalizeMessageSegments(message)
    .filter((segment) => segment?.type === 'image' && segment?.data)
    .map((segment) => segment.data);
}

export function countMessageSegments(message, type) {
  return normalizeMessageSegments(message)
    .filter((segment) => segment?.type === type)
    .length;
}

export function toOpenAiTextContent(text) {
  return {
    type: 'text',
    text: String(text ?? '')
  };
}

export async function toDataUrlFromLocalImage(filePath) {
  const buffer = await fs.readFile(filePath);
  const extension = path.extname(filePath).toLowerCase();
  const mimeType = MIME_BY_EXTENSION.get(extension) ?? 'application/octet-stream';
  return `data:${mimeType};base64,${buffer.toString('base64')}`;
}

export async function imageSegmentToOpenAiContent(data) {
  const candidates = [data?.url, data?.file, data?.path, data?.thumb]
    .map((item) => String(item ?? '').trim())
    .filter(Boolean);

  for (const candidate of candidates) {
    if (/^data:/i.test(candidate)) {
      return { type: 'image_url', image_url: { url: candidate } };
    }
    if (/^https?:\/\//i.test(candidate)) {
      return { type: 'image_url', image_url: { url: candidate } };
    }
    if (/^file:\/\//i.test(candidate)) {
      const localPath = fileURLToPath(candidate);
      if (await pathExists(localPath)) {
        return { type: 'image_url', image_url: { url: await toDataUrlFromLocalImage(localPath) } };
      }
    }
    if (path.isAbsolute(candidate) && await pathExists(candidate)) {
      return { type: 'image_url', image_url: { url: await toDataUrlFromLocalImage(candidate) } };
    }
  }

  return null;
}

export function buildReplyMessage(replyToMessageId, text) {
  const segments = [];
  if (replyToMessageId != null) {
    segments.push({
      type: 'reply',
      data: { id: String(replyToMessageId) }
    });
  }
  segments.push({
    type: 'text',
    data: { text: String(text ?? '') }
  });
  return segments;
}

export function buildForwardNodes(text, options = {}) {
  const chunks = splitText(text, options.maxChunkLength ?? 300);
  const userId = String(options.userId ?? '0');
  const nickname = String(options.nickname ?? 'Cain');
  const time = String(options.time ?? Math.floor(Date.now() / 1000));

  return chunks.map((chunk) => ({
    type: 'node',
    data: {
      user_id: userId,
      nickname,
      content: [
        {
          type: 'text',
          data: { text: chunk }
        }
      ],
      time
    }
  }));
}

export function splitText(text, maxLength = 1500) {
  const source = String(text ?? '');
  if (source.length <= maxLength) {
    return [source];
  }

  const parts = [];
  let remaining = source;
  while (remaining.length > maxLength) {
    let cutIndex = remaining.lastIndexOf('\n', maxLength);
    if (cutIndex < Math.floor(maxLength / 2)) {
      cutIndex = maxLength;
    }
    parts.push(remaining.slice(0, cutIndex).trim());
    remaining = remaining.slice(cutIndex).trimStart();
  }
  if (remaining) {
    parts.push(remaining);
  }
  return parts.filter(Boolean);
}

export function nowIso() {
  return new Date().toISOString();
}

export function isNonEmptyString(value) {
  return typeof value === 'string' && value.trim().length > 0;
}

export function joinUrl(baseUrl, pathName) {
  return new URL(pathName, baseUrl.endsWith('/') ? baseUrl : `${baseUrl}/`).toString();
}

export function replaceTemplate(template, values) {
  return String(template ?? '').replace(/\{([a-zA-Z0-9_]+)}/g, (_, key) => {
    const value = values[key];
    return value == null ? '' : String(value);
  });
}

export async function pathExists(targetPath) {
  try {
    await fs.access(targetPath);
    return true;
  } catch {
    return false;
  }
}

export function tokenizeCommandLine(input) {
  const tokens = [];
  const text = String(input ?? '').trim();
  const regex = /"((?:\\.|[^"\\])*)"|'((?:\\.|[^'\\])*)'|(\S+)/g;
  let match;
  while ((match = regex.exec(text)) !== null) {
    const token = match[1] ?? match[2] ?? match[3] ?? '';
    tokens.push(token.replace(/\\(["'\\])/g, '$1'));
  }
  return tokens;
}

export function parseOptionTokens(tokens) {
  const flags = {};
  const positionals = [];

  for (let index = 0; index < tokens.length; index += 1) {
    const token = tokens[index];
    if (!token.startsWith('--')) {
      positionals.push(token);
      continue;
    }

    const option = token.slice(2);
    const equalIndex = option.indexOf('=');
    if (equalIndex >= 0) {
      flags[option.slice(0, equalIndex)] = option.slice(equalIndex + 1);
      continue;
    }

    const nextToken = tokens[index + 1];
    if (nextToken && !nextToken.startsWith('--')) {
      flags[option] = nextToken;
      index += 1;
    } else {
      flags[option] = true;
    }
  }

  return { positionals, flags };
}

export function toBoolean(value, fallback = false) {
  if (typeof value === 'boolean') {
    return value;
  }
  if (value == null || value === '') {
    return fallback;
  }
  const normalized = String(value).trim().toLowerCase();
  if (['1', 'true', 'yes', 'on', 'y'].includes(normalized)) {
    return true;
  }
  if (['0', 'false', 'no', 'off', 'n'].includes(normalized)) {
    return false;
  }
  return fallback;
}

export function toInteger(value, fallback) {
  const numeric = Number(value);
  if (!Number.isFinite(numeric)) {
    return fallback;
  }
  return Math.trunc(numeric);
}
