import fs from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { randomUUID } from 'node:crypto';
import { spawn } from 'node:child_process';

import { ensureDir, extractReplyId, nowIso } from './utils.mjs';

const OFFER_TTL_MS = 20 * 60 * 1000;
const DEFAULT_CODEX_TIMEOUT_MS = 30 * 60 * 1000;
const MAX_RECENT_LINES = 16;

const OUTPUT_SCHEMA = {
  type: 'object',
  additionalProperties: false,
  required: [
    'status',
    'assistantMessage',
    'issueSummary',
    'artifactPath',
    'artifactName',
    'artifactVersion',
    'publishFolderName'
  ],
  properties: {
    status: {
      type: 'string',
      enum: ['needs_user_reply', 'artifact_ready', 'done', 'failed']
    },
    assistantMessage: { type: 'string' },
    issueSummary: { type: 'string' },
    artifactPath: { type: 'string' },
    artifactName: { type: 'string' },
    artifactVersion: { type: 'string' },
    publishFolderName: { type: 'string' }
  }
};

function normalizeText(value) {
  return String(value ?? '').replace(/\r\n/g, '\n').trim();
}

function truncate(text, maxChars = 500) {
  const normalized = normalizeText(text);
  if (normalized.length <= maxChars) {
    return normalized;
  }
  return `${normalized.slice(0, maxChars)}...(已截断)`;
}

