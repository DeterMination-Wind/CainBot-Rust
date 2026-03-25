import fs from 'node:fs/promises';
import path from 'node:path';

import { extractFileSegments } from './message-attachment-reader.mjs';
import {
  countMessageSegments,
  extractImageSegments,
  isNonEmptyString,
  normalizeMessageSegments,
  pathExists,
  plainTextFromMessage
} from './utils.mjs';

function estimateContentChars(content) {
  if (typeof content === 'string') {
    return content.length;
  }
  if (content == null) {
    return 0;
  }
  try {
    return JSON.stringify(content).length;
  } catch {
    return String(content).length;
  }
}

function estimateMessageChars(messages) {
  return messages.reduce((total, item) => {
    const role = String(item?.role ?? '');
    return total + role.length + estimateContentChars(item?.content);
  }, 0);
}

function getDateTimeText(dateValue) {
  const date = dateValue ? new Date(dateValue) : new Date();
  if (!Number.isFinite(date.getTime())) {
    return '';
  }
  return new Intl.DateTimeFormat('zh-CN', {
    timeZone: 'Asia/Shanghai',
    hour12: false,
    month: '2-digit',
    day: '2-digit',
    hour: '2-digit',
    minute: '2-digit'
  }).format(date);
}

function extractBalancedJsonObject(text) {
  const source = String(text ?? '').trim();
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
      if (depth === 0) {
        continue;
      }
      depth -= 1;
      if (depth === 0 && startIndex >= 0) {
        return source.slice(startIndex, index + 1);
      }
    }
  }

  return '';
}

function parseJsonObject(text, fallback) {
  try {
    const parsed = JSON.parse(extractBalancedJsonObject(text));
    return parsed && typeof parsed === 'object' ? parsed : fallback;
  } catch {
    return fallback;
  }
}

function buildSessionLabel(context) {
  if (context.messageType === 'group') {
    return `群 ${context.groupId}`;
  }
  return `私聊 ${context.userId}`;
}

function buildMessageSummary(message, rawMessage = '') {
  const text = plainTextFromMessage(message, rawMessage).replace(/\s+/g, ' ').trim();
  const imageCount = extractImageSegments(message).length;
  const fileCount = extractFileSegments(message).length;
  const atCount = countMessageSegments(message, 'at');
  const faceCount = countMessageSegments(message, 'face');
  const replyCount = countMessageSegments(message, 'reply');
  const tags = [];

  if (imageCount > 0) {
    tags.push(`图片${imageCount}张`);
  }
  if (fileCount > 0) {
    tags.push(`文件${fileCount}个`);
  }
  if (atCount > 0) {
    tags.push(`at${atCount}次`);
  }
  if (faceCount > 0) {
    tags.push(`表情${faceCount}个`);
  }
  if (replyCount > 0) {
    tags.push(`引用${replyCount}次`);
  }

  const parts = [];
  if (text) {
    parts.push(text.slice(0, 360));
  }
  if (tags.length > 0) {
    parts.push(`[${tags.join('，')}]`);
  }

  return parts.join(' ').trim() || '(无可读文本，可能主要是图片、文件或表情)';
}

function buildTimelineLine(entry, index) {
  const speaker = String(entry?.sender ?? entry?.userId ?? (entry?.role === 'assistant' ? 'Cain' : '群友')).trim() || '群友';
  const prefix = `${index + 1}. [${getDateTimeText(entry?.time || entry?.createdAt)}] ${speaker}`;
  return `${prefix}：${String(entry?.text ?? '').trim() || '(空消息)'}`;
}

function buildTimelineBlock(messages, maxMessages = 30) {
  const items = Array.isArray(messages) ? messages.slice(-maxMessages) : [];
  if (items.length === 0) {
    return '(暂无共享上下文)';
  }
  return items.map((item, index) => buildTimelineLine(item, index)).join('\n');
}

function normalizeFilterDecision(raw) {
  const parsed = parseJsonObject(raw, {});
  return {
    shouldPrompt: parsed?.should_prompt === true || parsed?.shouldPrompt === true,
    reason: String(parsed?.reason ?? '').trim()
  };
}

function normalizePromptReview(raw) {
  const parsed = parseJsonObject(raw, {});
  return {
    approved: parsed?.approved !== false,
    prompt: String(parsed?.prompt ?? '').trim(),
    reason: String(parsed?.reason ?? '').trim()
  };
}

function normalizeTopicClosure(raw) {
  const parsed = parseJsonObject(raw, {});
  return {
    shouldEnd: parsed?.should_end === true || parsed?.shouldEnd === true,
    reason: String(parsed?.reason ?? '').trim()
  };
}

function normalizeMemoryCaptureDecision(raw) {
  const parsed = parseJsonObject(raw, {});
  return {
    shouldAppend: parsed?.should_append === true || parsed?.shouldAppend === true,
    memory: String(parsed?.memory ?? parsed?.entry ?? parsed?.content ?? '').trim(),
    reason: String(parsed?.reason ?? '').trim()
  };
}

function looksLikeCorrectionCandidate(text) {
  const normalized = String(text ?? '').replace(/\s+/g, ' ').trim();
  if (!normalized || normalized.length < 4) {
    return false;
  }
  if (/(说错|讲错|不对|不是|纠正|更正|其实|应该|应为|而是|正确|是指|指的是|意思是)/i.test(normalized)) {
    return true;
  }
  return /(倍速|单位生产|工厂不会加速|原版|x端|mindustryx|release|tag|版本|pc|电脑版|桌面版|apk|jar|exe)/i.test(normalized);
}

function getSenderName(event) {
  return String(
    event?.sender?.card
    || event?.sender?.nickname
    || event?.user_id
    || '群友'
  ).trim() || '群友';
}

