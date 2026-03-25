import path from 'node:path';
import tls from 'node:tls';
import fs from 'node:fs/promises';
import { fileURLToPath } from 'node:url';

import { loadConfig } from './config.mjs';
import { ChatSessionManager } from './chat-session-manager.mjs';
import { CodexReadonlyTools } from './codex-readonly-tools.mjs';
import { CodexBridgeServer } from './codex-bridge-server.mjs';
import { GroupFileDownloadManager } from './group-file-download-manager.mjs';
import { LocalRagRetriever } from './local-rag-retriever.mjs';
import { Logger } from './logger.mjs';
import { NapCatClient } from './napcat-client.mjs';
import { MsavMapAnalyzer } from './msav-map-analyzer.mjs';
import { OpenAiChatClient } from './openai-chat-client.mjs';
import { OpenAiTranslator } from './openai-translator.mjs';
import { RuntimeConfigStore } from './runtime-config-store.mjs';
import { StateStore } from './state-store.mjs';
import { WebUiSyncStore } from './webui-sync-store.mjs';
import { readTextFilesFromMessage } from './message-attachment-reader.mjs';
import {
  escapeRegExp,
  extractImageSegments,
  extractReplyId,
  imageSegmentToOpenAiContent,
  isNonEmptyString,
  normalizeMessageSegments,
  pathExists,
  plainTextFromMessage,
  tokenizeCommandLine,
  parseOptionTokens
} from './utils.mjs';

const __filename = fileURLToPath(import.meta.url);
const srcDir = path.dirname(__filename);
const projectRoot = path.resolve(srcDir, '..');
const GROUP_CARD_SYNC_RETRY_MS = 10 * 60 * 1000;
const SHUTDOWN_VOTE_REQUIRED_COUNT = 3;
const SHUTDOWN_VOTE_TTL_MS = 30 * 60 * 1000;
const SHUTDOWN_VOTE_FILTER_MODEL = 'gpt-5.4-mini';
const SHUTDOWN_VOTE_PROMPT = '确定要关闭此bot的功能吗，大于两个人回复本消息"Y"将确认此操作';
const OWNER_LOG_MAX_CHARS = 1500;

const E_SUBCOMMANDS = new Set(['过滤', '聊天', '状态', '启用', '禁用', '文件下载']);
let fatalLogger = null;

function trimOwnerLogText(text, maxChars = OWNER_LOG_MAX_CHARS) {
  const normalized = String(text ?? '').trim();
  if (!normalized || normalized.length <= maxChars) {
    return normalized;
  }
  return `${normalized.slice(0, maxChars)}\n...(已截断)`;
}

function normalizeLocalWebUiHost(host) {
  const normalized = String(host ?? '').trim();
  if (!normalized || normalized === '::' || normalized === '0.0.0.0') {
    return '127.0.0.1';
  }
  if (normalized.includes(':') && !normalized.startsWith('[') && !normalized.endsWith(']')) {
    return `[${normalized}]`;
  }
  return normalized;
}

async function buildWebUiDirectInfo(webUiConfigPath, pluginId = 'napcat-plugin-cain-monitor') {
  const normalizedPath = String(webUiConfigPath ?? '').trim();
  if (!normalizedPath) {
    return null;
  }
  try {
    const raw = JSON.parse(await fs.readFile(normalizedPath, 'utf8'));
    const port = Number(raw?.port ?? 6099) || 6099;
    const baseUrl = `http://${normalizeLocalWebUiHost(raw?.host)}:${port}`;
    const token = String(raw?.token ?? '').trim();
    const userPanelUrl = token
      ? `${baseUrl}/webui?token=${encodeURIComponent(token)}`
      : `${baseUrl}/webui`;
    return {
      configPath: normalizedPath,
      baseUrl,
      userPanelUrl,
      monitorPageUrl: `${baseUrl}/plugin/${pluginId}/page/dashboard`
    };
  } catch {
    return null;
  }
}

function enableSystemCaCertificates() {
  if (typeof tls.setDefaultCACertificates !== 'function') {
    return { enabled: false, reason: 'tls.setDefaultCACertificates 不可用' };
  }

  const bundledCertificates = tls.getCACertificates('bundled');
  const systemCertificates = tls.getCACertificates('system');
  if (!Array.isArray(systemCertificates) || systemCertificates.length === 0) {
    return { enabled: false, reason: '系统证书库为空' };
  }

  const mergedCertificates = [...new Set([...bundledCertificates, ...systemCertificates])];
  tls.setDefaultCACertificates(mergedCertificates);
  return {
    enabled: true,
    bundledCount: bundledCertificates.length,
    systemCount: systemCertificates.length,
    mergedCount: mergedCertificates.length
  };
}

function formatEpochTime(seconds) {
  const numeric = Number(seconds);
  if (!Number.isFinite(numeric) || numeric <= 0) {
    return '-';
  }
  return new Intl.DateTimeFormat('zh-CN', {
    timeZone: 'Asia/Shanghai',
    hour12: false,
    year: 'numeric',
    month: '2-digit',
    day: '2-digit',
    hour: '2-digit',
    minute: '2-digit',
    second: '2-digit'
  }).format(new Date(numeric * 1000));
}

function formatCurrentDateTime() {
  return new Intl.DateTimeFormat('zh-CN', {
    timeZone: 'Asia/Shanghai',
    hour12: false,
    year: 'numeric',
    month: '2-digit',
    day: '2-digit',
    hour: '2-digit',
    minute: '2-digit',
    second: '2-digit'
  }).format(new Date());
}

function describeModelFramework(baseUrl) {
  const normalizedBaseUrl = String(baseUrl ?? '').trim();
  if (!normalizedBaseUrl) {
    return 'OpenAI Chat Completions 兼容接口';
  }

  try {
    const parsed = new URL(normalizedBaseUrl);
    const host = parsed.host || normalizedBaseUrl;
    return `OpenAI Chat Completions 兼容接口 (${host})`;
  } catch {
    return `OpenAI Chat Completions 兼容接口 (${normalizedBaseUrl})`;
  }
}

function formatChatReplyForSend(config, text) {
  return String(text ?? '').trim();
}

function createContextFromEvent(event) {
  const messageType = String(event?.message_type ?? '').trim() === 'group' ? 'group' : 'private';
  return {
    messageType,
    groupId: messageType === 'group' ? String(event?.group_id ?? '').trim() : '',
    userId: String(event?.user_id ?? '').trim(),
    selfId: String(event?.self_id ?? '').trim()
  };
}

function getSenderName(event) {
  return String(
    event?.sender?.card
    || event?.sender?.nickname
    || event?.user_id
    || '用户'
  ).trim() || '用户';
}