function stripJsonComments(text) {
  return String(text ?? '')
    .replace(/\/\*[\s\S]*?\*\//g, '')
    .replace(/(^|[^:])\/\/.*$/gm, '$1');
}

function readLooseField(text, fieldName) {
  const patterns = [
    new RegExp(`"${fieldName}"\\s*:\\s*"([^"\\n]+)"`, 'i'),
    new RegExp(`"${fieldName}"\\s*:\\s*'([^'\\n]+)'`, 'i'),
    new RegExp(`${fieldName}\\s*:\\s*"([^"\\n]+)"`, 'i'),
    new RegExp(`${fieldName}\\s*:\\s*'([^'\\n]+)'`, 'i'),
    new RegExp(`${fieldName}\\s*:\\s*([^,\\n]+)`, 'i')
  ];
  for (const pattern of patterns) {
    const match = text.match(pattern);
    if (match?.[1]) {
      return normalizeText(match[1]).replace(/^['"]|['"]$/g, '');
    }
  }
  return '';
}

function buildScopeKey(context, userIdOverride = '') {
  const messageType = normalizeText(context?.messageType) || 'group';
  const userId = normalizeText(userIdOverride || context?.userId);
  if (messageType === 'group') {
    return `group:${normalizeText(context?.groupId)}:user:${userId}`;
  }
  return `private:${userId}`;
}

function buildConversationText(messages) {
  return (Array.isArray(messages) ? messages : [])
    .slice(-MAX_RECENT_LINES)
    .map((entry, index) => {
      const speaker = normalizeText(entry?.speaker) || 'unknown';
      const text = normalizeText(entry?.text) || '(空)';
      return `${index + 1}. ${speaker}: ${text}`;
    })
    .join('\n');
}

function extractMessageIds(results) {
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
      const messageId = normalizeText(value?.message_id ?? value?.messageId);
      if (messageId) {
        ids.push(messageId);
      }
      Object.values(value).forEach(visit);
    }
  };
  visit(results);
  return Array.from(new Set(ids));
}

function extractThreadIdFromJsonl(text) {
  for (const line of String(text ?? '').split(/\r?\n/)) {
    const trimmed = line.trim();
    if (!trimmed.startsWith('{')) {
      continue;
    }
    try {
      const parsed = JSON.parse(trimmed);
      if (parsed?.type === 'thread.started' && parsed?.thread_id) {
        return normalizeText(parsed.thread_id);
      }
    } catch {
    }
  }
  return '';
}

function defaultPublishFolderName(modInfo) {
  return normalizeText(modInfo?.projectFolderName || modInfo?.name || modInfo?.displayName || 'mod')
    .replace(/[\\/:*?"<>|]/g, '_')
    .trim();
}

function normalizeIssueSummary(text) {
  const normalized = normalizeText(text);
  return normalized || '用户反馈该模组存在 bug 或体验问题';
}

function maybeProjectRootFromModFile(filePath) {
  const normalized = path.normalize(filePath);
  const segments = normalized.split(path.sep);
  const lower = segments.map((segment) => segment.toLowerCase());
  if (lower.includes('node_modules') || lower.includes('.git')) {
    return null;
  }
  if (lower.includes('build') || lower.includes('bin') || lower.includes('dist') || lower.includes('release') || segments.includes('构建')) {
    return null;
  }
  const joined = lower.join('/');
  if (joined.includes('/src/main/resources/')) {
    return path.resolve(path.dirname(filePath), '..', '..', '..');
  }
  return path.dirname(filePath);
}

async function scanOwnedMods(codexRoot, ownerName) {
  const results = [];
  const seenRoots = new Set();

  async function walk(dir) {
    const entries = await fs.readdir(dir, { withFileTypes: true }).catch(() => []);
    for (const entry of entries) {
      const fullPath = path.join(dir, entry.name);
      if (entry.isDirectory()) {
        if (entry.name.startsWith('.') || ['node_modules', 'build', 'bin', 'dist', 'release', 'out', 'target', '构建'].includes(entry.name)) {
          continue;
        }
        await walk(fullPath);
        continue;
      }
      if (!entry.isFile() || !/^mod\.(json|hjson)$/i.test(entry.name)) {
        continue;
      }
      const projectRoot = maybeProjectRootFromModFile(fullPath);
      if (!projectRoot || seenRoots.has(projectRoot)) {
        continue;
      }
      const raw = await fs.readFile(fullPath, 'utf8').catch(() => '');
      if (!raw) {
        continue;
      }
      const content = stripJsonComments(raw);
      const author = readLooseField(content, 'author');
      if (!author || !author.toLowerCase().includes(String(ownerName ?? '').trim().toLowerCase())) {
        continue;
      }
      seenRoots.add(projectRoot);
      results.push({
        id: path.basename(projectRoot),
        projectRoot,
        projectFolderName: path.basename(projectRoot),
        modFilePath: fullPath,
        name: readLooseField(content, 'name') || path.basename(projectRoot),
        displayName: readLooseField(content, 'displayName') || path.basename(projectRoot),
        author,
        version: readLooseField(content, 'version'),
        description: readLooseField(content, 'description'),
        repo: readLooseField(content, 'repo')
      });
    }
  }

  await walk(codexRoot);
  return results.sort((left, right) => left.projectFolderName.localeCompare(right.projectFolderName, 'zh-CN'));
}

function boolFromJsonField(value) {
  return value === true;
}

function parseJsonObject(text) {
  try {
    return JSON.parse(String(text ?? '').trim());
  } catch {
    return null;
  }
}

function buildCandidatePrompt(mods) {
  const catalog = mods
    .map((item) => [
      `projectKey=${item.id}`,
      `folder=${item.projectFolderName}`,
      `displayName=${item.displayName}`,
      `name=${item.name}`,
      `version=${item.version || '-'}`,
      `description=${item.description || '-'}`
    ].join(' | '))
    .join('\n');
  return [
    '你负责判断一条 QQ 消息是否在反馈作者自己的模组问题，并且应该主动询问“要不要我直接跟进修/优化”。',
    '只在同时满足以下条件时 should_offer=true：',
    '1. 能明确对应下列模组中的某一个。',
    '2. 消息表达的是 bug、报错、异常、体验不好、难用、误导、卡顿、兼容性差、需要优化体验。',
    '3. 不是单纯提问版本号、下载地址、功能说明，也不是泛泛闲聊。',
    '输出必须是 JSON：{"should_offer":boolean,"project_key":"匹配到的projectKey或空字符串","issue_summary":"一句话概括","reason":"简短原因"}。',
    '如果不确定，should_offer=false。',
    '',
    '可匹配模组目录：',
    catalog || '(空)'
  ].join('\n');
}

function buildConsentPrompt() {
  return [
    '你负责判断用户是否是在回应机器人刚刚提出的“要不要我直接跟进修/优化”。',
    '输出必须是 JSON：{"decision":"accept|decline|ignore","reason":"简短原因"}。',
    'accept 表示用户明确同意继续修/优化。',
    'decline 表示用户明确拒绝、暂时不要、表示不用修。',
    'ignore 表示不是在回应这件事，或意图不明。'
  ].join('\n');
}

function buildFollowupPrompt(session) {
  return [
    '你负责判断用户刚发的新消息，是否仍然是在和当前“模组修复会话”继续交流。',
    '输出必须是 JSON：{"is_followup":boolean,"reason":"简短原因"}。',
    '如果消息是在补充复现、描述新结果、反馈测试情况、表示修好了/没修好、继续提需求，则 is_followup=true。',
    '如果消息明显是在聊别的话题，则 is_followup=false。',
    '',
    '当前会话摘要：',
    normalizeIssueSummary(session?.issueSummary),
    '',
    '最近会话记录：',
    buildConversationText(session?.messages)
  ].join('\n');
}

function buildSatisfactionPrompt(session) {
  return [
    '你负责判断用户最新一条消息，是否意味着“这轮修复/优化已经可以收口，可以结束跟踪并发布当前产物”。',
    '不要靠单个关键词机械判断，要结合语气和上下文。',
    '输出必须是 JSON：{"accepted":boolean,"reason":"简短原因"}。',
    '',
    '当前问题：',
    normalizeIssueSummary(session?.issueSummary),
    '',
    '最近会话记录：',
    buildConversationText(session?.messages)
  ].join('\n');
}

function buildCodexPrompt(session, bridgeInfo) {
  const modInfo = session.modInfo ?? {};
  const bridgeLines = bridgeInfo?.baseUrl
    ? [
        `本机 Cain Codex bridge baseUrl: ${bridgeInfo.baseUrl}`,
        `send group message: ${bridgeInfo.sendGroupMessageUrl}`,
        `send private message: ${bridgeInfo.sendPrivateMessageUrl}`,
        `read group messages: ${bridgeInfo.readGroupMessagesUrl}`,
        `read private messages: ${bridgeInfo.readPrivateMessagesUrl}`,
        `send group file: ${bridgeInfo.sendGroupFileUrl}`,
        `send group file to folder: ${bridgeInfo.sendGroupFileToFolderUrl}`
      ]
    : ['当前未提供 Cain bridge。'];
  return [
    '你是一个持续跟踪 QQ 用户模组问题的主 agent。你的任务不是只给建议，而是尽可能直接修改代码、构建本地产物、让用户测试，直到问题解决或明确无法继续。',
    '如果你的运行环境支持 sub-agent / worker，请按需拉起 gpt-5.4-high 的子 agent 处理局部代码修改或验证任务。',
    '你当前运行在本地代码仓/目录里，可以直接编辑文件、运行构建命令、验证结果。',
    '版本规则：只允许发布小版本或中版本；不要升级大版本。需要同步修改项目中真实生效的版本号。',
    '交付规则：优先遵循项目已有的 build/deploy 任务，产物必须是本地真实存在的 zip/jar 等可发文件。',
    '如果信息仍不足，直接向用户提一个最小必要的问题，不要空谈。',
    '如果已经足够开始修，就直接动手，不要把实现方案整段甩给用户。',
    '最终输出必须严格符合 JSON Schema，不要输出任何额外文本。',
    '',
    `当前模组目录：${modInfo.projectRoot || '.'}`,
    `模组 folder：${modInfo.projectFolderName || '-'}`,
    `模组 displayName：${modInfo.displayName || '-'}`,
    `模组 name：${modInfo.name || '-'}`,
    `当前版本：${modInfo.version || session.latestArtifactVersion || '-'}`,
    `建议发布文件夹：${session.publishFolderName || defaultPublishFolderName(modInfo)}`,
    '',
    '与 Cain 通信的本地 bridge（如你想自行读取最近消息或发进度）如下：',
    ...bridgeLines,
    '注意：最终测试产物请不要自己通过 bridge 发送文件；只在 JSON 中返回 artifactPath / artifactName / publishFolderName，让宿主 Cain 发送。',
    '',
    '当前问题摘要：',
    normalizeIssueSummary(session.issueSummary),
    '',
    '最近用户会话记录：',
    buildConversationText(session.messages),
    '',
    '状态字段含义：',
    '1. needs_user_reply: 你需要 Cain 先向用户追问一个问题。',
    '2. artifact_ready: 你已经做出真实修改并准备好了可发给用户测试的产物。',
    '3. done: 已经无需继续追问，也不需要额外发新产物。',
    '4. failed: 当前确实无法继续，请说明原因。',
    '',
    '字段要求：',
    '1. assistantMessage 用简体中文，像真人，简洁直接。',
    '2. issueSummary 概括当前真正的问题。',
    '3. artifactPath 必须是绝对路径；如果没有产物就填空字符串。',
    '4. artifactName 没有就填空字符串。',
    '5. artifactVersion 没有就填空字符串。',
    '6. publishFolderName 没有就填建议文件夹名。'
  ].join('\n');
}

export class ModIssueRepairManager {
  constructor(config, chatClient, napcatClient, stateStore, logger, codexBridgeInfo = null) {
    this.config = config ?? {};
    this.chatClient = chatClient;
    this.napcatClient = napcatClient;
    this.stateStore = stateStore;
    this.logger = logger;
    this.codexBridgeInfo = codexBridgeInfo;
    this.runningSessions = new Map();
    this.modIndex = [];
    this.lastModIndexAt = 0;
  }

  async initialize() {
    if (this.config.enabled === false) {
      return;
    }
    await this.refreshModIndex();
  }

  async refreshModIndex(force = false) {
    if (!force && Date.now() - this.lastModIndexAt < 5 * 60 * 1000) {
      return this.modIndex;
    }
    const codexRoot = normalizeText(this.config.codexRoot);
    if (!codexRoot) {
      this.modIndex = [];
      return this.modIndex;
    }
    this.modIndex = await scanOwnedMods(codexRoot, this.config.ownerName || 'DeterMination');
    this.lastModIndexAt = Date.now();
    return this.modIndex;
  }

  async handleIncomingMessage(context, event, text) {
    if (this.config.enabled === false) {
      return false;
    }
    const normalizedText = normalizeText(text);
    if (!normalizedText) {
      return false;
    }
    await this.refreshModIndex();

    const scopeKey = buildScopeKey(context);
    const offer = this.#findPendingOffer(scopeKey);
    if (offer && await this.#maybeHandleOfferReply(offer, context, event, normalizedText)) {
      return true;
    }

    const session = this.#findActiveSession(scopeKey);
    if (session && await this.#maybeHandleSessionReply(session, context, event, normalizedText)) {
      return true;
    }

    const decision = await this.#classifyCandidate(context, event, normalizedText);
    if (!decision.shouldOffer) {
      return false;
    }

    const modInfo = this.modIndex.find((item) => item.id === decision.projectKey);
    if (!modInfo) {
      return false;
    }

    const createdAt = nowIso();
    const offerRecord = {
      id: randomUUID(),
      scopeKey,
      context: {
        messageType: normalizeText(context?.messageType) || 'group',
        groupId: normalizeText(context?.groupId),
        userId: normalizeText(context?.userId),
        selfId: normalizeText(context?.selfId)
      },
      targetUserId: normalizeText(context?.userId),
      sourceMessageId: normalizeText(event?.message_id),
      modId: modInfo.id,
      modDisplayName: modInfo.displayName || modInfo.projectFolderName,
      issueSummary: normalizeIssueSummary(decision.issueSummary),
      createdAt,
      updatedAt: createdAt,
      expiresAt: new Date(Date.now() + OFFER_TTL_MS).toISOString(),
      botMessageIds: []
    };
    const results = await this.napcatClient.replyText(
      context,
      event.message_id,
      `看起来像是 ${offerRecord.modDisplayName} 的 bug / 体验问题。要不要我直接跟进修一下或顺手优化体验？`
    );
    offerRecord.botMessageIds = extractMessageIds(results);
    this.stateStore.setIssueRepairOffer(offerRecord);
    await this.stateStore.save();
    return true;
  }

  #findPendingOffer(scopeKey) {
    const now = Date.now();
    for (const offer of this.stateStore.listIssueRepairOffers()) {
      if (normalizeText(offer?.scopeKey) !== scopeKey) {
        continue;
      }
      const expiresAt = Date.parse(String(offer?.expiresAt ?? ''));
      if (Number.isFinite(expiresAt) && expiresAt < now) {
        this.stateStore.deleteIssueRepairOffer(offer.id);
        continue;
      }
      return offer;
    }
    return null;
  }

  #findActiveSession(scopeKey) {
    return this.stateStore.listIssueRepairSessions().find((session) => {
      if (normalizeText(session?.scopeKey) !== scopeKey) {
        return false;
      }
      return !['completed', 'failed'].includes(normalizeText(session?.status));
    }) ?? null;
  }

  async #classifyCandidate(context, event, text) {
    if (this.modIndex.length === 0) {
      return { shouldOffer: false, projectKey: '', issueSummary: '' };
    }
    const raw = await this.chatClient.complete([
      {
        role: 'system',
        content: buildCandidatePrompt(this.modIndex)
      },
      {
        role: 'user',
        content: [
          `消息来源：${normalizeText(context?.messageType) === 'group' ? `群 ${normalizeText(context?.groupId)}` : `私聊 ${normalizeText(context?.userId)}`}`,
          `发送者：${normalizeText(event?.sender?.card || event?.sender?.nickname || event?.user_id || context?.userId || '用户')}`,
          `消息内容：${text}`
        ].join('\n')
      }
    ], {
      model: this.config.classifyModel || 'gpt-5.4-mini',
      temperature: 0.1
    });
    const parsed = parseJsonObject(raw) ?? {};
    return {
      shouldOffer: boolFromJsonField(parsed.should_offer),
      projectKey: normalizeText(parsed.project_key),
      issueSummary: normalizeIssueSummary(parsed.issue_summary)
    };
  }

  async #maybeHandleOfferReply(offer, context, event, text) {
    const raw = await this.chatClient.complete([
      {
        role: 'system',
        content: buildConsentPrompt()
      },
      {
        role: 'user',
        content: [
          `上一条机器人提议：看起来像是 ${normalizeText(offer.modDisplayName)} 的 bug / 体验问题，要不要直接跟进修复或优化。`,
          `用户刚发的新消息：${text}`,
          `该消息是否是对此提议的回应？`
        ].join('\n')
      }
    ], {
      model: this.config.consentModel || 'gpt-5.4-mini',
      temperature: 0.1
    });
    const parsed = parseJsonObject(raw) ?? {};
    const decision = normalizeText(parsed.decision);
    if (decision === 'ignore') {
      return false;
    }
    this.stateStore.deleteIssueRepairOffer(offer.id);
    if (decision !== 'accept') {
      await this.stateStore.save();
      return true;
    }

    const modInfo = this.modIndex.find((item) => item.id === offer.modId);
    if (!modInfo) {
      await this.stateStore.save();
      return true;
    }

    const session = {
      id: randomUUID(),
      scopeKey: offer.scopeKey,
      status: 'running',
      codexThreadId: '',
      context: offer.context,
      sourceMessageId: normalizeText(event?.message_id),
      targetUserId: offer.targetUserId,
      issueSummary: offer.issueSummary,
      modInfo,
      publishFolderName: defaultPublishFolderName(modInfo),
      createdAt: nowIso(),
      updatedAt: nowIso(),
      latestArtifactPath: '',
      latestArtifactName: '',
      latestArtifactVersion: '',
      messages: [
        {
          role: 'user',
          speaker: normalizeText(event?.sender?.card || event?.sender?.nickname || event?.user_id || context?.userId || '用户'),
          text,
          createdAt: nowIso()
        }
      ],
      botMessageIds: [],
      pendingRerun: false
    };
    this.stateStore.setIssueRepairSession(session);
    await this.stateStore.save();
    const noticeResults = await this.napcatClient.replyText(
      context,
      event.message_id,
      `行，我开始跟这个 ${modInfo.displayName || modInfo.projectFolderName} 问题，先直接修。`
    );
    session.botMessageIds = extractMessageIds(noticeResults);
    session.updatedAt = nowIso();
    this.stateStore.setIssueRepairSession(session);
    await this.stateStore.save();
    void this.#runSession(session.id, 'accepted');
    return true;
  }

  async #maybeHandleSessionReply(session, context, event, text) {
    const replyId = normalizeText(extractReplyId(event?.message, event?.raw_message));
    const directlyReplyingToSession = replyId && Array.isArray(session.botMessageIds) && session.botMessageIds.includes(replyId);
    if (!directlyReplyingToSession) {
      const raw = await this.chatClient.complete([
        {
          role: 'system',
          content: buildFollowupPrompt(session)
        },
        {
          role: 'user',
          content: `用户最新消息：${text}`
        }
      ], {
        model: this.config.followupModel || 'gpt-5.4-mini',
        temperature: 0.1
      });
      const parsed = parseJsonObject(raw) ?? {};
      if (parsed.is_followup !== true) {
        return false;
      }
    }

    session.messages.push({
      role: 'user',
      speaker: normalizeText(event?.sender?.card || event?.sender?.nickname || event?.user_id || context?.userId || '用户'),
      text,
      createdAt: nowIso()
    });
    session.messages = session.messages.slice(-24);
    session.updatedAt = nowIso();
    this.stateStore.setIssueRepairSession(session);
    await this.stateStore.save();

    if (session.status === 'waiting-user-feedback' && await this.#isSatisfied(session)) {
      await this.#publishAndClose(session, context, event.message_id);
      return true;
    }

    if (this.runningSessions.has(session.id)) {
      session.pendingRerun = true;
      this.stateStore.setIssueRepairSession(session);
      await this.stateStore.save();
      await this.napcatClient.replyText(context, event.message_id, '我把这条也带上，当前这轮跑完后继续接。');
      return true;
    }

    session.status = 'running';
    this.stateStore.setIssueRepairSession(session);
    await this.stateStore.save();
    await this.napcatClient.replyText(context, event.message_id, '继续看。');
    void this.#runSession(session.id, 'user-followup');
    return true;
  }

  async #isSatisfied(session) {
    const raw = await this.chatClient.complete([
      {
        role: 'system',
        content: buildSatisfactionPrompt(session)
      },
      {
        role: 'user',
        content: `用户最新消息：${normalizeText(session.messages.at(-1)?.text)}`
      }
    ], {
      model: this.config.satisfactionModel || 'gpt-5.4-mini',
      temperature: 0.1
    });
    const parsed = parseJsonObject(raw) ?? {};
    return parsed.accepted === true;
  }

  async #runSession(sessionId, reason) {
    const current = this.stateStore.getIssueRepairSession(sessionId);
    if (!current || this.runningSessions.has(sessionId)) {
      return;
    }
    const promise = this.#executeSession(current, reason)
      .catch((error) => {
        this.logger.warn(`修复会话执行失败 ${sessionId}: ${error.message}`);
      })
      .finally(() => {
        this.runningSessions.delete(sessionId);
      });
    this.runningSessions.set(sessionId, promise);
    await promise;
  }

  async #executeSession(session, reason) {
    const freshSession = this.stateStore.getIssueRepairSession(session.id);
    if (!freshSession) {
      return;
    }
    const workDir = normalizeText(freshSession?.modInfo?.projectRoot);
    if (!workDir) {
      throw new Error('会话缺少 mod projectRoot');
    }

    const sessionDir = path.join(os.tmpdir(), 'napcat-cain-repair', freshSession.id);
    await ensureDir(sessionDir);
    const promptPath = path.join(sessionDir, `prompt-${Date.now()}.txt`);
    const schemaPath = path.join(sessionDir, 'output-schema.json');
    const outputPath = path.join(sessionDir, `last-message-${Date.now()}.json`);
    await fs.writeFile(schemaPath, JSON.stringify(OUTPUT_SCHEMA, null, 2), 'utf8');
    await fs.writeFile(promptPath, buildCodexPrompt(freshSession, this.codexBridgeInfo), 'utf8');

    const stdoutChunks = [];
    const stderrChunks = [];
    const args = normalizeText(freshSession.codexThreadId)
      ? [
          'exec',
          'resume',
          normalizeText(freshSession.codexThreadId),
          '-m', this.config.model || 'gpt-5.4-high',
          '--dangerously-bypass-approvals-and-sandbox',
          '--skip-git-repo-check',
          '--json',
          '--color', 'never',
          '--output-schema', schemaPath,
          '-o', outputPath,
          '-C', workDir,
          '-'
        ]
      : [
          'exec',
          '-m', this.config.model || 'gpt-5.4-high',
          '--dangerously-bypass-approvals-and-sandbox',
          '--skip-git-repo-check',
          '--json',
          '--color', 'never',
          '--output-schema', schemaPath,
          '-o', outputPath,
          '-C', workDir,
          '-'
        ];
    const child = spawn(this.config.codexCommand || 'codex', args, {
      windowsHide: true,
      stdio: ['pipe', 'pipe', 'pipe']
    });
    const timeoutMs = Math.max(60 * 1000, Number(this.config.codexTimeoutMs ?? DEFAULT_CODEX_TIMEOUT_MS) || DEFAULT_CODEX_TIMEOUT_MS);
    const timer = setTimeout(() => {
      child.kill();
    }, timeoutMs);

    child.stdout.on('data', (chunk) => stdoutChunks.push(Buffer.from(chunk).toString('utf8')));
    child.stderr.on('data', (chunk) => stderrChunks.push(Buffer.from(chunk).toString('utf8')));
    child.stdin.end(await fs.readFile(promptPath, 'utf8'));

    const exitCode = await new Promise((resolve, reject) => {
      child.once('error', reject);
      child.once('exit', (code) => resolve(code ?? 0));
    }).finally(() => clearTimeout(timer));

    const stdoutText = stdoutChunks.join('');
    const stderrText = stderrChunks.join('');
    const threadId = extractThreadIdFromJsonl(stdoutText);
    this.logger.info(`修复会话 ${freshSession.id} (${reason}) Codex 结束: code=${exitCode} stdout=${truncate(stdoutText)} stderr=${truncate(stderrText)}`);

    const latest = this.stateStore.getIssueRepairSession(freshSession.id);
    if (!latest) {
      return;
    }
    if (threadId) {
      latest.codexThreadId = threadId;
    }

    if (exitCode !== 0) {
      latest.status = 'failed';
      latest.updatedAt = nowIso();
      latest.messages.push({
        role: 'assistant',
        speaker: 'Cain',
        text: '这轮修复执行失败了，我这边稍后得重开一轮。',
        createdAt: nowIso()
      });
      this.stateStore.setIssueRepairSession(latest);
      await this.stateStore.save();
      await this.napcatClient.replyText(latest.context, latest.sourceMessageId || latest.botMessageIds?.[0] || '', '这轮修复跑挂了，等我整理一下再继续。');
      return;
    }

    const parsed = parseJsonObject(await fs.readFile(outputPath, 'utf8').catch(() => '')) ?? {};
    const assistantMessage = normalizeText(parsed.assistantMessage);
    latest.issueSummary = normalizeIssueSummary(parsed.issueSummary || latest.issueSummary);
    latest.publishFolderName = normalizeText(parsed.publishFolderName) || latest.publishFolderName || defaultPublishFolderName(latest.modInfo);

    if (normalizeText(parsed.status) === 'artifact_ready') {
      latest.latestArtifactPath = normalizeText(parsed.artifactPath);
      latest.latestArtifactName = normalizeText(parsed.artifactName) || path.basename(latest.latestArtifactPath);
      latest.latestArtifactVersion = normalizeText(parsed.artifactVersion);
      latest.status = 'waiting-user-feedback';
      if (assistantMessage) {
        latest.messages.push({ role: 'assistant', speaker: 'Cain', text: assistantMessage, createdAt: nowIso() });
      }
      this.stateStore.setIssueRepairSession(latest);
      await this.stateStore.save();
      await this.#sendArtifactForFeedback(latest);
    } else if (normalizeText(parsed.status) === 'needs_user_reply') {
      latest.status = 'waiting-user-input';
      if (assistantMessage) {
        latest.messages.push({ role: 'assistant', speaker: 'Cain', text: assistantMessage, createdAt: nowIso() });
      }
      this.stateStore.setIssueRepairSession(latest);
      await this.stateStore.save();
      const results = await this.napcatClient.sendContextMessage(latest.context, assistantMessage || '我还差一点信息，你补一句。');
      latest.botMessageIds = extractMessageIds(results);
      this.stateStore.setIssueRepairSession(latest);
      await this.stateStore.save();
    } else if (normalizeText(parsed.status) === 'done') {
      latest.status = 'completed';
      if (assistantMessage) {
        latest.messages.push({ role: 'assistant', speaker: 'Cain', text: assistantMessage, createdAt: nowIso() });
      }
      this.stateStore.setIssueRepairSession(latest);
      await this.stateStore.save();
      if (assistantMessage) {
        const results = await this.napcatClient.sendContextMessage(latest.context, assistantMessage);
        latest.botMessageIds = extractMessageIds(results);
        this.stateStore.setIssueRepairSession(latest);
        await this.stateStore.save();
      }
    } else {
      latest.status = 'failed';
      if (assistantMessage) {
        latest.messages.push({ role: 'assistant', speaker: 'Cain', text: assistantMessage, createdAt: nowIso() });
      }
      this.stateStore.setIssueRepairSession(latest);
      await this.stateStore.save();
      const results = await this.napcatClient.sendContextMessage(latest.context, assistantMessage || '现在还没法继续往下修。');
      latest.botMessageIds = extractMessageIds(results);
      this.stateStore.setIssueRepairSession(latest);
      await this.stateStore.save();
    }

    const rerunSession = this.stateStore.getIssueRepairSession(freshSession.id);
    if (rerunSession?.pendingRerun === true && !['completed', 'failed'].includes(normalizeText(rerunSession.status))) {
      rerunSession.pendingRerun = false;
      rerunSession.status = 'running';
      rerunSession.updatedAt = nowIso();
      this.stateStore.setIssueRepairSession(rerunSession);
      await this.stateStore.save();
      void this.#runSession(rerunSession.id, 'queued-followup');
    }
  }

  async #sendArtifactForFeedback(session) {
    if (!session.latestArtifactPath) {
      return;
    }
    if (normalizeText(session.context?.messageType) === 'group') {
      const result = await this.napcatClient.sendLocalFileToContext(session.context, {
        filePath: session.latestArtifactPath,
        fileName: session.latestArtifactName
      });
      const messageResult = await this.napcatClient.sendGroupMessage(session.context.groupId, [
        {
          type: 'at',
          data: { qq: session.targetUserId }
        },
        {
          type: 'text',
          data: {
            text: ` ${normalizeText(session.messages.at(-1)?.text) || '先试这个构建，看问题还在不在。'}`
          }
        }
      ]);
      session.botMessageIds = extractMessageIds([result, messageResult]);
      this.stateStore.setIssueRepairSession(session);
      await this.stateStore.save();
      return;
    }
    const fileResult = await this.napcatClient.sendLocalFileToContext(session.context, {
      filePath: session.latestArtifactPath,
      fileName: session.latestArtifactName
    });
    const messageResult = await this.napcatClient.sendContextMessage(
      session.context,
      normalizeText(session.messages.at(-1)?.text) || '这版我已经打出来了，你直接试这个。'
    );
    session.botMessageIds = extractMessageIds([fileResult, messageResult]);
    this.stateStore.setIssueRepairSession(session);
    await this.stateStore.save();
  }

  async #publishAndClose(session, context, replyToMessageId) {
    if (!session.latestArtifactPath) {
      session.status = 'completed';
      this.stateStore.setIssueRepairSession(session);
      await this.stateStore.save();
      return;
    }
    const publishGroupId = normalizeText(this.config.publishGroupId);
    const folderName = normalizeText(session.publishFolderName) || defaultPublishFolderName(session.modInfo);
    await this.napcatClient.sendLocalFileToGroup({
      groupId: publishGroupId,
      filePath: session.latestArtifactPath,
      fileName: session.latestArtifactName,
      folderName,
      notifyText: `${session.modInfo.displayName || session.modInfo.projectFolderName} 已确认可用，已同步当前修复产物。`
    });
    session.status = 'completed';
    session.updatedAt = nowIso();
    this.stateStore.setIssueRepairSession(session);
    await this.stateStore.save();
    await this.napcatClient.replyText(
      context,
      replyToMessageId,
      `那我就按这版收口，已经同步到 188709300 的 ${folderName} 里了。`
    );
  }
}
