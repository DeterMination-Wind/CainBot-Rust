import fs from 'node:fs/promises';
import http from 'node:http';
import path from 'node:path';

import {
  countMessageSegments,
  extractImageSegments,
  normalizeMessageSegments,
  plainTextFromMessage
} from './utils.mjs';

function normalizeHost(host) {
  const value = String(host ?? '').trim();
  return value || '127.0.0.1';
}

function normalizeToken(value) {
  return String(value ?? '').trim();
}

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
    return null;
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

function normalizeStringArray(value) {
  const source = Array.isArray(value) ? value : [value];
  return Array.from(new Set(
    source
      .map((item) => String(item ?? '').trim())
      .filter(Boolean)
  ));
}

function decodeOutgoingText(value) {
  return String(value ?? '')
    .replace(/\\r\\n/g, '\n')
    .replace(/\\n/g, '\n')
    .trim();
}

function buildOutgoingMessage(payload = {}) {
  const replyToMessageId = String(
    payload?.replyToMessageId
      ?? payload?.replyTo
      ?? payload?.reply_to_message_id
      ?? ''
  ).trim();
  const text = decodeOutgoingText(payload?.text ?? payload?.message ?? '');
  const atUserIds = normalizeStringArray(
    payload?.atUserIds
      ?? payload?.at_users
      ?? payload?.mentions
      ?? payload?.atUserId
  );

  const segments = [];
  if (replyToMessageId) {
    segments.push({
      type: 'reply',
      data: { id: replyToMessageId }
    });
  }

  atUserIds.forEach((userId, index) => {
    segments.push({
      type: 'at',
      data: { qq: userId }
    });
    if (index < atUserIds.length - 1 || text) {
      segments.push({
        type: 'text',
        data: { text: ' ' }
      });
    }
  });

  if (text) {
    segments.push({
      type: 'text',
      data: { text }
    });
  }

  if (segments.length === 0) {
    throw new Error('消息内容不能为空；请提供 text/message，或提供 atUserIds');
  }
  if (segments.length === 1 && segments[0]?.type === 'text') {
    return segments[0].data.text;
  }
  return segments;
}

function normalizeMessageRecord(message, index) {
  const segments = normalizeMessageSegments(message?.message);
  const atUserIds = Array.from(new Set(
    segments
      .filter((segment) => segment?.type === 'at')
      .map((segment) => String(segment?.data?.qq ?? '').trim())
      .filter(Boolean)
  ));

  return {
    index,
    time: formatEpochTime(message?.time),
    userId: String(message?.user_id ?? message?.sender?.user_id ?? '').trim(),
    sender: String(
      message?.sender?.card
      || message?.sender?.nickname
      || message?.sender?.nick
      || message?.sender?.user_name
      || message?.user_id
      || ''
    ).trim(),
    messageId: String(message?.message_id ?? '').trim(),
    messageSeq: String(message?.message_seq ?? '').trim(),
    realSeq: String(message?.real_seq ?? '').trim(),
    text: plainTextFromMessage(message?.message, message?.raw_message),
    summary: buildMessageSummary(message),
    atUserIds,
    imageCount: extractImageSegments(message?.message).length,
    fileCount: countMessageSegments(message?.message, 'file'),
    faceCount: countMessageSegments(message?.message, 'face'),
    replyCount: countMessageSegments(message?.message, 'reply')
  };
}

function normalizeHistoryMessages(payload) {
  if (Array.isArray(payload)) {
    return payload.filter(Boolean);
  }
  if (Array.isArray(payload?.messages)) {
    return payload.messages.filter(Boolean);
  }
  if (Array.isArray(payload?.data?.messages)) {
    return payload.data.messages.filter(Boolean);
  }
  return [];
}

function resolveReadableFilePath(inputPath) {
  const requested = String(inputPath ?? '').trim();
  if (!requested) {
    throw new Error('path 不能为空');
  }
  return path.isAbsolute(requested)
    ? path.resolve(requested)
    : path.resolve(process.cwd(), requested);
}

async function readTextFileWindow(filePath, options = {}) {
  const absolutePath = resolveReadableFilePath(filePath);
  const content = await fs.readFile(absolutePath, 'utf8');
  const lines = content.split(/\r?\n/);
  const totalLines = lines.length;
  const startLine = clampInteger(options?.startLine, 1, 1, Math.max(totalLines, 1));
  const defaultEndLine = Math.min(totalLines, startLine + 199);
  const endLine = clampInteger(options?.endLine, defaultEndLine, startLine, Math.max(totalLines, startLine));
  const maxChars = clampInteger(options?.maxChars, 12000, 200, 200000);

  const selectedLines = lines.slice(startLine - 1, endLine);
  let text = selectedLines
    .map((line, index) => `${startLine + index}: ${line}`)
    .join('\n');
  let truncated = false;
  if (text.length > maxChars) {
    text = `${text.slice(0, maxChars)}\n...[truncated]`;
    truncated = true;
  }

  return {
    path: absolutePath,
    totalLines,
    startLine,
    endLine,
    truncated,
    text
  };
}