function summarizeToolRequest(toolRequest) {
  const tool = String(toolRequest?.tool ?? '').trim() || 'unknown';
  switch (tool) {
    case 'read_codex_file':
      return `${tool} path=${String(toolRequest?.path ?? '').trim() || '(empty)'} lines=${toolRequest?.start_line ?? 1}-${toolRequest?.end_line ?? 'auto'} maxChars=${toolRequest?.max_chars ?? 12000}`;
    case 'subagent_codex_lookup':
      return `${tool} path=${String(toolRequest?.path ?? '.').trim() || '.'} question=${JSON.stringify(String(toolRequest?.question ?? toolRequest?.query ?? '').trim())} maxResults=${toolRequest?.max_results ?? 4}`;
    case 'search_codex_files':
      return `${tool} query=${JSON.stringify(String(toolRequest?.query ?? '').trim())} limit=${toolRequest?.limit ?? 10}`;
    case 'list_codex_directory':
      return `${tool} path=${String(toolRequest?.path ?? '.').trim() || '.'} maxEntries=${toolRequest?.max_entries ?? 50}`;
    case 'inspect_codex_project':
      return `${tool} project=${JSON.stringify(String(toolRequest?.project ?? toolRequest?.query ?? '').trim())} path=${JSON.stringify(String(toolRequest?.path ?? '').trim())} maxFiles=${toolRequest?.max_files ?? 6}`;
    case 'read_bot_memory':
      return `${tool} maxChars=${toolRequest?.max_chars ?? 16000}`;
    case 'append_bot_memory':
      return `${tool} memory=${JSON.stringify(String(toolRequest?.memory ?? toolRequest?.entry ?? toolRequest?.content ?? '').trim())}`;
    case 'send_prompt_image':
      return `${tool} name=${JSON.stringify(String(toolRequest?.name ?? toolRequest?.file ?? toolRequest?.image ?? '').trim())}`;
    case 'search_group_emotes':
      return `${tool} query=${JSON.stringify(String(toolRequest?.query ?? '').trim())} limit=${toolRequest?.limit ?? 6}`;
    case 'send_group_emote':
      return `${tool} query=${JSON.stringify(String(toolRequest?.query ?? toolRequest?.name ?? toolRequest?.file ?? toolRequest?.path ?? toolRequest?.relativePath ?? '').trim())}`;
    case 'read_recent_chat_messages':
    case 'read_group_chat_messages':
      return `${tool} count=${toolRequest?.count ?? (tool === 'read_recent_chat_messages' ? 20 : 100)}`;
    case 'start_group_file_download':
      return `${tool} repoChoice=${JSON.stringify(String(toolRequest?.repo_choice ?? '').trim())} version=${JSON.stringify(String(toolRequest?.version_query ?? toolRequest?.version ?? '').trim())} platform=${JSON.stringify(String(toolRequest?.platform_hint ?? toolRequest?.platform ?? '').trim())}`;
    case 'read_github_repo_releases':
      return `${tool} repo=${JSON.stringify(String(toolRequest?.repo ?? toolRequest?.repository ?? toolRequest?.url ?? '').trim())} maxReleases=${toolRequest?.max_releases ?? 10}`;
    case 'read_github_repo_commits':
      return `${tool} repo=${JSON.stringify(String(toolRequest?.repo ?? toolRequest?.repository ?? toolRequest?.url ?? '').trim())} ref=${JSON.stringify(String(toolRequest?.sha ?? toolRequest?.branch ?? toolRequest?.ref ?? '').trim())} maxCommits=${toolRequest?.max_commits ?? 50}`;
    default:
      return `${tool} ${JSON.stringify(toolRequest)}`;
  }
}

function summarizeToolResult(toolResult) {
  const tool = String(toolResult?.tool ?? '').trim() || 'unknown';
  if (toolResult?.error) {
    return `${tool} error=${toolResult.error}`;
  }
  switch (tool) {
    case 'read_codex_file':
      return `${tool} path=${toolResult?.path ?? '(unknown)'} lines=${toolResult?.start_line ?? '?'}-${toolResult?.end_line ?? '?'} truncated=${toolResult?.truncated === true}`;
    case 'subagent_codex_lookup':
      return `${tool} path=${toolResult?.path ?? '(unknown)'} mode=${toolResult?.mode ?? '(unknown)'} returned=${toolResult?.returned_results ?? 0}`;
    case 'search_codex_files':
      return `${tool} query=${JSON.stringify(String(toolResult?.query ?? '').trim())} results=${toolResult?.returned_results ?? 0} scanned=${toolResult?.scanned_files ?? 0}`;
    case 'list_codex_directory':
      return `${tool} path=${toolResult?.path ?? '(unknown)'} entries=${toolResult?.returned_entries ?? 0}/${toolResult?.total_entries ?? 0}`;
    case 'inspect_codex_project':
      return `${tool} selected=${toolResult?.selected_path ?? '(unknown)'} type=${toolResult?.selected_type ?? '(unknown)'} contextFiles=${Array.isArray(toolResult?.context_files) ? toolResult.context_files.length : 0}`;
    case 'read_bot_memory':
      return `${tool} path=${toolResult?.path ?? '(unknown)'} truncated=${toolResult?.truncated === true}`;
    case 'append_bot_memory':
      return `${tool} appended=${toolResult?.appended === true} memory=${JSON.stringify(String(toolResult?.memory ?? '').trim())}`;
    case 'send_prompt_image':
    case 'send_group_emote':
      return `${tool} sent=${toolResult?.sent === true} file=${toolResult?.file ?? '(unknown)'}`;
    case 'search_group_emotes':
      return `${tool} query=${JSON.stringify(String(toolResult?.query ?? '').trim())} returned=${toolResult?.returnedCount ?? 0}`;
    case 'read_recent_chat_messages':
    case 'read_group_chat_messages':
      return `${tool} returned=${toolResult?.returnedCount ?? 0} requested=${toolResult?.requestedCount ?? 0}`;
    case 'start_group_file_download':
      return `${tool} started=${toolResult?.started === true} state=${toolResult?.state ?? '(none)'} releaseTag=${toolResult?.release_tag ?? '(none)'}`;
    case 'read_github_repo_releases':
      return `${tool} repo=${toolResult?.repo?.full_name ?? '(unknown)'} returned=${toolResult?.returnedCount ?? 0} latestTag=${toolResult?.latestTag ?? '(none)'}`;
    case 'read_github_repo_commits':
      return `${tool} repo=${toolResult?.repo?.full_name ?? '(unknown)'} returned=${toolResult?.returnedCount ?? 0} ref=${toolResult?.ref ?? '(default)'}`;
    default:
      return `${tool} ok`;
  }
}

