import fs from 'node:fs/promises';
import path from 'node:path';
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';

import { extractFileSegments } from './message-attachment-reader.mjs';
import { isNonEmptyString, pathExists, plainTextFromMessage } from './utils.mjs';

const execFileAsync = promisify(execFile);
const MAX_DOWNLOAD_BYTES = 32 * 1024 * 1024;
const DEFAULT_PROGRESS_TEXT = '检测到.msav文件，正在生成简要介绍...';

function truncateText(text, maxChars) {
  const normalized = String(text ?? '').trim();
  if (normalized.length <= maxChars) {
    return normalized;
  }
  return `${normalized.slice(0, maxChars)}...(已截断)`;
}

function pickMessageId(results = []) {
  for (const item of results) {
    const messageId = String(item?.message_id ?? item?.messageId ?? '').trim();
    if (messageId) {
      return messageId;
    }
  }
  return '';
}

function isMsavName(fileName) {
  return /\.msav$/i.test(String(fileName ?? '').trim());
}

function sanitizeBaseName(fileName) {
  const normalized = String(fileName ?? 'map.msav').trim() || 'map.msav';
  const safe = normalized.replace(/[\\/:*?"<>|]+/g, '_');
  return isMsavName(safe) ? safe : `${safe}.msav`;
}

function summarizeTags(tags = {}) {
  return Object.entries(tags)
    .filter(([key]) => !['name', 'author', 'description', 'rules'].includes(key))
    .filter(([, value]) => isNonEmptyString(value))
    .slice(0, 12)
    .map(([key, value]) => `${key}=${truncateText(value, 120)}`)
    .join('；');
}

export class MsavMapAnalyzer {
  constructor(options = {}) {
    this.projectRoot = path.resolve(String(options.projectRoot ?? process.cwd()));
    this.cacheDir = path.resolve(String(options.cacheDir ?? path.join(this.projectRoot, 'data', 'msav-parser')));
    this.javaSourcePath = path.join(this.projectRoot, 'scripts', 'CainMsavMetadata.java');
    this.chatClient = options.chatClient;
    this.napcatClient = options.napcatClient;
    this.logger = options.logger;
    this.webUiSyncStore = options.webUiSyncStore ?? null;
    this.model = String(options.model ?? 'gpt-5.2').trim() || 'gpt-5.2';
    this.progressNoticeText = String(options.progressNoticeText ?? DEFAULT_PROGRESS_TEXT).trim() || DEFAULT_PROGRESS_TEXT;
    this.replyContextTtlMs = Math.max(60_000, Number(options.replyContextTtlMs ?? 24 * 60 * 60 * 1000) || 24 * 60 * 60 * 1000);
    this.maxReplyContexts = Math.max(20, Number(options.maxReplyContexts ?? 200) || 200);
    this.cachedJarPath = '';
    this.replyContexts = new Map();
  }

  getReplyContext(messageId) {
    const key = String(messageId ?? '').trim();
    if (!key) {
      return null;
    }
    this.#cleanupReplyContexts();
    return this.replyContexts.get(key) ?? null;
  }

  async maybeAnalyzeIncomingMsav(context, event) {
    const candidate = await this.#findIncomingMsavCandidate(event?.message);
    if (!candidate) {
      return null;
    }

    const taskId = this.#buildTaskId(event, candidate.fileName);
    const taskBase = {
      id: taskId,
      fileName: candidate.fileName,
      sourceMessageId: String(event?.message_id ?? '').trim(),
      messageType: String(context?.messageType ?? '').trim(),
      groupId: String(context?.groupId ?? '').trim(),
      userId: String(context?.userId ?? '').trim()
    };

    let noticeMessageId = '';
    try {
      await this.#updateTask({
        ...taskBase,
        status: 'running',
        stage: 'detected',
        message: this.progressNoticeText
      });

      const noticeResults = await this.napcatClient.replyText(context, event?.message_id, this.progressNoticeText);
      noticeMessageId = pickMessageId(noticeResults);

      await this.#updateTask({
        ...taskBase,
        noticeMessageId,
        status: 'running',
        stage: 'preparing',
        message: '正在读取并准备地图文件...'
      });
      const resolved = await this.#materializeMsav(candidate);

      await this.#updateTask({
        ...taskBase,
        noticeMessageId,
        status: 'running',
        stage: 'parsing',
        message: '正在解析地图元数据...'
      });
      const metadata = await this.#parseMetadata(resolved.filePath);
      const metadataText = this.#buildMetadataText(metadata, resolved.fileName);
      const userText = plainTextFromMessage(event?.message, event?.raw_message);

      await this.#updateTask({
        ...taskBase,
        noticeMessageId,
        status: 'running',
        stage: 'generating',
        message: '正在生成地图简要介绍...'
      });
      const introduction = await this.#generateIntroduction(metadataText, userText);
      const sentResults = await this.napcatClient.replyText(context, event?.message_id, introduction);
      const replyMessageId = pickMessageId(sentResults);

      if (replyMessageId) {
        this.replyContexts.set(replyMessageId, {
          createdAt: Date.now(),
          fileName: resolved.fileName,
          metadataText,
          introduction,
          sourceMessageId: String(event?.message_id ?? '').trim(),
          messageType: String(context?.messageType ?? '').trim(),
          groupId: String(context?.groupId ?? '').trim(),
          userId: String(context?.userId ?? '').trim()
        });
        this.#cleanupReplyContexts();
      }

      await this.#finishTask(taskId, {
        ...taskBase,
        noticeMessageId,
        replyMessageId,
        status: 'success',
        stage: 'done',
        message: '地图简要介绍已生成。',
        resultPreview: truncateText(introduction, 180)
      });

      return {
        handled: true,
        noticeMessageId,
        replyMessageId,
        metadataText,
        introduction
      };
    } catch (error) {
      await this.#finishTask(taskId, {
        ...taskBase,
        noticeMessageId,
        status: 'error',
        stage: 'error',
        message: '地图解析失败。',
        error: String(error?.message ?? error)
      });
      throw error;
    }
  }

  async #findIncomingMsavCandidate(message) {
    for (const segment of extractFileSegments(message)) {
      const fileId = String(segment?.file_id ?? '').trim();
      if (!fileId) {
        continue;
      }
      const fileInfo = await this.napcatClient.getFile(fileId);
      const fileName = String(fileInfo?.file_name ?? segment?.name ?? segment?.file ?? '').trim();
      if (isMsavName(fileName)) {
        return {
          fileId,
          fileName,
          fileSegment: segment,
          fileInfo
        };
      }
    }
    return null;
  }

  async #materializeMsav(candidate) {
    const fileName = String(candidate?.fileName ?? '').trim();
    const fileInfo = candidate?.fileInfo ?? {};
    const inputDir = path.join(this.cacheDir, 'inputs');
    await fs.mkdir(inputDir, { recursive: true });

    if (typeof fileInfo?.base64 === 'string' && fileInfo.base64.length > 0) {
      const targetPath = path.join(inputDir, `${Date.now()}-${sanitizeBaseName(fileName)}`);
      await fs.writeFile(targetPath, Buffer.from(fileInfo.base64, 'base64'));
      return { fileName, filePath: targetPath };
    }

    const directPath = String(fileInfo?.file ?? '').trim();
    if (directPath && path.isAbsolute(directPath) && await pathExists(directPath)) {
      return { fileName, filePath: directPath };
    }

    const url = String(fileInfo?.url ?? '').trim();
    if (!url) {
      throw new Error(`NapCat 没有返回 .msav 文件的可读路径：${fileName}`);
    }

    const response = await fetch(url, {
      method: 'GET',
      headers: { Accept: 'application/octet-stream' }
    });
    if (!response.ok) {
      throw new Error(`下载 .msav 文件失败：HTTP ${response.status}`);
    }
    const arrayBuffer = await response.arrayBuffer();
    const buffer = Buffer.from(arrayBuffer);
    if (buffer.length > MAX_DOWNLOAD_BYTES) {
      throw new Error(`.msav 文件过大：${buffer.length} bytes`);
    }
    const targetPath = path.join(inputDir, `${Date.now()}-${sanitizeBaseName(fileName)}`);
    await fs.writeFile(targetPath, buffer);
    return { fileName, filePath: targetPath };
  }

  async #parseMetadata(filePath) {
    const jarPath = await this.#findMindustryJar();
    const classDir = await this.#ensureParserCompiled(jarPath);
    const classPath = [classDir, jarPath].join(path.delimiter);
    const { stdout, stderr } = await execFileAsync('java', ['-cp', classPath, 'CainMsavMetadata', filePath], {
      windowsHide: true,
      timeout: 60000,
      maxBuffer: 1024 * 1024
    });
    const output = String(stdout ?? '').trim();
    if (!output) {
      throw new Error(`地图解析器没有输出结果${stderr ? `：${String(stderr).trim()}` : ''}`);
    }
    try {
      return JSON.parse(output);
    } catch (error) {
      throw new Error(`地图解析器输出不是有效 JSON：${error.message}`);
    }
  }

  async #ensureParserCompiled(jarPath) {
    const outputDir = path.join(this.cacheDir, 'classes');
    await fs.mkdir(outputDir, { recursive: true });
    const classFile = path.join(outputDir, 'CainMsavMetadata.class');

    const [sourceStat, classStat] = await Promise.all([
      fs.stat(this.javaSourcePath),
      fs.stat(classFile).catch(() => null)
    ]);
    if (classStat && classStat.mtimeMs >= sourceStat.mtimeMs) {
      return outputDir;
    }

    await execFileAsync('javac', [
      '-encoding', 'UTF-8',
      '-cp', jarPath,
      '-d', outputDir,
      this.javaSourcePath
    ], {
      windowsHide: true,
      timeout: 60000,
      maxBuffer: 1024 * 1024
    });
    return outputDir;
  }

  async #findMindustryJar() {
    if (this.cachedJarPath && await pathExists(this.cachedJarPath)) {
      return this.cachedJarPath;
    }

    const envPath = String(process.env.CAINBOT_MINDUSTRY_JAR ?? '').trim();
    if (envPath && await pathExists(envPath)) {
      this.cachedJarPath = path.resolve(envPath);
      return this.cachedJarPath;
    }

    const downloadsDir = path.join(process.env.USERPROFILE || path.dirname(this.projectRoot), 'Downloads');
    const entries = await fs.readdir(downloadsDir, { withFileTypes: true });
    const candidates = [];
    for (const entry of entries) {
      if (!entry.isFile()) {
        continue;
      }
      const name = entry.name;
      if (!/^(MindustryX-.*Desktop\.jar|Mindustry\.jar)$/i.test(name)) {
        continue;
      }
      const fullPath = path.join(downloadsDir, name);
      const stat = await fs.stat(fullPath);
      candidates.push({
        fullPath,
        name,
        score: /Desktop\.jar$/i.test(name) ? 2 : 1,
        mtimeMs: stat.mtimeMs
      });
    }

    candidates.sort((left, right) => (right.score - left.score) || (right.mtimeMs - left.mtimeMs));
    if (candidates.length === 0) {
      throw new Error('未找到本地 Mindustry 桌面版 jar。请把 Mindustry 桌面包放到 Downloads，或设置环境变量 CAINBOT_MINDUSTRY_JAR');
    }

    this.cachedJarPath = candidates[0].fullPath;
    return this.cachedJarPath;
  }

  #buildMetadataText(metadata, fileName) {
    const safeMetadata = metadata ?? {};
    const rulesJson = truncateText(String(safeMetadata.rules_json ?? ''), 2600);
    const tagSummary = summarizeTags(safeMetadata.tags ?? {});
    return [
      `文件名：${fileName || safeMetadata.file_name || '未知.msav'}`,
      `地图名：${safeMetadata.name || '未命名'}`,
      `尺寸：${safeMetadata.width || '?'}x${safeMetadata.height || '?'}`,
      safeMetadata.author ? `作者：${safeMetadata.author}` : '',
      safeMetadata.description ? `描述：${truncateText(safeMetadata.description, 280)}` : '',
      safeMetadata.suggested_mode ? `推测模式：${safeMetadata.suggested_mode}` : '',
      rulesJson ? `规则：${rulesJson}` : '',
      tagSummary ? `其他标签：${tagSummary}` : ''
    ].filter(Boolean).join('\n');
  }

  async #generateIntroduction(metadataText, userText) {
    const userParts = [
      '下面是本地解析器提取出的 Mindustry 地图元数据。',
      metadataText
    ];
    if (isNonEmptyString(userText)) {
      userParts.push(`发送者附带文本：${truncateText(userText, 400)}`);
    }

    return await this.chatClient.complete([
      {
        role: 'system',
        content: [
          '你是一个会看 Mindustry 地图元数据的助手。',
          '请基于给到的元数据，用中文简短介绍这张地图。',
          '优先说清楚地图规模、可能玩法、模式倾向、作者/描述透露的主题，以及值得注意的点。',
          '如果某些结论只是推测，要明确说是根据元数据推测。',
          '输出尽量精简，一段式，不要使用 Markdown。'
        ].join('\n')
      },
      {
        role: 'user',
        content: userParts.join('\n\n')
      }
    ], {
      model: this.model,
      temperature: 0.5
    });
  }

  async #updateTask(task) {
    if (!this.webUiSyncStore) {
      return;
    }
    try {
      await this.webUiSyncStore.upsertMsavTask(task);
    } catch (error) {
      this.logger?.warn?.(`写入 .msav WebUI 进度失败：${error.message}`);
    }
  }

  async #finishTask(taskId, updates) {
    if (!this.webUiSyncStore) {
      return;
    }
    try {
      await this.webUiSyncStore.markMsavTaskFinished(taskId, updates);
    } catch (error) {
      this.logger?.warn?.(`结束 .msav WebUI 进度失败：${error.message}`);
    }
  }

  #buildTaskId(event, fileName) {
    const messageId = String(event?.message_id ?? '').trim();
    const normalizedName = sanitizeBaseName(fileName).replace(/[^a-zA-Z0-9._-]+/g, '_');
    return messageId ? `msav-${messageId}-${normalizedName}` : `msav-${Date.now()}-${normalizedName}`;
  }

  #cleanupReplyContexts() {
    const now = Date.now();
    for (const [key, value] of this.replyContexts.entries()) {
      if ((now - Number(value?.createdAt ?? 0)) > this.replyContextTtlMs) {
        this.replyContexts.delete(key);
      }
    }
    if (this.replyContexts.size <= this.maxReplyContexts) {
      return;
    }
    const entries = Array.from(this.replyContexts.entries())
      .sort((left, right) => Number(left[1]?.createdAt ?? 0) - Number(right[1]?.createdAt ?? 0));
    while (entries.length > this.maxReplyContexts) {
      const removed = entries.shift();
      if (removed) {
        this.replyContexts.delete(removed[0]);
      }
    }
  }
}