function readRequestBody(req, maxBytes = 64 * 1024) {
  return new Promise((resolve, reject) => {
    const chunks = [];
    let total = 0;

    req.on('data', (chunk) => {
      total += chunk.length;
      if (total > maxBytes) {
        reject(new Error(`请求体过大，超过限制 ${maxBytes} bytes`));
        req.destroy();
        return;
      }
      chunks.push(chunk);
    });
    req.on('end', () => {
      resolve(Buffer.concat(chunks).toString('utf8'));
    });
    req.on('error', reject);
  });
}

function sendJson(res, statusCode, payload) {
  res.writeHead(statusCode, {
    'Content-Type': 'application/json; charset=utf-8',
    'Cache-Control': 'no-store'
  });
  res.end(JSON.stringify(payload, null, 2));
}

function parseBearerToken(headerValue) {
  const match = String(headerValue ?? '').match(/^Bearer\s+(.+)$/i);
  return match ? String(match[1]).trim() : '';
}

function getRemoteAddress(req) {
  return String(req?.socket?.remoteAddress ?? '').trim();
}

function isLoopbackAddress(address) {
  return ['127.0.0.1', '::1', '::ffff:127.0.0.1'].includes(String(address ?? '').trim());
}

export class CodexBridgeServer {
  constructor(config, napcatClient, logger) {
    this.config = config ?? {};
    this.napcatClient = napcatClient;
    this.logger = logger;
    this.server = null;
  }

  async start() {
    if (this.config.enabled === false) {
      return null;
    }
    if (this.server) {
      return this.getInfo();
    }

    this.server = http.createServer(async (req, res) => {
      await this.#handleRequest(req, res);
    });

    await new Promise((resolve, reject) => {
      const onError = (error) => {
        this.server?.off('listening', onListening);
        reject(error);
      };
      const onListening = () => {
        this.server?.off('error', onError);
        resolve();
      };
      this.server.once('error', onError);
      this.server.once('listening', onListening);
      this.server.listen(Number(this.config.port ?? 3186), normalizeHost(this.config.host));
    });

    const info = this.getInfo();
    this.logger.info(`Codex 文件桥已启动：${info.baseUrl}`);
    return info;
  }

  async stop() {
    if (!this.server) {
      return;
    }
    const server = this.server;
    this.server = null;
    await new Promise((resolve, reject) => {
      server.close((error) => {
        if (error) {
          reject(error);
          return;
        }
        resolve();
      });
    });
  }

  getInfo() {
    const host = normalizeHost(this.config.host);
    const port = Number(this.config.port ?? 3186) || 3186;
    const baseUrl = `http://${host}:${port}`;
    return {
      enabled: this.config.enabled !== false,
      host,
      port,
      baseUrl,
      sendGroupFileUrl: `${baseUrl}/codex/send-group-file`,
      sendGroupFileToFolderUrl: `${baseUrl}/codex/send-group-file-to-folder`,
      sendGroupMessageUrl: `${baseUrl}/codex/send-group-message`,
      sendPrivateMessageUrl: `${baseUrl}/codex/send-private-message`,
      readGroupMessagesUrl: `${baseUrl}/codex/read-group-messages`,
      readPrivateMessagesUrl: `${baseUrl}/codex/read-private-messages`,
      readFileUrl: `${baseUrl}/codex/read-file`,
      healthUrl: `${baseUrl}/codex/health`,
      tokenRequired: normalizeToken(this.config.token).length > 0
    };
  }