function textLooksLikeDirectBotMention(text, displayName) {
  const normalizedText = String(text ?? '').trim();
  const normalizedDisplayName = String(displayName ?? '').trim();
  if (!normalizedText || !normalizedDisplayName) {
    return false;
  }

  const escapedName = escapeRegExp(normalizedDisplayName);
  return new RegExp(`(?:^|\\s|[>＞])@?${escapedName}(?:[\\s:：,，.。!！?？]|$)`, 'i').test(normalizedText);
}

function eventMentionsSelf(event, botDisplayName = 'Cain') {
  const selfId = String(event?.self_id ?? '').trim();
  if (selfId) {
    const hasAtSegment = normalizeMessageSegments(event?.message)
      .some((segment) => segment?.type === 'at' && String(segment?.data?.qq ?? '').trim() === selfId);
    if (hasAtSegment) {
      return true;
    }

    const rawMessage = String(event?.raw_message ?? '');
    if (new RegExp(`\\[CQ:at,qq=${escapeRegExp(selfId)}(?:,|\\])`, 'i').test(rawMessage)) {
      return true;
    }
  }

  return textLooksLikeDirectBotMention(
    plainTextFromMessage(event?.message, event?.raw_message),
    botDisplayName
  );
}

function eventMentionsOtherUser(event, botDisplayName = 'Cain') {
  const selfId = String(event?.self_id ?? '').trim();
  const atSegments = normalizeMessageSegments(event?.message)
    .filter((segment) => segment?.type === 'at')
    .map((segment) => String(segment?.data?.qq ?? '').trim())
    .filter(Boolean);
  if (atSegments.some((qq) => qq && qq !== selfId)) {
    return true;
  }

  const rawMessage = String(event?.raw_message ?? '');
  const rawAtMatches = Array.from(rawMessage.matchAll(/\[CQ:at,qq=([^,\]]+)/ig));
  if (rawAtMatches.some((match) => String(match?.[1] ?? '').trim() && String(match?.[1] ?? '').trim() !== selfId)) {
    return true;
  }

  const plainText = plainTextFromMessage(event?.message, rawMessage);
  const mentionMatches = Array.from(plainText.matchAll(/(?:^|[\s>＞])@([^\s@]{1,32})/g));
  if (mentionMatches.length === 0) {
    return false;
  }

  const normalizedBotName = String(botDisplayName ?? '').trim().toLowerCase();
  return mentionMatches.some((match) => {
    const mentionName = String(match?.[1] ?? '').trim().toLowerCase();
    return mentionName && mentionName !== normalizedBotName;
  });
}

function buildHelpText(config) {
  const displayName = String(config?.bot?.displayName ?? '[Bot]Cain').trim() || '[Bot]Cain';
  const lines = [
    'Cain 可用命令：',
    '/help',
    '/chat 文本',
    '/tr 文本',
    '#翻译 文本',
    '引用一条消息后发送 /chat',
    '引用一条消息后发送 /tr 或 #翻译',
    `群里直接 @${displayName} 也会按 /chat 处理。`,
    '发送 .msav 文件会自动解析；回复那条地图介绍消息会继续围绕同一张地图聊天。',
    '',
    '/e 状态',
    '/e 过滤 <要求>',
    '/e 聊天 <要求>',
    '/e 启用',
    '/e 禁用',
    '/e 文件下载 启用 [群文件夹名]',
    '/e 文件下载 关闭',
    '',
    '说明：',
    '- 只有指定群会启用普通群消息的“问题过滤 + 提示”流程。',
    '- /e 的过滤和聊天 prompt 仅当前群的群主、管理员或 bot 主人可修改。',
    '- /e 启用 与 /e 禁用 仅 bot 主人可用。',
    '- /e 文件下载 启用 [群文件夹名] 与 /e 文件下载 关闭 仅当前群群主、管理员或 bot 主人可用。'
  ];
  return lines.join('\n');
}