function looksLikeMindustryQuestion(text) {
  const normalized = String(text ?? '').trim();
  if (!normalized) {
    return false;
  }
  return /(mindustry|mindustryx|mdt|牡丹亭|datapatch|content|方块|建筑|炮塔|工厂|单位|物品|液体|状态|星球|天气|地图|mod|模组|字段|继承|类|电弧硅炉|硅炉|电弧)/i.test(normalized);
}

function looksLikeCodexRoutingQuestion(text) {
  const normalized = String(text ?? '').trim();
  if (!normalized) {
    return false;
  }
  return /(模组|mod|插件|plugin|脚本|script|源码|源代码|仓库|repo|github|项目|工程|目录|文件夹|构建|编译|gradle|java|kotlin|报错|报异常|堆栈|服务端|服务器|scriptagent|shenyu|betterhotkey|neon|mindustryjav modtemplate|template)/i.test(normalized);
}

function extractKnowledgeTokens(text) {
  const source = String(text ?? '').replace(/\s+/g, ' ').trim();
  if (!source) {
    return [];
  }
  const rawTokens = source.match(/[\p{Script=Han}]{2,}|[A-Za-z0-9_.:-]{3,}/gu) ?? [];
  const stopwords = new Set(['mindustry', 'mindustryx', 'datapatch', 'content', '字段', '结构', '对应', '准确', '问题', '现在', '这个', '那个', '什么', '怎么', '一下']);
  const seen = new Set();
  const tokens = [];
  const pushToken = (value) => {
    const token = String(value ?? '').trim();
    if (!token) {
      return;
    }
    const normalized = token.toLowerCase();
    if (!normalized || seen.has(normalized) || stopwords.has(normalized)) {
      return;
    }
    seen.add(normalized);
    tokens.push(token);
  };

  for (const token of rawTokens) {
    if (/^[\p{Script=Han}]+$/u.test(token) && token.length > 4) {
      for (let size = Math.min(6, token.length); size >= 2; size -= 1) {
        for (let index = 0; index + size <= token.length; index += 1) {
          pushToken(token.slice(index, index + size));
          if (tokens.length >= 12) {
            return tokens;
          }
        }
      }
      continue;
    }
    pushToken(token);
    if (tokens.length >= 12) {
      return tokens;
    }
  }
  return tokens.slice(0, 12);
}

function scoreKnowledgeEntry(entry, tokens) {
  const searchable = JSON.stringify(entry).toLowerCase();
  let score = 0;
  for (const token of tokens) {
    const normalized = String(token).toLowerCase();
    if (!normalized) {
      continue;
    }
    if (searchable.includes(normalized)) {
      score += Math.max(8, normalized.length * 4);
    }
  }
  return score;
}

function formatJsonSnippet(value, maxChars = 2400) {
  const text = JSON.stringify(value, null, 2);
  if (text.length <= maxChars) {
    return text;
  }
  return `${text.slice(0, maxChars)}\n...(已截断)`;
}

export class ChatSessionManager {
  constructor(config, chatClient, stateStore, logger, codexTools = null, runtimeConfigStore = null, localRagRetriever = null) {
    this.config = config;
    this.chatClient = chatClient;
    this.stateStore = stateStore;
    this.logger = logger;
    this.codexTools = codexTools;
    this.runtimeConfigStore = runtimeConfigStore;
    this.localRagRetriever = localRagRetriever;
    this.pendingBySession = new Map();
    this.mindustryKnowledgeCache = null;
    this.codexFolderGuideCache = null;
    this.correctionMemoryChecked = new Set();
  }

  buildSessionKey(context) {
    if (context.messageType === 'group') {
      return `qa:group:${context.groupId}`;
    }
    return `qa:private:${context.userId}`;
  }

  isGroupEnabled(groupId) {
    return this.runtimeConfigStore?.isQaGroupEnabled(groupId, this.config.enabledGroupIds) === true;
  }

  isGroupProactiveReplyEnabled(groupId) {
    return this.runtimeConfigStore?.isQaGroupProactiveReplyEnabled(groupId, this.config.enabledGroupIds) === true;
  }

  getGroupPromptStatus(groupId) {
    const override = this.runtimeConfigStore?.getGroupQaOverride(groupId) ?? null;
    return {
      groupId: String(groupId ?? '').trim(),
      enabled: this.isGroupEnabled(groupId),
      proactiveReplyEnabled: this.isGroupProactiveReplyEnabled(groupId),
      fileDownloadEnabled: this.runtimeConfigStore?.isQaGroupFileDownloadEnabled(groupId) === true,
      fileDownloadFolderName: this.runtimeConfigStore?.getQaGroupFileDownloadFolderName(groupId) || '',
      filterPrompt: override?.filterPrompt || this.config.filter.prompt,
      answerPrompt: override?.answerPrompt || this.config.answer.systemPrompt
    };
  }