  async #handleRequest(req, res) {
    try {
      const method = String(req.method ?? 'GET').toUpperCase();
      const url = new URL(req.url ?? '/', 'http://127.0.0.1');

      if (method === 'GET' && url.pathname === '/codex/health') {
        return sendJson(res, 200, {
          ok: true,
          service: 'cain-codex-bridge',
          ...this.getInfo()
        });
      }

      if (method === 'POST' && url.pathname === '/codex/send-group-file') {
        return await this.#handleSendGroupFile(req, res);
      }
      if (method === 'POST' && url.pathname === '/codex/send-group-file-to-folder') {
        return await this.#handleSendGroupFileToFolder(req, res);
      }
      if (method === 'POST' && url.pathname === '/codex/send-group-message') {
        return await this.#handleSendGroupMessage(req, res);
      }
      if (method === 'POST' && url.pathname === '/codex/send-private-message') {
        return await this.#handleSendPrivateMessage(req, res);
      }
      if (method === 'POST' && url.pathname === '/codex/read-group-messages') {
        return await this.#handleReadGroupMessages(req, res);
      }
      if (method === 'POST' && url.pathname === '/codex/read-private-messages') {
        return await this.#handleReadPrivateMessages(req, res);
      }
      if (method === 'POST' && url.pathname === '/codex/read-file') {
        return await this.#handleReadFile(req, res);
      }

      return sendJson(res, 404, {
        ok: false,
        error: '未找到接口'
      });
    } catch (error) {
      this.logger.warn(`Codex 文件桥请求处理失败：${error.message}`);
      return sendJson(res, 500, {
        ok: false,
        error: error.message
      });
    }
  }

  async #validateLocalRequest(req, res) {
    const remoteAddress = getRemoteAddress(req);
    if (!isLoopbackAddress(remoteAddress)) {
      sendJson(res, 403, {
        ok: false,
        error: `仅允许本机访问，当前来源：${remoteAddress || '(unknown)'}`
      });
      return false;
    }

    const requiredToken = normalizeToken(this.config.token);
    if (!requiredToken) {
      return true;
    }

    const providedToken = parseBearerToken(req.headers.authorization);
    if (!providedToken || providedToken !== requiredToken) {
      sendJson(res, 401, {
        ok: false,
        error: 'Bearer Token 无效'
      });
      return false;
    }
    return true;
  }

  async #readJsonPayload(req, res) {
    try {
      const body = await readRequestBody(req, 64 * 1024);
      return body ? JSON.parse(body) : {};
    } catch (error) {
      sendJson(res, 400, {
        ok: false,
        error: `请求体不是有效 JSON：${error.message}`
      });
      return null;
    }
  }

  async #handleSendGroupFile(req, res) {
    if (!await this.#validateLocalRequest(req, res)) {
      return;
    }

    const payload = await this.#readJsonPayload(req, res);
    if (!payload) {
      return;
    }

    const groupId = String(payload?.groupId ?? '').trim();
    const filePath = String(payload?.filePath ?? '').trim();
    const fileName = String(payload?.fileName ?? '').trim();
    const folderName = String(payload?.folderName ?? payload?.folder ?? payload?.folderPath ?? payload?.directory ?? '').trim();
    const notifyText = String(payload?.notifyText ?? '').trim();

    if (!groupId) {
      return sendJson(res, 400, { ok: false, error: 'groupId 不能为空' });
    }
    if (!filePath) {
      return sendJson(res, 400, { ok: false, error: 'filePath 不能为空' });
    }

    const result = await this.napcatClient.sendLocalFileToGroup({
      groupId,
      filePath,
      fileName,
      folderName,
      notifyText
    });

    return sendJson(res, 200, {
      ok: true,
      ...result
    });
  }

  async #handleSendGroupFileToFolder(req, res) {
    if (!await this.#validateLocalRequest(req, res)) {
      return;
    }

    const payload = await this.#readJsonPayload(req, res);
    if (!payload) {
      return;
    }

    const groupId = String(payload?.groupId ?? '').trim();
    const filePath = String(payload?.filePath ?? '').trim();
    const fileName = String(payload?.fileName ?? '').trim();
    const folderName = String(payload?.folderName ?? payload?.folder ?? payload?.folderPath ?? payload?.directory ?? '').trim();
    const notifyText = String(payload?.notifyText ?? '').trim();

    if (!groupId) {
      return sendJson(res, 400, { ok: false, error: 'groupId 不能为空' });
    }
    if (!filePath) {
      return sendJson(res, 400, { ok: false, error: 'filePath 不能为空' });
    }
    if (!folderName) {
      return sendJson(res, 400, { ok: false, error: 'folderName/folderPath 不能为空' });
    }

    const result = await this.napcatClient.sendLocalFileToGroup({
      groupId,
      filePath,
      fileName,
      folderName,
      notifyText
    });

    return sendJson(res, 200, {
      ok: true,
      ...result
    });
  }

  async #handleSendGroupMessage(req, res) {
    if (!await this.#validateLocalRequest(req, res)) {
      return;
    }

    const payload = await this.#readJsonPayload(req, res);
    if (!payload) {
      return;
    }

    const groupId = String(payload?.groupId ?? '').trim();
    if (!groupId) {
      return sendJson(res, 400, { ok: false, error: 'groupId 不能为空' });
    }

    const message = buildOutgoingMessage(payload);
    const result = await this.napcatClient.sendGroupMessage(groupId, message);
    return sendJson(res, 200, {
      ok: true,
      target: `group:${groupId}`,
      atUserIds: normalizeStringArray(payload?.atUserIds ?? payload?.at_users ?? payload?.mentions ?? payload?.atUserId),
      replyToMessageId: String(payload?.replyToMessageId ?? payload?.replyTo ?? payload?.reply_to_message_id ?? '').trim() || null,
      result
    });
  }

  async #handleSendPrivateMessage(req, res) {
    if (!await this.#validateLocalRequest(req, res)) {
      return;
    }

    const payload = await this.#readJsonPayload(req, res);
    if (!payload) {
      return;
    }

    const userId = String(payload?.userId ?? '').trim();
    if (!userId) {
      return sendJson(res, 400, { ok: false, error: 'userId 不能为空' });
    }

    const message = buildOutgoingMessage(payload);
    const result = await this.napcatClient.sendPrivateMessage(userId, message);
    return sendJson(res, 200, {
      ok: true,
      target: `private:${userId}`,
      replyToMessageId: String(payload?.replyToMessageId ?? payload?.replyTo ?? payload?.reply_to_message_id ?? '').trim() || null,
      result
    });
  }

  async #handleReadGroupMessages(req, res) {
    if (!await this.#validateLocalRequest(req, res)) {
      return;
    }

    const payload = await this.#readJsonPayload(req, res);
    if (!payload) {
      return;
    }

    const groupId = String(payload?.groupId ?? '').trim();
    if (!groupId) {
      return sendJson(res, 400, { ok: false, error: 'groupId 不能为空' });
    }

    const count = clampInteger(payload?.count, 20, 1, 200);
    const history = await this.napcatClient.getGroupMessageHistory(groupId, {
      count,
      messageSeq: payload?.messageSeq ?? payload?.message_seq,
      reverseOrder: Boolean(payload?.reverseOrder ?? payload?.reverse_order ?? false),
      disableGetUrl: Boolean(payload?.disableGetUrl ?? payload?.disable_get_url ?? true),
      parseMultMsg: Boolean(payload?.parseMultMsg ?? payload?.parse_mult_msg ?? false),
      quickReply: Boolean(payload?.quickReply ?? payload?.quick_reply ?? false)
    });
    const messages = normalizeHistoryMessages(history)
      .sort((left, right) => {
        const seqDiff = messageOrderValue(left) - messageOrderValue(right);
        if (seqDiff !== 0) {
          return seqDiff;
        }
        return toFiniteNumber(left?.message_id, 0) - toFiniteNumber(right?.message_id, 0);
      })
      .slice(-count);

    return sendJson(res, 200, {
      ok: true,
      target: `group:${groupId}`,
      requestedCount: count,
      returnedCount: messages.length,
      messages: messages.map((message, index) => normalizeMessageRecord(message, index + 1))
    });
  }

  async #handleReadPrivateMessages(req, res) {
    if (!await this.#validateLocalRequest(req, res)) {
      return;
    }

    const payload = await this.#readJsonPayload(req, res);
    if (!payload) {
      return;
    }

    const userId = String(payload?.userId ?? '').trim();
    if (!userId) {
      return sendJson(res, 400, { ok: false, error: 'userId 不能为空' });
    }

    const count = clampInteger(payload?.count, 20, 1, 200);
    const history = await this.napcatClient.getFriendMessageHistory(userId, {
      count,
      messageSeq: payload?.messageSeq ?? payload?.message_seq,
      reverseOrder: Boolean(payload?.reverseOrder ?? payload?.reverse_order ?? false),
      disableGetUrl: Boolean(payload?.disableGetUrl ?? payload?.disable_get_url ?? true),
      parseMultMsg: Boolean(payload?.parseMultMsg ?? payload?.parse_mult_msg ?? false),
      quickReply: Boolean(payload?.quickReply ?? payload?.quick_reply ?? false)
    });
    const messages = normalizeHistoryMessages(history)
      .sort((left, right) => {
        const seqDiff = messageOrderValue(left) - messageOrderValue(right);
        if (seqDiff !== 0) {
          return seqDiff;
        }
        return toFiniteNumber(left?.message_id, 0) - toFiniteNumber(right?.message_id, 0);
      })
      .slice(-count);

    return sendJson(res, 200, {
      ok: true,
      target: `private:${userId}`,
      requestedCount: count,
      returnedCount: messages.length,
      messages: messages.map((message, index) => normalizeMessageRecord(message, index + 1))
    });
  }

  async #handleReadFile(req, res) {
    if (!await this.#validateLocalRequest(req, res)) {
      return;
    }

    const payload = await this.#readJsonPayload(req, res);
    if (!payload) {
      return;
    }

    const filePath = String(payload?.path ?? payload?.filePath ?? '').trim();
    if (!filePath) {
      return sendJson(res, 400, { ok: false, error: 'path 不能为空' });
    }

    const result = await readTextFileWindow(filePath, {
      startLine: payload?.startLine ?? payload?.start_line,
      endLine: payload?.endLine ?? payload?.end_line,
      maxChars: payload?.maxChars ?? payload?.max_chars
    });

    return sendJson(res, 200, {
      ok: true,
      ...result
    });
  }
}