function parseIncomingCommand(text) {
  const trimmed = String(text ?? '').trim();
  if (!trimmed) {
    return null;
  }

  if (trimmed.startsWith('#')) {
    const match = trimmed.match(/^#(翻译)(?:\s+([\s\S]*))?$/);
    if (!match) {
      return null;
    }
    return {
      rawName: match[1],
      name: 'translate',
      rawArgs: String(match[2] ?? '').trim(),
      argument: String(match[2] ?? '').trim(),
      flags: {},
      positionals: [],
      prefix: '#'
    };
  }

  if (!trimmed.startsWith('/')) {
    return null;
  }

  const withoutPrefix = trimmed.slice(1).trimStart();
  if (!withoutPrefix) {
    return null;
  }
  const match = withoutPrefix.match(/^(\S+)(?:\s+([\s\S]*))?$/);
  if (!match) {
    return null;
  }

  const rawName = String(match[1] ?? '').trim();
  const rawArgs = String(match[2] ?? '').trim();
  const lowered = rawName.toLowerCase();
  const aliasMap = new Map([
    ['help', 'help'],
    ['chat', 'chat'],
    ['tr', 'translate'],
    ['e', 'edit']
  ]);
  const name = aliasMap.get(lowered);
  if (!name) {
    return null;
  }

  const tokenized = tokenizeCommandLine(rawArgs);
  const parsed = parseOptionTokens(tokenized);
  return {
    rawName,
    name,
    rawArgs,
    argument: rawArgs,
    flags: parsed.flags,
    positionals: parsed.positionals,
    prefix: '/'
  };
}

async function ensureConfigExists(configPath) {
  if (await pathExists(configPath)) {
    return;
  }
  const examplePath = path.join(projectRoot, 'config.example.json');
  throw new Error(`未找到配置文件：${configPath}\n请先复制 ${examplePath} 为 config.json 后再填写。`);
}

async function sendLongReply(napcatClient, context, messageId, text) {
  return await napcatClient.replyText(context, messageId, text);
}

async function sendChatResultIfPresent(config, napcatClient, context, messageId, result) {
  const replyText = formatChatReplyForSend(config, result?.text);
  if (!replyText) {
    return;
  }
  await sendLongReply(napcatClient, context, messageId, replyText);
}

function extractMessageIdsFromSendResults(results) {
  const normalizedResults = Array.isArray(results) ? results : [results];
  const ids = [];
  const visit = (value) => {
    if (!value) {
      return;
    }
    if (Array.isArray(value)) {
      value.forEach(visit);
      return;
    }
    if (typeof value === 'object') {
      const messageId = String(value.message_id ?? value.messageId ?? '').trim();
      if (messageId) {
        ids.push(messageId);
      }
      Object.values(value).forEach(visit);
    }
  };
  normalizedResults.forEach(visit);
  return Array.from(new Set(ids));
}

async function extractOpenAiImagesFromMessage(message) {
  const images = [];
  for (const segment of extractImageSegments(message)) {
    const imageContent = await imageSegmentToOpenAiContent(segment);
    if (imageContent) {
      images.push(imageContent);
    }
  }
  return images;
}

async function tryGetMessage(napcatClient, messageId) {
  const normalized = String(messageId ?? '').trim();
  if (!normalized) {
    return null;
  }
  try {
    return await napcatClient.getMessage(normalized);
  } catch {
    return null;
  }
}

function buildRequestContextPrefix(context, event) {
  const target = context.messageType === 'group'
    ? `群 ${context.groupId}`
    : `私聊 ${context.userId}`;
  return [
    `当前请求时间：${formatEpochTime(event?.time)}`,
    `当前消息来源：${target}`,
    `当前发送者：${getSenderName(event)} (${context.userId || '-'})`,
    `当前消息ID：${String(event?.message_id ?? '').trim() || '-'}`
  ].join('\n');
}

function buildAiRuntimePrefix(config) {
  return [
    `当前 AI 身份：${String(config?.bot?.displayName ?? '[Bot]Cain').trim() || '[Bot]Cain'}`,
    `当前 AI 模型：${String(config?.qa?.answer?.model ?? '').trim() || String(config?.qa?.client?.model ?? '').trim() || '未知'}`,
    `当前日期时间：${formatCurrentDateTime()}`,
    `当前模型框架：${describeModelFramework(config?.qa?.client?.baseUrl)}`
  ].join('\n');
}

function formatTextFileSections(label, result) {
  const sections = [];
  const files = Array.isArray(result?.files) ? result.files : [];
  files.forEach((file, index) => {
    sections.push([
      `${label}${index + 1}：${file.fileName}`,
      file.truncated ? '以下内容已截断。' : '',
      file.text
    ].filter(Boolean).join('\n'));
  });
  const errors = Array.isArray(result?.errors) ? result.errors.filter(Boolean) : [];
  if (errors.length > 0) {
    sections.push(`${label}读取失败：${errors.join('；')}`);
  }
  return sections;
}

async function buildChatInput(napcatClient, event, options = {}) {
  const context = createContextFromEvent(event);
  const explicitText = String(options?.argument ?? options?.rawArgs ?? '').trim();
  const allowCurrentTextFallback = options?.allowCurrentTextFallback === true;
  const aiRuntimePrefix = String(options?.aiRuntimePrefix ?? '').trim();
  const replyId = extractReplyId(event?.message, event?.raw_message);
  const repliedMessage = await tryGetMessage(napcatClient, replyId);

  const currentImages = await extractOpenAiImagesFromMessage(event?.message);
  const replyImages = repliedMessage ? await extractOpenAiImagesFromMessage(repliedMessage?.message) : [];
  const currentFiles = await readTextFilesFromMessage(napcatClient, event?.message, { maxFiles: 3, maxChars: 12000 });
  const replyFiles = repliedMessage
    ? await readTextFilesFromMessage(napcatClient, repliedMessage?.message, { maxFiles: 3, maxChars: 12000 })
    : { files: [], errors: [] };

  const currentText = allowCurrentTextFallback ? plainTextFromMessage(event?.message, event?.raw_message) : '';
  const replyText = plainTextFromMessage(repliedMessage?.message, repliedMessage?.raw_message);
  const effectiveText = explicitText || currentText;
  const combinedImages = [...replyImages, ...currentImages];

  const sections = [aiRuntimePrefix, buildRequestContextPrefix(context, event)].filter(Boolean);
  if (effectiveText) {
    sections.push(`用户当前消息：${effectiveText}`);
  }
  if (replyText) {
    sections.push(`引用消息文本：${replyText}`);
  }
  sections.push(...formatTextFileSections('当前消息附件文本', currentFiles));
  sections.push(...formatTextFileSections('引用消息附件文本', replyFiles));

  let text = sections.filter(Boolean).join('\n\n').trim();
  if (!text && combinedImages.length > 0) {
    text = `请结合这 ${combinedImages.length} 张图片回答用户。`;
  } else if (combinedImages.length > 0) {
    text = `${text}\n\n请结合附带的 ${combinedImages.length} 张图片一起回答。`.trim();
  }

  return {
    text,
    images: combinedImages,
    historyText: text,
    runtimeContext: {
      context,
      senderName: getSenderName(event),
      timelineText: effectiveText
        || replyText
        || (combinedImages.length > 0 ? `附带图片 ${combinedImages.length} 张` : '')
        || (currentFiles.files?.length ? `附带文本文件 ${currentFiles.files.length} 个` : '')
        || '发起了一次问答请求',
      currentMessageId: String(event?.message_id ?? '').trim(),
      currentTime: Number(event?.time ?? 0) || 0
    }
  };
}

function appendExtraSectionsToChatInput(baseInput, extraSections = []) {
  const sections = Array.isArray(extraSections) ? extraSections.filter(Boolean) : [];
  if (sections.length === 0) {
    return baseInput;
  }
  const extraText = sections.join('\n\n');
  return {
    ...baseInput,
    text: [baseInput?.text, extraText].filter(Boolean).join('\n\n'),
    historyText: [baseInput?.historyText, extraText].filter(Boolean).join('\n\n')
  };
}

async function buildTranslationInput(napcatClient, event, command) {
  const explicitText = String(command?.argument ?? command?.rawArgs ?? '').trim();
  const replyId = extractReplyId(event?.message, event?.raw_message);
  const repliedMessage = await tryGetMessage(napcatClient, replyId);
  const replyText = plainTextFromMessage(repliedMessage?.message, repliedMessage?.raw_message);
  const currentFiles = await readTextFilesFromMessage(napcatClient, event?.message, { maxFiles: 3, maxChars: 12000 });
  const replyFiles = repliedMessage
    ? await readTextFilesFromMessage(napcatClient, repliedMessage?.message, { maxFiles: 3, maxChars: 12000 })
    : { files: [], errors: [] };
  const currentImages = await extractOpenAiImagesFromMessage(event?.message);
  const replyImages = repliedMessage ? await extractOpenAiImagesFromMessage(repliedMessage?.message) : [];

  const parts = [];
  if (explicitText) {
    parts.push(explicitText);
  } else if (replyText) {
    parts.push(replyText);
  }
  parts.push(...formatTextFileSections('当前消息附件文本', currentFiles));
  parts.push(...formatTextFileSections('引用消息附件文本', replyFiles));

  return {
    text: parts.filter(Boolean).join('\n\n').trim(),
    images: [...replyImages, ...currentImages]
  };
}

function getTrackedMsavSections(msavMapAnalyzer, event) {
  const replyId = extractReplyId(event?.message, event?.raw_message);
  if (!replyId) {
    return [];
  }
  const tracked = msavMapAnalyzer.getReplyContext(replyId);
  if (!tracked?.metadataText) {
    return [];
  }
  const sections = [`以下是本地 .msav 地图解析信息：\n${tracked.metadataText}`];
  if (isNonEmptyString(tracked?.introduction)) {
    sections.push(`上一次给用户的地图介绍：\n${tracked.introduction}`);
  }
  return sections;
}

async function getUserGroupRole(napcatClient, event, context, ownerUserId) {
  if (String(context?.userId ?? '') === String(ownerUserId ?? '')) {
    return 'owner-bot';
  }
  const senderRole = String(event?.sender?.role ?? '').trim().toLowerCase();
  if (senderRole === 'owner' || senderRole === 'admin') {
    return senderRole;
  }
  if (context?.messageType !== 'group' || !context?.groupId || !context?.userId) {
    return 'member';
  }
  try {
    const info = await napcatClient.getGroupMemberInfo(context.groupId, context.userId, true);
    const role = String(info?.role ?? '').trim().toLowerCase();
    if (role === 'owner' || role === 'admin') {
      return role;
    }
  } catch {
  }
  return 'member';
}

function requireGroupId(command, context) {
  const groupId = String(command?.flags?.group ?? context?.groupId ?? '').trim();
  if (!groupId) {
    throw new Error('该命令需要群号；如果你在私聊里使用，请加 --group <群号>');
  }
  return groupId;
}

function getEditInstruction(command, subcommand) {
  const rawArgs = String(command?.rawArgs ?? '').trim();
  if (!rawArgs) {
    return '';
  }
  const normalizedSubcommand = String(subcommand ?? '').trim();
  if (!normalizedSubcommand) {
    return rawArgs;
  }
  if (rawArgs.startsWith(normalizedSubcommand)) {
    return rawArgs.slice(normalizedSubcommand.length).trim();
  }
  const tokens = tokenizeCommandLine(rawArgs);
  if (tokens.length <= 1) {
    return '';
  }
  return tokens.slice(1).join(' ').trim();
}

function formatGroupStatus(status) {
  return [
    `当前群启用状态：${status.enabled ? '已启用' : '未启用'}`,
    `当前主动回复状态：${status.proactiveReplyEnabled ? '已启用' : '已关闭'}`,
    `当前文件下载状态：${status.fileDownloadEnabled ? '已启用' : '已关闭'}`,
    `当前文件下载群文件夹：${status.fileDownloadFolderName || '(根目录)'}`,
    '',
    '当前过滤 prompt：',
    status.filterPrompt || '(空)',
    '',
    '当前聊天 prompt：',
    status.answerPrompt || '(空)'
  ].join('\n');
}

function isQuestionIntentText(text) {
  const normalized = String(text ?? '').trim();
  if (!normalized) {
    return false;
  }

  const compact = normalized.replace(/\s+/g, '');
  if (!compact) {
    return false;
  }

  if (/[?？]/.test(compact)) {
    return true;
  }

  if (/^(谁|什么|怎么|咋|如何|为什么|为啥|哪里|哪儿|哪个|哪位|是否|是不是|有无|有没有|能否|能不能|可不可以|行不行|咋办|怎么办)/.test(compact)) {
    return true;
  }

  if (/(请问|求问|求助|求解|请教|帮忙|help|报错|错误|bug|故障|没反应|不生效|无效|失效|什么意思|是什么|是啥|怎么弄|怎么做|怎么打|怎么过|怎么配|怎么改|在哪|还是)/i.test(compact)) {
    return true;
  }

  return false;
}

function extractRecalledMessageId(event) {
  const messageId = String(event?.message_id ?? event?.msg_id ?? '').trim();
  return messageId || '';
}

function extractJsonObject(text) {
  const source = String(text ?? '').trim();
  if (!source) {
    return null;
  }
  try {
    return JSON.parse(source);
  } catch {
  }

  const start = source.indexOf('{');
  const end = source.lastIndexOf('}');
  if (start >= 0 && end > start) {
    try {
      return JSON.parse(source.slice(start, end + 1));
    } catch {
    }
  }
  return null;
}

function looksLikeBotOppositionCandidate(text, displayName = '') {
  const normalized = String(text ?? '').trim().toLowerCase();
  const normalizedDisplayName = String(displayName ?? '').trim().toLowerCase();
  if (!normalized) {
    return false;
  }
  if (/(机器人|機器人|bot|自动回复|ai|闭嘴|别说话|别回复|别回|太吵|好吵|谁的机器人|关掉|关闭)/i.test(normalized)) {
    return true;
  }
  if (normalizedDisplayName && normalized.includes(normalizedDisplayName.toLowerCase())) {
    return true;
  }
  return false;
}

function textContainsShutdownVoteApproval(text) {
  const normalized = String(text ?? '').trim();
  if (!normalized) {
    return false;
  }
  return /(^|[^A-Za-z0-9])y([^A-Za-z0-9]|$)/i.test(normalized);
}

function shouldReplyErrorToChat(error) {
  if (!error) {
    return true;
  }
  if (error.silent === true) {
    return false;
  }
  const message = String(error.message ?? error).trim();
  if (!message) {
    return true;
  }
  if (/聊天接口/.test(message)) {
    return false;
  }
  return true;
}

async function handleCommand(params) {
  const {
    config,
    command,
    context,
    event,
    napcatClient,
    chatSessionManager,
    translator,
    msavMapAnalyzer
  } = params;

  switch (command.name) {
    case 'help': {
      await sendLongReply(napcatClient, context, event.message_id, buildHelpText(config));
      return true;
    }
    case 'chat': {
      if (context.messageType === 'group' && !chatSessionManager.isGroupEnabled(context.groupId)) {
        throw new Error('当前群未启用 Cain 问答。请先由 bot 主人执行 /e 启用，或把群号加入 qa.enabledGroupIds。');
      }
      const chatInput = appendExtraSectionsToChatInput(
        await buildChatInput(napcatClient, event, {
          argument: command.argument,
          rawArgs: command.rawArgs,
          allowCurrentTextFallback: false,
          aiRuntimePrefix: buildAiRuntimePrefix(config)
        }),
        getTrackedMsavSections(msavMapAnalyzer, event)
      );
      const result = await chatSessionManager.chat(context, chatInput);
      await sendChatResultIfPresent(config, napcatClient, context, event.message_id, result);
      return true;
    }
    case 'translate': {
      const source = await buildTranslationInput(napcatClient, event, command);
      if (!source.text && source.images.length === 0) {
        throw new Error('没有可翻译的内容；请直接写在命令后，或引用一条消息，或附带图片。');
      }
      const translated = await translator.translate(source);
      await sendLongReply(napcatClient, context, event.message_id, translated);
      return true;
    }
    case 'edit': {
      const subcommand = String(command.positionals?.[0] ?? '').trim();
      if (!E_SUBCOMMANDS.has(subcommand)) {
        throw new Error('用法：/e 状态 | /e 过滤 <要求> | /e 聊天 <要求> | /e 启用 | /e 禁用 | /e 文件下载 启用 [群文件夹名]|关闭');
      }
      const groupId = requireGroupId(command, context);
      const role = await getUserGroupRole(napcatClient, event, context, config.bot.ownerUserId);
      const isOwner = String(context.userId) === String(config.bot.ownerUserId ?? '');

      if (subcommand === '状态') {
        const status = chatSessionManager.getGroupPromptStatus(groupId);
        await sendLongReply(napcatClient, context, event.message_id, formatGroupStatus(status));
        return true;
      }

      if (subcommand === '启用' || subcommand === '禁用') {
        if (!isOwner) {
          throw new Error('/e 启用 和 /e 禁用 仅 bot 主人可用。');
        }
        const result = await params.runtimeConfigStore.setQaGroupEnabled(groupId, subcommand === '启用');
        const status = chatSessionManager.getGroupPromptStatus(groupId);
        await sendLongReply(
          napcatClient,
          context,
          event.message_id,
          [
            result.action === 'created' ? '已创建群开关记录。' : '已更新群开关记录。',
            formatGroupStatus(status)
          ].join('\n\n')
        );
        return true;
      }

      if (subcommand === '文件下载') {
        if (!(isOwner || role === 'owner' || role === 'admin')) {
          throw new Error('只有该群群主、管理员或 bot 主人可以修改文件下载开关。');
        }
        const action = String(command.positionals?.[1] ?? '').trim();
        if (action !== '启用' && action !== '关闭') {
          throw new Error('用法：/e 文件下载 启用 [群文件夹名]|关闭');
        }
        const folderName = action === '启用'
          ? String(command.positionals?.slice(2).join(' ') ?? '').trim()
          : '';
        const result = await params.runtimeConfigStore.setQaGroupFileDownloadEnabled(
          groupId,
          action === '启用',
          config.qa.enabledGroupIds,
          folderName
        );
        const status = chatSessionManager.getGroupPromptStatus(groupId);
        await sendLongReply(
          napcatClient,
          context,
          event.message_id,
          [
            result.action === 'created' ? '已创建文件下载开关记录。' : '已更新文件下载开关记录。',
            formatGroupStatus(status)
          ].join('\n\n')
        );
        return true;
      }

      if (!(isOwner || role === 'owner' || role === 'admin')) {
        throw new Error('只有该群群主、管理员或 bot 主人可以修改 prompt。');
      }

      const instruction = getEditInstruction(command, subcommand);
      if (!instruction) {
        throw new Error(`/e ${subcommand} 后必须跟修改要求。`);
      }

      const result = subcommand === '过滤'
        ? await chatSessionManager.updateFilterPrompt(groupId, instruction)
        : await chatSessionManager.updateAnswerPrompt(groupId, instruction);
      const status = chatSessionManager.getGroupPromptStatus(groupId);
      await sendLongReply(
        napcatClient,
        context,
        event.message_id,
        [
          '修改完成。',
          result.reason ? `审核说明：${result.reason}` : '',
          '',
          subcommand === '过滤' ? '当前过滤 prompt：' : '当前聊天 prompt：',
          result.prompt,
          '',
          '当前完整状态：',
          formatGroupStatus(status)
        ].filter(Boolean).join('\n')
      );
      return true;
    }
    default:
      return false;
  }
}

async function main() {
  const caSetupResult = enableSystemCaCertificates();
  const configPath = process.env.CAINBOT_CONFIG
    ? path.resolve(process.env.CAINBOT_CONFIG)
    : path.join(projectRoot, 'config.json');

  await ensureConfigExists(configPath);
  const { config, configDir } = await loadConfig(configPath);
  const logger = new Logger(config.bot.logLevel);
  fatalLogger = logger;
  if (caSetupResult.enabled) {
    logger.info(`已启用系统证书库，合并 CA ${caSetupResult.mergedCount} 条（bundled=${caSetupResult.bundledCount}, system=${caSetupResult.systemCount}）。`);
  } else {
    logger.warn(`未启用系统证书库：${caSetupResult.reason}`);
  }

  const stateStore = new StateStore(config.bot.stateFile, logger);
  await stateStore.load();

  const runtimeConfigStore = new RuntimeConfigStore(
    config.bot.runtimeConfigFile,
    configDir,
    {},
    logger
  );
  await runtimeConfigStore.load();

  const webUiSyncStore = new WebUiSyncStore(
    path.join(projectRoot, 'data', 'webui-sync.json'),
    logger
  );
  await webUiSyncStore.load();

  const napcatClient = new NapCatClient({
    ...config.napcat,
    forwardNickname: config.bot.displayName,
    botUserId: config.bot.ownerUserId
  }, logger);
  logger.setNonInfoNotifier(async ({ text }) => {
    const ownerUserId = String(config.bot.ownerUserId ?? '').trim();
    if (!ownerUserId) {
      return;
    }
    await napcatClient.sendPrivateMessage(ownerUserId, trimOwnerLogText(text));
  });
  const replySourceToBotReplies = new Map();
  const botReplyToSource = new Map();
  const groupNicknameSyncState = new Map();
  const shutdownVotesByGroup = new Map();
  const shutdownVoteMessageToGroup = new Map();

  const trackReplyResults = ({ replyToMessageId, results }) => {
    const sourceId = String(replyToMessageId ?? '').trim();
    if (!sourceId) {
      return;
    }
    const messageIds = extractMessageIdsFromSendResults(results);
    if (messageIds.length === 0) {
      return;
    }
    const existing = replySourceToBotReplies.get(sourceId) ?? new Set();
    for (const messageId of messageIds) {
      existing.add(messageId);
      botReplyToSource.set(messageId, sourceId);
    }
    replySourceToBotReplies.set(sourceId, existing);
  };
  napcatClient.onReplySent = async (payload) => {
    trackReplyResults(payload);
  };
  const qaClient = new OpenAiChatClient(config.qa.client, logger);
  const translator = new OpenAiTranslator(config.translation, logger);
  const localRagRetriever = new LocalRagRetriever(config.qa.answer, logger);
  const groupFileDownloadManager = new GroupFileDownloadManager(
    config.qa.answer,
    runtimeConfigStore,
    napcatClient,
    logger,
    {
      downloadRoot: path.join(projectRoot, 'data', 'release-downloads')
    }
  );
  const msavMapAnalyzer = new MsavMapAnalyzer({
    projectRoot,
    cacheDir: path.join(projectRoot, 'data', 'msav-parser'),
    napcatClient,
    chatClient: qaClient,
    logger,
    webUiSyncStore,
    model: config.qa.answer.model
  });

  const codexReadonlyTools = new CodexReadonlyTools(config.qa.answer, logger, {
    memoryFile: config.qa.answer.memoryFile,
    promptImageRoot: config.qa.answer.promptImageRoot,
    readRecentMessages: async () => [],
    readGroupMessages: async () => [],
    startGroupFileDownload: async (request, runtimeContext = {}) => {
      return await groupFileDownloadManager.startGroupDownloadFlowFromTool(
        runtimeContext?.context ?? {},
        {
          message_id: String(runtimeContext?.currentMessageId ?? '').trim(),
          raw_message: String(request?.request_text ?? request?.text ?? request?.query ?? runtimeContext?.timelineText ?? '').trim()
        },
        request
      );
    },
    sendPromptImage: async ({ context, imagePath }) => {
      await napcatClient.sendContextImage(context, imagePath);
    }
  });

  const chatSessionManager = new ChatSessionManager(
    config.qa,
    qaClient,
    stateStore,
    logger,
    codexReadonlyTools,
    runtimeConfigStore,
    localRagRetriever
  );

  const codexBridgeServer = new CodexBridgeServer(config.codexBridge, napcatClient, logger);
  const codexBridgeInfo = await codexBridgeServer.start();
  const webUiDirectInfo = await buildWebUiDirectInfo(config.napcat.webUiConfigPath);
  if (codexBridgeInfo?.baseUrl && config.bot.ownerUserId) {
    try {
      await napcatClient.sendPrivateMessage(
        config.bot.ownerUserId,
        [
          'Cain 已启动。',
          webUiDirectInfo?.userPanelUrl ? `WebUI直达：${webUiDirectInfo.userPanelUrl}` : '',
          webUiDirectInfo?.baseUrl ? `WebUI主页：${webUiDirectInfo.baseUrl}/webui` : '',
          webUiDirectInfo?.monitorPageUrl ? `Cain 面板：${webUiDirectInfo.monitorPageUrl}` : '',
          `Codex 文件桥：${codexBridgeInfo.baseUrl}`,
          `健康检查：${codexBridgeInfo.healthUrl}`
        ].filter(Boolean).join('\n')
      );
    } catch (error) {
      logger.warn(`通知主人启动信息失败：${error.message}`);
    }
  }

  const idleTimers = new Map();
  const idleDelayMs = Math.max(1, Number(config.qa.topicClosure.idleMinutes ?? 15)) * 60 * 1000;
  const clearIdleTimer = (groupId) => {
    const normalizedGroupId = String(groupId ?? '').trim();
    if (!normalizedGroupId) {
      return;
    }
    const existing = idleTimers.get(normalizedGroupId);
    if (existing?.timer) {
      clearTimeout(existing.timer);
    }
    idleTimers.delete(normalizedGroupId);
  };
  const touchGroupActivity = (groupId) => {
    const normalizedGroupId = String(groupId ?? '').trim();
    if (!normalizedGroupId || !chatSessionManager.isGroupEnabled(normalizedGroupId)) {
      return;
    }
    const previous = idleTimers.get(normalizedGroupId);
    if (previous?.timer) {
      clearTimeout(previous.timer);
    }
    const token = Date.now();
    const timer = setTimeout(async () => {
      const current = idleTimers.get(normalizedGroupId);
      if (!current || current.token !== token) {
        return;
      }
      if (!chatSessionManager.isGroupEnabled(normalizedGroupId)) {
        return;
      }
      try {
        const result = await chatSessionManager.maybeCloseGroupTopic(normalizedGroupId);
        logger.info(`群 ${normalizedGroupId} 空闲话题判断：${result.shouldEnd ? '结束' : '继续'}${result.reason ? ` (${result.reason})` : ''}`);
      } catch (error) {
        logger.warn(`群 ${normalizedGroupId} 空闲话题判断失败：${error.message}`);
      }
    }, idleDelayMs);
    idleTimers.set(normalizedGroupId, { token, timer });
  };

  const deleteTrackedReplyMessage = async (replyMessageId) => {
    const normalizedReplyId = String(replyMessageId ?? '').trim();
    if (!normalizedReplyId) {
      return;
    }
    const sourceId = botReplyToSource.get(normalizedReplyId);
    botReplyToSource.delete(normalizedReplyId);
    if (sourceId) {
      const linked = replySourceToBotReplies.get(sourceId);
      if (linked) {
        linked.delete(normalizedReplyId);
        if (linked.size === 0) {
          replySourceToBotReplies.delete(sourceId);
        }
      }
    }
    try {
      await napcatClient.deleteMessage(normalizedReplyId);
    } catch (error) {
      logger.warn(`撤回 bot 消息失败 ${normalizedReplyId}: ${error.message}`);
    }
  };

  const recallLinkedReplies = async (sourceMessageId) => {
    const normalizedSourceId = String(sourceMessageId ?? '').trim();
    if (!normalizedSourceId) {
      return;
    }
    const linked = replySourceToBotReplies.get(normalizedSourceId);
    if (!linked || linked.size === 0) {
      return;
    }
    replySourceToBotReplies.delete(normalizedSourceId);
    await Promise.allSettled(Array.from(linked).map((replyMessageId) => deleteTrackedReplyMessage(replyMessageId)));
  };

  const clearShutdownVote = (groupId) => {
    const normalizedGroupId = String(groupId ?? '').trim();
    if (!normalizedGroupId) {
      return;
    }
    const vote = shutdownVotesByGroup.get(normalizedGroupId);
    if (!vote) {
      return;
    }
    shutdownVotesByGroup.delete(normalizedGroupId);
    for (const messageId of vote.messageIds) {
      shutdownVoteMessageToGroup.delete(messageId);
    }
  };

  const getActiveShutdownVote = (groupId) => {
    const normalizedGroupId = String(groupId ?? '').trim();
    if (!normalizedGroupId) {
      return null;
    }
    const vote = shutdownVotesByGroup.get(normalizedGroupId);
    if (!vote) {
      return null;
    }
    if (vote.expiresAt <= Date.now()) {
      clearShutdownVote(normalizedGroupId);
      return null;
    }
    return vote;
  };

  const getShutdownVoteByReplyMessageId = (messageId) => {
    const normalizedMessageId = String(messageId ?? '').trim();
    if (!normalizedMessageId) {
      return null;
    }
    const groupId = shutdownVoteMessageToGroup.get(normalizedMessageId);
    if (!groupId) {
      return null;
    }
    const vote = getActiveShutdownVote(groupId);
    if (!vote) {
      shutdownVoteMessageToGroup.delete(normalizedMessageId);
      return null;
    }
    return vote;
  };

  const ensureGroupNickname = async (groupId, selfId) => {
    const normalizedGroupId = String(groupId ?? '').trim();
    const normalizedSelfId = String(selfId ?? '').trim();
    const targetNickname = String(config.bot.groupNickname ?? config.bot.displayName ?? '').trim();
    if (!normalizedGroupId || !normalizedSelfId || !targetNickname) {
      return;
    }
    const stateKey = `${normalizedGroupId}:${normalizedSelfId}`;
    const current = groupNicknameSyncState.get(stateKey);
    if (current?.status === 'pending') {
      return;
    }
    if (current?.status === 'ok' && current?.nickname === targetNickname) {
      return;
    }
    if (current?.status === 'failed' && current?.nickname === targetNickname && (Date.now() - current.updatedAt) < GROUP_CARD_SYNC_RETRY_MS) {
      return;
    }

    groupNicknameSyncState.set(stateKey, {
      status: 'pending',
      nickname: targetNickname,
      updatedAt: Date.now()
    });

    try {
      const info = await napcatClient.getGroupMemberInfo(normalizedGroupId, normalizedSelfId, true).catch(() => null);
      if (String(info?.card ?? '').trim() === targetNickname) {
        groupNicknameSyncState.set(stateKey, {
          status: 'ok',
          nickname: targetNickname,
          updatedAt: Date.now()
        });
        return;
      }
      await napcatClient.setGroupCard(normalizedGroupId, normalizedSelfId, targetNickname);
      groupNicknameSyncState.set(stateKey, {
        status: 'ok',
        nickname: targetNickname,
        updatedAt: Date.now()
      });
      logger.info(`已同步群 ${normalizedGroupId} 的 bot 群名片为 ${targetNickname}`);
    } catch (error) {
      groupNicknameSyncState.set(stateKey, {
        status: 'failed',
        nickname: targetNickname,
        updatedAt: Date.now()
      });
      logger.warn(`同步群 ${normalizedGroupId} 的 bot 群名片失败：${error.message}`);
    }
  };

  const classifyBotOpposition = async (context, event, text) => {
    if (!looksLikeBotOppositionCandidate(text, config.bot.displayName)) {
      return { shouldStartVote: false, reason: 'heuristic-skip' };
    }
    const raw = await qaClient.complete([
      {
        role: 'system',
        content: [
          '你负责判断一条 QQ 群消息是否在反对、嫌弃、质疑或要求关闭当前 bot 的发言功能。',
          '以下情形判定 should_start_vote=true：嫌 bot 吵、叫 bot 闭嘴/别回复、质疑“这是谁的机器人”、反感 bot 存在、希望关掉 bot 功能。',
          '以下情形判定 should_start_vote=false：正常向 bot 提问、普通讨论机器人技术实现、友善地询问 bot 来源且没有反对语气。',
          '输出必须是 JSON：{"should_start_vote":boolean,"reason":"简短原因"}。不要输出额外文字。'
        ].join('\n\n')
      },
      {
        role: 'user',
        content: [
          `群号：${context.groupId}`,
          `发送者：${getSenderName(event)} (${context.userId || '-'})`,
          `消息内容：${String(text ?? '').trim() || '(空)'}`
        ].join('\n')
      }
    ], {
      model: SHUTDOWN_VOTE_FILTER_MODEL,
      temperature: 0.1
    });
    const parsed = extractJsonObject(raw);
    return {
      shouldStartVote: parsed?.should_start_vote === true,
      reason: String(parsed?.reason ?? '').trim()
    };
  };

  const maybeHandleShutdownVoteReply = async (context, event) => {
    if (context.messageType !== 'group') {
      return false;
    }
    const replyId = extractReplyId(event?.message, event?.raw_message);
    if (!replyId) {
      return false;
    }
    const vote = getShutdownVoteByReplyMessageId(replyId);
    if (!vote) {
      return false;
    }
    const replyText = plainTextFromMessage(event?.message, event?.raw_message);
    if (!textContainsShutdownVoteApproval(replyText)) {
      return true;
    }

    const voterId = String(context.userId ?? '').trim();
    if (!voterId) {
      return true;
    }
    vote.voters.add(voterId);
    vote.expiresAt = Date.now() + SHUTDOWN_VOTE_TTL_MS;
    if (vote.voters.size < SHUTDOWN_VOTE_REQUIRED_COUNT) {
      return true;
    }

    clearShutdownVote(context.groupId);
    await chatSessionManager.disableGroupProactiveReplies(context.groupId);
    await sendLongReply(
      napcatClient,
      context,
      event.message_id,
      '已根据投票关闭本群主动回复功能。仍可通过 @我 或 /chat 调用。'
    );
    return true;
  };

  const maybeStartShutdownVote = async (context, event, text) => {
    if (context.messageType !== 'group' || !chatSessionManager.isGroupEnabled(context.groupId) || !chatSessionManager.isGroupProactiveReplyEnabled(context.groupId)) {
      return false;
    }
    const decision = await classifyBotOpposition(context, event, text);
    if (!decision.shouldStartVote) {
      return false;
    }

    const activeVote = getActiveShutdownVote(context.groupId);
    if (activeVote) {
      await sendLongReply(
        napcatClient,
        context,
        event.message_id,
        '当前群已有关闭投票在进行，直接回复那条投票消息 "Y" 即可。'
      );
      return true;
    }

    const results = await sendLongReply(
      napcatClient,
      context,
      event.message_id,
      SHUTDOWN_VOTE_PROMPT
    );
    const messageIds = extractMessageIdsFromSendResults(results);
    if (messageIds.length === 0) {
      return true;
    }

    const vote = {
      groupId: String(context.groupId ?? '').trim(),
      sourceMessageId: String(event?.message_id ?? '').trim(),
      initiatorUserId: String(context.userId ?? '').trim(),
      messageIds: new Set(messageIds),
      voters: new Set(),
      createdAt: Date.now(),
      expiresAt: Date.now() + SHUTDOWN_VOTE_TTL_MS
    };
    shutdownVotesByGroup.set(vote.groupId, vote);
    messageIds.forEach((messageId) => shutdownVoteMessageToGroup.set(messageId, vote.groupId));
    logger.info(`群 ${vote.groupId} 发起关闭投票：${decision.reason || 'bot-opposition'}`);
    return true;
  };

  const handleNoticeEvent = async (event) => {
    const noticeType = String(event?.notice_type ?? '').trim();
    if (!['group_recall', 'friend_recall'].includes(noticeType)) {
      return;
    }
    const recalledMessageId = extractRecalledMessageId(event);
    if (!recalledMessageId) {
      return;
    }
    await recallLinkedReplies(recalledMessageId);
    if (botReplyToSource.has(recalledMessageId)) {
      const sourceId = botReplyToSource.get(recalledMessageId);
      botReplyToSource.delete(recalledMessageId);
      const linked = sourceId ? replySourceToBotReplies.get(sourceId) : null;
      if (linked) {
        linked.delete(recalledMessageId);
        if (linked.size === 0) {
          replySourceToBotReplies.delete(sourceId);
        }
      }
    }
  };

  const handleRequestEvent = async (event) => {
    const requestType = String(event?.request_type ?? '').trim();
    const subType = String(event?.sub_type ?? '').trim();
    if (requestType !== 'group' || subType !== 'invite') {
      return;
    }
    const flag = String(event?.flag ?? '').trim();
    if (!flag) {
      logger.warn(`收到群邀请请求但缺少 flag：${JSON.stringify(event)}`);
      return;
    }
    await napcatClient.setGroupAddRequest(flag, true, '');
    logger.info(
      `已自动同意群邀请：group=${String(event?.group_id ?? '').trim() || '-'} inviter=${String(event?.user_id ?? '').trim() || '-'}`
    );
  };

  let shuttingDown = false;
  const shutdown = async (signal = 'shutdown') => {
    if (shuttingDown) {
      return;
    }
    shuttingDown = true;
    logger.info(`收到 ${signal}，准备停止 Cain Bot。`);
    napcatClient.stop();
    for (const item of idleTimers.values()) {
      clearTimeout(item?.timer);
    }
    idleTimers.clear();
    await Promise.allSettled([
      codexBridgeServer.stop()
    ]);
  };

  process.once('SIGINT', () => { void shutdown('SIGINT'); });
  process.once('SIGTERM', () => { void shutdown('SIGTERM'); });

  logger.info('Cain Bot 启动中。');
  await napcatClient.startEventLoop(async (event) => {
    if (!event) {
      return;
    }
    if (event.post_type === 'request') {
      await handleRequestEvent(event);
      return;
    }
    if (event.post_type === 'notice') {
      await handleNoticeEvent(event);
      return;
    }
    if (event.post_type !== 'message') {
      return;
    }
    if (String(event.user_id ?? '') === String(event.self_id ?? '')) {
      return;
    }

    const context = createContextFromEvent(event);
    const text = plainTextFromMessage(event.message, event.raw_message);
    const command = parseIncomingCommand(text);

    try {
      if (context.messageType === 'group') {
        await ensureGroupNickname(context.groupId, context.selfId);
      }

      if (context.messageType === 'group' && chatSessionManager.isGroupEnabled(context.groupId)) {
        touchGroupActivity(context.groupId);
        if (await maybeHandleShutdownVoteReply(context, event)) {
          return;
        }
        if (await maybeStartShutdownVote(context, event, text)) {
          return;
        }
      }

      const msavResult = await msavMapAnalyzer.maybeAnalyzeIncomingMsav(context, event);
      if (msavResult?.handled) {
        return;
      }

      if (command) {
        await handleCommand({
          config,
          command,
          context,
          event,
          napcatClient,
          chatSessionManager,
          translator,
          runtimeConfigStore,
          msavMapAnalyzer
        });
        return;
      }

      if (context.messageType === 'group' && await groupFileDownloadManager.handleGroupMessage(context, event, text)) {
        return;
      }

      if (context.messageType === 'group' && chatSessionManager.isGroupEnabled(context.groupId) && eventMentionsSelf(event, config.bot.displayName)) {
        const chatInput = appendExtraSectionsToChatInput(
          await buildChatInput(napcatClient, event, {
            argument: text,
            rawArgs: text,
            allowCurrentTextFallback: true,
            aiRuntimePrefix: buildAiRuntimePrefix(config)
          }),
          getTrackedMsavSections(msavMapAnalyzer, event)
        );
        const result = await chatSessionManager.chat(context, chatInput);
        await sendChatResultIfPresent(config, napcatClient, context, event.message_id, result);
        return;
      }

      if (context.messageType === 'group' && chatSessionManager.isGroupEnabled(context.groupId)) {
        await chatSessionManager.recordIncomingMessage(context, event);
        void chatSessionManager.maybeCaptureCorrectionMemory(context, event).catch((error) => {
          logger.warn(`后台长期记忆捕获失败：${error.message}`);
        });
        if (!chatSessionManager.isGroupProactiveReplyEnabled(context.groupId)) {
          return;
        }
        if (!isQuestionIntentText(text)) {
          return;
        }
        if (eventMentionsOtherUser(event, config.bot.displayName)) {
          return;
        }
        const filterResult = await chatSessionManager.shouldSuggestReply(context, event);
        if (filterResult.shouldPrompt) {
          const chatInput = appendExtraSectionsToChatInput(
            await buildChatInput(napcatClient, event, {
              argument: text,
              rawArgs: text,
              allowCurrentTextFallback: true,
              aiRuntimePrefix: buildAiRuntimePrefix(config)
            }),
            getTrackedMsavSections(msavMapAnalyzer, event)
          );
          const result = await chatSessionManager.chat(context, chatInput);
          await sendChatResultIfPresent(config, napcatClient, context, event.message_id, result);
          await chatSessionManager.markHinted(context, event.message_id);
          return;
        }
      }
    } catch (error) {
      logger.error(`事件处理失败：${error.stack || error.message}`);
      if (!shouldReplyErrorToChat(error)) {
        logger.info(`已抑制聊天接口错误的群内提示：${error.message}`);
        return;
      }
      try {
        await sendLongReply(napcatClient, context, event.message_id, `处理失败：${error.message}`);
      } catch (replyError) {
        logger.warn(`发送错误提示失败：${replyError.message}`);
      }
    }
  });
}

await main().catch((error) => {
  if (fatalLogger) {
    fatalLogger.error(error?.stack || error?.message || error);
  } else {
    console.error(error?.stack || error?.message || error);
  }
  process.exitCode = 1;
});