  async recordIncomingMessage(context, event, options = {}) {
    const sessionKey = this.buildSessionKey(context);
    const summary = String(options?.summary ?? buildMessageSummary(event?.message, event?.raw_message)).trim();
    const entry = {
      role: 'user',
      kind: options?.kind ?? 'group-message',
      messageId: String(event?.message_id ?? '').trim(),
      userId: String(context?.userId ?? '').trim(),
      sender: getSenderName(event),
      text: summary,
      rawText: plainTextFromMessage(event?.message, event?.raw_message),
      time: Number(event?.time ?? 0) > 0 ? new Date(Number(event.time) * 1000).toISOString() : new Date().toISOString(),
      createdAt: new Date().toISOString()
    };
    this.stateStore.appendChatSessionEntry(sessionKey, entry, this.config.answer.maxTimelineMessages);
    await this.stateStore.save();
    return entry;
  }

  async markHinted(context, messageId) {
    const sessionKey = this.buildSessionKey(context);
    this.stateStore.setChatSessionHintedMessage(sessionKey, messageId);
    await this.stateStore.save();
  }

  async chat(context, input) {
    const sessionKey = this.buildSessionKey(context);
    return await this.#runExclusive(sessionKey, async () => {
      const session = this.stateStore.getChatSession(sessionKey);
      const normalizedInput = this.#normalizeInput(input);
      if (!normalizedInput.historyText) {
        throw new Error('聊天内容不能为空');
      }

      const userEntry = {
        role: 'user',
        kind: context.messageType === 'group' ? 'direct-question' : 'private-question',
        messageId: String(normalizedInput.runtimeContext?.currentMessageId ?? '').trim(),
        userId: String(context.userId ?? '').trim(),
        sender: normalizedInput.runtimeContext?.senderName || context.userId || '用户',
        text: String(normalizedInput.runtimeContext?.timelineText ?? normalizedInput.historyText).slice(0, 600),
        time: normalizedInput.runtimeContext?.currentTime
          ? new Date(Number(normalizedInput.runtimeContext.currentTime) * 1000).toISOString()
          : new Date().toISOString(),
        createdAt: new Date().toISOString()
      };
      this.stateStore.appendChatSessionEntry(sessionKey, userEntry, this.config.answer.maxTimelineMessages);

      const timelineText = buildTimelineBlock(
        this.stateStore.getChatSession(sessionKey).messages,
        this.config.answer.contextWindowMessages
      );
      const systemPrompt = await this.#buildAnswerSystemPrompt(context);
      const ragPrompt = await this.#maybeBuildRagPrompt(normalizedInput.historyText);
      const codexFolderGuidePrompt = await this.#maybeBuildCodexFolderGuidePrompt(normalizedInput.historyText);
      const mindustryPrompt = await this.#maybeBuildMindustryKnowledgePrompt(normalizedInput.historyText);
      const userContent = [
        '以下是当前共享上下文：',
        timelineText,
        '',
        '以下是本次需要你回答的请求：',
        normalizedInput.text
      ].join('\n');

      const requestMessages = [
        { role: 'system', content: systemPrompt },
        ...(ragPrompt ? [{ role: 'system', content: ragPrompt }] : []),
        ...(codexFolderGuidePrompt ? [{ role: 'system', content: codexFolderGuidePrompt }] : []),
        ...(mindustryPrompt ? [{ role: 'system', content: mindustryPrompt }] : []),
        { role: 'user', content: normalizedInput.images.length > 0 ? this.#buildMultimodalUserContent(userContent, normalizedInput.images) : userContent }
      ];

      const completion = await this.#completeWithReadonlyTools(context, normalizedInput.runtimeContext, requestMessages);
      const assistantEntry = {
        role: 'assistant',
        kind: completion.notice === 'group-file-download-started' ? 'tool-handoff' : 'answer',
        messageId: '',
        userId: String(context.selfId ?? '').trim(),
        sender: 'Cain',
        text: completion.text || (completion.notice === 'group-file-download-started' ? '[已转交群文件下载流程]' : ''),
        time: new Date().toISOString(),
        createdAt: new Date().toISOString()
      };
      if (assistantEntry.text) {
        this.stateStore.appendChatSessionEntry(sessionKey, assistantEntry, this.config.answer.maxTimelineMessages);
      }
      await this.stateStore.save();
      return {
        text: completion.text,
        notice: completion.notice || ''
      };
    });
  }

  async shouldSuggestReply(context, event) {
    const sessionKey = this.buildSessionKey(context);
    return await this.#runExclusive(`${sessionKey}:filter`, async () => {
      const session = this.stateStore.getChatSession(sessionKey);
      if (String(session.lastHintedMessageId ?? '') === String(event?.message_id ?? '')) {
        return { shouldPrompt: false, reason: 'already-hinted' };
      }

      const summary = buildMessageSummary(event?.message, event?.raw_message);
      const recentContext = buildTimelineBlock(
        Array.isArray(session.messages) ? session.messages : [],
        Math.min(Number(this.config.answer.contextWindowMessages ?? 30) || 30, 12)
      );
      const raw = await this.chatClient.complete([
        {
          role: 'system',
          content: [
            this.#getEffectiveFilterPrompt(context.groupId),
            '你只负责判断是否值得提醒群友可以使用 Cain 来问。',
            '只输出 JSON：{"should_prompt":boolean,"reason":"简短原因"}。'
          ].join('\n\n')
        },
        {
          role: 'user',
          content: [
            `群号：${context.groupId}`,
            `发送者：${getSenderName(event)} (${context.userId || '-'})`,
            `当前消息摘要：${summary}`,
            '',
            '最近共享上下文（仅用于判断这是不是同一游戏话题的延续）：',
            recentContext
          ].join('\n')
        }
      ], {
        model: this.config.filter.model,
        temperature: 0.1
      });

      return normalizeFilterDecision(raw);
    });
  }

  async maybeCloseGroupTopic(groupId) {
    const context = {
      messageType: 'group',
      groupId: String(groupId ?? '').trim(),
      userId: '',
      selfId: ''
    };
    const sessionKey = this.buildSessionKey(context);
    return await this.#runExclusive(`${sessionKey}:topic-close`, async () => {
      const session = this.stateStore.getChatSession(sessionKey);
      const messages = Array.isArray(session.messages) ? session.messages.slice(-this.config.topicClosure.messageWindow) : [];
      if (messages.length === 0) {
        return { shouldEnd: true, reason: 'empty-session' };
      }
      const raw = await this.chatClient.complete([
        {
          role: 'system',
          content: this.config.topicClosure.systemPrompt
        },
        {
          role: 'user',
          content: [
            `群号：${context.groupId}`,
            '最近消息：',
            buildTimelineBlock(messages, this.config.topicClosure.messageWindow)
          ].join('\n\n')
        }
      ], {
        model: this.config.topicClosure.model,
        temperature: this.config.topicClosure.temperature
      });

      const decision = normalizeTopicClosure(raw);
      if (decision.shouldEnd) {
        this.stateStore.clearChatSession(sessionKey);
        await this.stateStore.save();
      }
      return decision;
    });
  }

  async disableGroup(groupId) {
    const normalizedGroupId = String(groupId ?? '').trim();
    if (!normalizedGroupId) {
      throw new Error('groupId 不能为空');
    }
    if (!this.runtimeConfigStore) {
      throw new Error('runtimeConfigStore 未配置，无法关闭群问答');
    }
    await this.runtimeConfigStore.setQaGroupEnabled(normalizedGroupId, false);
    this.stateStore.clearChatSession(this.buildSessionKey({
      messageType: 'group',
      groupId: normalizedGroupId,
      userId: '',
      selfId: ''
    }));
    await this.stateStore.save();
    return this.getGroupPromptStatus(normalizedGroupId);
  }

  async disableGroupProactiveReplies(groupId) {
    const normalizedGroupId = String(groupId ?? '').trim();
    if (!normalizedGroupId) {
      throw new Error('groupId 不能为空');
    }
    if (!this.runtimeConfigStore) {
      throw new Error('runtimeConfigStore 未配置，无法关闭群主动回复');
    }
    await this.runtimeConfigStore.setQaGroupProactiveReplyEnabled(
      normalizedGroupId,
      false,
      this.config.enabledGroupIds
    );
    return this.getGroupPromptStatus(normalizedGroupId);
  }

  async updateFilterPrompt(groupId, instruction) {
    return await this.#reviewAndPersistPrompt(groupId, 'filter', instruction);
  }

  async updateAnswerPrompt(groupId, instruction) {
    return await this.#reviewAndPersistPrompt(groupId, 'answer', instruction);
  }

  async maybeCaptureCorrectionMemory(context, event) {
    if (context?.messageType !== 'group' || !this.codexTools?.appendMemoryEntry) {
      return null;
    }

    const rawText = plainTextFromMessage(event?.message, event?.raw_message);
    if (!looksLikeCorrectionCandidate(rawText)) {
      return null;
    }

    const messageId = String(event?.message_id ?? '').trim();
    const captureKey = `${this.buildSessionKey(context)}:${messageId || rawText.slice(0, 80)}`;
    if (this.correctionMemoryChecked.has(captureKey)) {
      return null;
    }
    this.correctionMemoryChecked.add(captureKey);
    if (this.correctionMemoryChecked.size > 2000) {
      const [firstKey] = this.correctionMemoryChecked;
      if (firstKey) {
        this.correctionMemoryChecked.delete(firstKey);
      }
    }

    try {
      const memoryFile = String(this.codexTools?.memoryFile ?? '').trim();
      if (!memoryFile || !(await pathExists(memoryFile))) {
        return null;
      }

      const sessionKey = this.buildSessionKey(context);
      const session = this.stateStore.getChatSession(sessionKey);
      const allMessages = Array.isArray(session?.messages) ? session.messages : [];
      if (allMessages.length < 2) {
        return null;
      }

      const recentMessages = allMessages.slice(-18);
      let recentAssistantDistance = Number.POSITIVE_INFINITY;
      for (let index = recentMessages.length - 2; index >= 0; index -= 1) {
        if (recentMessages[index]?.role === 'assistant') {
          recentAssistantDistance = recentMessages.length - 1 - index;
          break;
        }
      }
      if (!Number.isFinite(recentAssistantDistance) || recentAssistantDistance > 8) {
        return null;
      }

      const raw = await this.chatClient.complete([
        {
          role: 'system',
          content: [
            '你负责从群聊上下文中判断：Cain 是否刚被群友纠正了一个适合写入长期记忆的事实错误。',
            '只有当最近上下文里确实出现了 Cain 先回答错、随后群友给出更正事实时，should_append 才能为 true。',
            '只保留可长期复用的稳定事实；不要记录闲聊、情绪、一次性事件、个人偏好、时间戳、用户名、群号。',
            '如果当前消息只是补充讨论、玩笑、猜测，或无法确认 Cain 之前说错了，就返回 false。',
            '输出必须是 JSON：{"should_append":boolean,"memory":"简短事实句","reason":"简短原因"}。',
            'memory 最多 40 字，不能为空；如果 should_append=false，则 memory 置空字符串。'
          ].join('\n')
        },
        {
          role: 'user',
          content: [
            `群号：${context.groupId}`,
            `当前消息发送者：${getSenderName(event)} (${context.userId || '-'})`,
            `当前消息：${rawText}`,
            '',
            '最近聊天时间线：',
            buildTimelineBlock(recentMessages, recentMessages.length)
          ].join('\n')
        }
      ], {
        model: this.config.filter.model ?? this.config.answer.model,
        temperature: 0.1
      });

      const decision = normalizeMemoryCaptureDecision(raw);
      if (!decision.shouldAppend || !decision.memory) {
        return null;
      }

      const result = await this.codexTools.appendMemoryEntry(decision.memory, {
        source: 'group-correction',
        groupId: context.groupId,
        messageId
      });
      this.logger.info(`长期记忆${result?.appended === true ? '已新增' : '已存在'}：${decision.memory}${decision.reason ? ` (${decision.reason})` : ''}`);
      return result;
    } catch (error) {
      this.logger.warn(`纠错长期记忆捕获失败：${error.message}`);
      return null;
    }
  }

  #getEffectiveFilterPrompt(groupId) {
    const override = this.runtimeConfigStore?.getGroupQaOverride(groupId);
    return override?.filterPrompt || this.config.filter.prompt;
  }

  async #buildAnswerSystemPrompt(context) {
    const override = context.messageType === 'group'
      ? this.runtimeConfigStore?.getGroupQaOverride(context.groupId)
      : null;
    const basePrompt = override?.answerPrompt || this.config.answer.systemPrompt;
    const parts = [basePrompt];
    if (this.codexTools?.getAlwaysLoadedMemoryPrompt) {
      const memoryPrompt = await this.codexTools.getAlwaysLoadedMemoryPrompt();
      if (memoryPrompt) {
        parts.push(memoryPrompt);
      }
    }
    if (this.localRagRetriever && await this.localRagRetriever.isEnabled()) {
      parts.push(this.localRagRetriever.getPromptInstructions());
    }
    if (this.codexTools && await this.codexTools.isEnabled()) {
      parts.push(this.codexTools.getPromptInstructions());
    }
    return parts.filter(Boolean).join('\n\n');
  }

  async #maybeBuildRagPrompt(text) {
    if (!this.localRagRetriever) {
      return '';
    }
    try {
      const ragResult = await this.localRagRetriever.retrieve(text);
      return String(ragResult?.prompt ?? '').trim();
    } catch (error) {
      this.logger.warn(`本地 RAG 注入失败：${error.message}`);
      return '';
    }
  }

  async #maybeBuildMindustryKnowledgePrompt(text) {
    if (!looksLikeMindustryQuestion(text)) {
      return '';
    }

    try {
      const knowledge = await this.#loadMindustryKnowledge();
      if (!knowledge) {
        return '';
      }

      const tokens = extractKnowledgeTokens(text);
      const instanceCandidates = knowledge.instances
        .map((entry) => ({ entry, score: scoreKnowledgeEntry(entry, tokens) }))
        .filter((item) => item.score > 0)
        .sort((left, right) => right.score - left.score || String(left.entry?.type ?? '').localeCompare(String(right.entry?.type ?? ''), 'zh-CN'))
        .slice(0, 3);

      const schemaByType = new Map(knowledge.compose.map((entry) => [String(entry?.type ?? '').trim(), entry]));
      const schemaCandidates = [];
      const seenSchemaTypes = new Set();

      for (const candidate of instanceCandidates) {
        const extendsType = String(candidate.entry?.extends ?? '').trim();
        const matched = schemaByType.get(extendsType);
        if (matched && !seenSchemaTypes.has(extendsType)) {
          seenSchemaTypes.add(extendsType);
          schemaCandidates.push(matched);
        }
      }

      if (schemaCandidates.length < 2) {
        const additionalSchema = knowledge.compose
          .map((entry) => ({ entry, score: scoreKnowledgeEntry(entry, tokens) }))
          .filter((item) => item.score > 0 && !seenSchemaTypes.has(String(item.entry?.type ?? '').trim()))
          .sort((left, right) => right.score - left.score || String(left.entry?.type ?? '').localeCompare(String(right.entry?.type ?? ''), 'zh-CN'))
          .slice(0, 2 - schemaCandidates.length)
          .map((item) => item.entry);
        for (const entry of additionalSchema) {
          const type = String(entry?.type ?? '').trim();
          if (!type || seenSchemaTypes.has(type)) {
            continue;
          }
          seenSchemaTypes.add(type);
          schemaCandidates.push(entry);
        }
      }

      this.logger.info(
        `Mindustry JSON 自动预读：query=${JSON.stringify(String(text ?? '').slice(0, 120))} tokens=${JSON.stringify(tokens)} ` +
        `instances=${instanceCandidates.map((item) => item.entry.type).join(',') || '(none)'} ` +
        `schemas=${schemaCandidates.map((item) => item.type).join(',') || '(none)'}`
      );

      const lines = [
        '【Mindustry 必读 JSON 已由系统预读】',
        `实例库路径：${knowledge.instancePath}`,
        `结构库路径：${knowledge.composePath}`,
        '下面这些片段已经来自那两个强制要求的本地 JSON；不要再声称“我还没读取到这两个 JSON”。'
      ];

      if (instanceCandidates.length > 0) {
        lines.push('实例库命中条目：');
        instanceCandidates.forEach((item, index) => {
          lines.push(`${index + 1}. score=${item.score}\n${formatJsonSnippet(item.entry, 2200)}`);
        });
      }

      if (schemaCandidates.length > 0) {
        lines.push('结构库相关条目：');
        schemaCandidates.forEach((entry, index) => {
          lines.push(`${index + 1}.\n${formatJsonSnippet(entry, 2200)}`);
        });
      }

      if (instanceCandidates.length === 0 && schemaCandidates.length === 0) {
        lines.push('系统已成功读取这两个 JSON，但当前关键词没有直接命中明显条目。此时你应继续用只读工具精确读取，而不是说自己没读过这两个 JSON。');
      } else {
        lines.push('如果这些片段仍不足以回答完整问题，再继续调用只读工具读取更具体的文件片段。');
      }

      return lines.join('\n\n');
    } catch (error) {
      this.logger.warn(`Mindustry JSON 自动预读失败：${error.message}`);
      return '';
    }
  }

  async #maybeBuildCodexFolderGuidePrompt(text) {
    if (!looksLikeCodexRoutingQuestion(text)) {
      return '';
    }

    try {
      const guide = await this.#loadCodexFolderGuide();
      if (!guide) {
        return '';
      }
      const codexRoot = String(this.config?.answer?.codexRoot ?? '').trim();
      const folderNames = (await fs.readdir(codexRoot, { withFileTypes: true }))
        .filter((entry) => entry?.isDirectory?.())
        .map((entry) => String(entry.name ?? '').trim())
        .filter(Boolean)
        .sort((left, right) => left.localeCompare(right, 'zh-CN'));

      const lines = [
        '【codex 根目录文件夹名称与用途索引已由系统预读】',
        `codex 根目录：${guide.rootPath}`,
        `索引文件：${guide.indexPath}`,
        `当前 codex 根目录文件夹名称：${folderNames.join('、')}`,
        '回答模组、插件、脚本、源码、仓库、项目目录、构建、编译、报错定位、服务端脚本这类问题前，先根据上面的文件夹名称判断应该看哪个目录，再参考下面的索引决定优先读哪个项目。',
        '如果问题已经明显对应某个目录，就优先读取那个目录；不要在没做目录判断前就盲搜整个 codex。',
        '文件夹用途索引：'
      ];

      for (const entry of guide.folders) {
        lines.push(`- ${entry.name}: ${entry.purpose}`);
      }

      lines.push('这份索引已经预读成功；不要再说自己还没看过 codex 根目录。');
      return lines.join('\n');
    } catch (error) {
      this.logger.warn(`codex 根目录索引自动预读失败：${error.message}`);
      return '';
    }
  }

  async #loadMindustryKnowledge() {
    if (this.mindustryKnowledgeCache) {
      return this.mindustryKnowledgeCache;
    }

    const codexRoot = String(this.config?.answer?.codexRoot ?? '').trim();
    if (!codexRoot) {
      return null;
    }

    const composePath = path.resolve(codexRoot, 'compose(MustRead_if_the_questions_are_about_data_patch).json');
    const instancePath = path.resolve(codexRoot, 'mindustryx-content(MustRead_if_the_questions_are_about_mindustry_instances).json');
    if (!(await pathExists(composePath)) || !(await pathExists(instancePath))) {
      return null;
    }

    const [composeText, instanceText] = await Promise.all([
      fs.readFile(composePath, 'utf8'),
      fs.readFile(instancePath, 'utf8')
    ]);
    const compose = JSON.parse(composeText);
    const instances = JSON.parse(instanceText);
    this.mindustryKnowledgeCache = {
      composePath,
      instancePath,
      compose: Array.isArray(compose) ? compose : [],
      instances: Array.isArray(instances) ? instances : []
    };
    return this.mindustryKnowledgeCache;
  }

  async #loadCodexFolderGuide() {
    if (this.codexFolderGuideCache) {
      return this.codexFolderGuideCache;
    }

    const codexRoot = String(this.config?.answer?.codexRoot ?? '').trim();
    if (!codexRoot) {
      return null;
    }

    const indexPath = path.resolve(codexRoot, 'codex-folder-index(MustRead_before_answering_mod_project_questions).json');
    const raw = JSON.parse(await fs.readFile(indexPath, 'utf8'));
    this.codexFolderGuideCache = {
      rootPath: String(raw?.rootPath ?? codexRoot).trim() || codexRoot,
      indexPath,
      folders: Array.isArray(raw?.folders)
        ? raw.folders.map((entry) => ({
          name: String(entry?.name ?? '').trim(),
          purpose: String(entry?.purpose ?? '').trim()
        })).filter((entry) => entry.name && entry.purpose)
        : []
    };
    return this.codexFolderGuideCache;
  }

  #buildMultimodalUserContent(text, images) {
    return [
      { type: 'text', text },
      ...images
    ];
  }

  #normalizeInput(input) {
    if (typeof input === 'string') {
      const text = String(input).trim();
      return {
        text,
        historyText: text,
        images: [],
        runtimeContext: {}
      };
    }

    const text = String(input?.text ?? '').trim();
    const historyText = String(input?.historyText ?? text).trim();
    return {
      text: text || historyText,
      historyText: historyText || text,
      images: Array.isArray(input?.images) ? input.images.filter(Boolean) : [],
      runtimeContext: input?.runtimeContext && typeof input.runtimeContext === 'object'
        ? input.runtimeContext
        : {}
    };
  }

  async #reviewAndPersistPrompt(groupId, type, instruction) {
    const normalizedGroupId = String(groupId ?? '').trim();
    const normalizedInstruction = String(instruction ?? '').trim();
    if (!normalizedGroupId) {
      throw new Error('groupId 不能为空');
    }
    if (!normalizedInstruction) {
      throw new Error('prompt 修改内容不能为空');
    }

    const current = this.getGroupPromptStatus(normalizedGroupId);
    const currentPrompt = type === 'filter' ? current.filterPrompt : current.answerPrompt;
    const raw = await this.chatClient.complete([
      {
        role: 'system',
        content: this.config.promptReview.systemPrompt
      },
      {
        role: 'user',
        content: [
          `群号：${normalizedGroupId}`,
          `目标类型：${type === 'filter' ? '过滤 prompt' : '聊天 prompt'}`,
          `当前 prompt：\n${currentPrompt}`,
          `管理员要求：\n${normalizedInstruction}`
        ].join('\n\n')
      }
    ], {
      model: this.config.promptReview.model,
      temperature: 0.2
    });

    const reviewed = normalizePromptReview(raw);
    if (!reviewed.approved || !reviewed.prompt) {
      throw new Error(reviewed.reason || 'prompt 审核未通过');
    }

    const existing = this.runtimeConfigStore?.getGroupQaOverride(normalizedGroupId) ?? {};
    const payload = {
      groupId: normalizedGroupId,
      filterPrompt: type === 'filter' ? reviewed.prompt : existing.filterPrompt,
      answerPrompt: type === 'answer' ? reviewed.prompt : existing.answerPrompt
    };
    await this.runtimeConfigStore.setGroupQaOverride(payload);
    return {
      groupId: normalizedGroupId,
      type,
      prompt: reviewed.prompt,
      reason: reviewed.reason
    };
  }

  async #completeWithReadonlyTools(context, runtimeContext, messages) {
    const toolEnabled = Boolean(this.codexTools && await this.codexTools.isEnabled());
    const softToolRounds = Number(this.config.answer.maxToolRounds ?? 4) || 4;
    const hardToolRounds = Math.max(softToolRounds, 20);
    const contextBudget = Number(this.config.answer.maxContextChars ?? 80000) || 80000;
    const workingMessages = [...messages];
    const repeatedTruncatedReads = new Map();

    const completeDirectAnswer = async (assistantText, reason = '') => {
      if (assistantText) {
        workingMessages.push({ role: 'assistant', content: assistantText });
      }
      workingMessages.push({
        role: 'user',
        content: [
          reason,
          '不要再调用任何工具。请只基于你已经拿到的信息直接回答用户；如果仍有不确定，就明确说明不确定点。回答不要使用 Markdown。'
        ].filter(Boolean).join('\n')
      });

      const finalText = await this.chatClient.complete(workingMessages, {
        model: this.config.answer.model,
        temperature: this.config.answer.temperature
      });
      return { text: finalText, notice: '' };
    };

    for (let round = 0; round < hardToolRounds; round += 1) {
      const assistantText = await this.chatClient.complete(workingMessages, {
        model: this.config.answer.model,
        temperature: this.config.answer.temperature
      });

      if (!toolEnabled) {
        return { text: assistantText, notice: '' };
      }

      const toolParsing = this.codexTools.parseToolCalls(assistantText);
      if (toolParsing.calls.length === 0) {
        return { text: assistantText, notice: '' };
      }

      if (estimateMessageChars(workingMessages) + estimateContentChars(assistantText) >= Math.max(contextBudget - 4000, Math.floor(contextBudget * 0.9))) {
        return await completeDirectAnswer(assistantText, '系统提示：当前上下文已经接近上限，请停止调用工具并直接回答。');
      }

      if (round >= hardToolRounds - 1) {
        return await completeDirectAnswer(assistantText, '系统提示：你已经连续调用了很多轮工具；请直接基于现有信息回答。');
      }

      const toolRequest = toolParsing.calls[0];
      const warnings = [];
      const readSignature = toolRequest.tool === 'read_codex_file'
        ? JSON.stringify({
          path: String(toolRequest?.path ?? '').trim(),
          start: Number(toolRequest?.start_line ?? 1) || 1,
          end: toolRequest?.end_line ?? 'auto',
          maxChars: Number(toolRequest?.max_chars ?? 12000) || 12000
        })
        : '';
      if (round >= softToolRounds) {
        warnings.push('你已经调用了较多轮工具；如果已有答案，请尽快直接回答用户。');
      }
      if (toolParsing.format === 'legacy') {
        warnings.push('你刚才没有使用约定的工具标记；后续必须使用特殊标记包裹单个工具请求。');
      }
      if (toolParsing.calls.length > 1) {
        warnings.push('你一次返回了多个工具请求；系统只执行第一个，后续必须一次只请求一个工具。');
      }

      this.logger.info(`聊天触发只读工具：${summarizeToolRequest(toolRequest)}`);
      let toolResult;
      if (toolRequest.tool === 'read_codex_file' && readSignature && (repeatedTruncatedReads.get(readSignature) ?? 0) > 0) {
        toolResult = {
          tool: 'read_codex_file',
          path: String(toolRequest?.path ?? '').trim(),
          truncated: true,
          blocked: true,
          error: '同一段已截断内容禁止重复读取；下一步请改用 subagent_codex_lookup，并在 question 中写明你要找的字段、对象或关键词。'
        };
      } else {
        try {
          toolResult = await this.codexTools.execute(toolRequest, { context, ...runtimeContext });
        } catch (error) {
          toolResult = {
            tool: String(toolRequest.tool ?? 'unknown'),
            error: error.message
          };
        }
      }

      if (toolResult?.error) {
        this.logger.warn(`聊天只读工具失败 ${toolRequest.tool}: ${toolResult.error}`);
      } else {
        this.logger.info(`聊天只读工具结果：${summarizeToolResult(toolResult)}`);
      }

      if (toolRequest.tool === 'read_codex_file' && readSignature) {
        if (toolResult?.truncated === true && !toolResult?.blocked) {
          repeatedTruncatedReads.set(readSignature, (repeatedTruncatedReads.get(readSignature) ?? 0) + 1);
          warnings.push('当前文件片段已截断；如果还不够，下一步不要再重复读同一段，必须改用 subagent_codex_lookup 定位具体内容。');
        } else if (toolResult?.truncated !== true) {
          repeatedTruncatedReads.delete(readSignature);
        }
      }

      if (toolRequest.tool === 'start_group_file_download' && toolResult?.started === true) {
        return {
          text: '',
          notice: 'group-file-download-started'
        };
      }

      workingMessages.push({ role: 'assistant', content: assistantText });
      workingMessages.push({
        role: 'user',
        content: [
          warnings.length > 0 ? `工具调用警告：${warnings.join(' ')}` : '',
          '以下是你请求的只读文件工具结果（只读、不可修改）：',
          this.codexTools.formatToolResult(toolResult),
          '如果信息已经足够，请直接回答用户；如果还不够，可以继续再请求一个工具。'
        ].filter(Boolean).join('\n')
      });
    }

    return await completeDirectAnswer('', '系统提示：请基于现有信息直接回答用户。');
  }

  async #runExclusive(sessionKey, task) {
    const previous = this.pendingBySession.get(sessionKey) ?? Promise.resolve();
    const current = previous.then(task, task);
    const tail = current.catch(() => {});
    this.pendingBySession.set(sessionKey, tail);
    try {
      return await current;
    } finally {
      if (this.pendingBySession.get(sessionKey) === tail) {
        this.pendingBySession.delete(sessionKey);
      }
    }
  }
}
