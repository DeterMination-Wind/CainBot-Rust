import fs from 'node:fs/promises';
import path from 'node:path';
import process from 'node:process';
import readline from 'node:readline';

import { loadConfig } from '../src/config.mjs';
import { RuntimeConfigStore } from '../src/runtime-config-store.mjs';
import { OpenAiChatClient } from '../src/openai-chat-client.mjs';
import { ChatSessionManager } from '../src/chat-session-manager.mjs';
import { CodexReadonlyTools } from '../src/codex-readonly-tools.mjs';
import { LocalRagRetriever } from '../src/local-rag-retriever.mjs';
import { NapCatClient } from '../src/napcat-client.mjs';

function writeStderr(...args) {
  const line = args.map((item) => {
    if (typeof item === 'string') {
      return item;
    }
    try {
      return JSON.stringify(item);
    } catch {
      return String(item);
    }
  }).join(' ');
  process.stderr.write(`${line}\n`);
}

function createWorkerLogger() {
  const log = (level, ...args) => {
    writeStderr(`[qa-session-worker] [${level}]`, ...args);
  };
  return {
    debug: (...args) => log('debug', ...args),
    info: (...args) => log('info', ...args),
    warn: (...args) => log('warn', ...args),
    error: (...args) => log('error', ...args),
    flush: async () => {},
    setNonInfoNotifier: () => {}
  };
}

function normalizeSession(session) {
  const source = session && typeof session === 'object' ? session : {};
  return {
    messages: Array.isArray(source.messages) ? source.messages : [],
    updatedAt: String(source.updatedAt ?? ''),
    lastActivityAt: String(source.lastActivityAt ?? source.updatedAt ?? ''),
    lastHintedMessageId: String(source.lastHintedMessageId ?? '')
  };
}

function normalizeChatSessions(value) {
  const source = value && typeof value === 'object' ? value : {};
  const entries = Object.entries(source)
    .map(([key, session]) => [String(key ?? '').trim(), normalizeSession(session)])
    .filter(([key]) => key);
  return Object.fromEntries(entries);
}

class MergingChatStateStore {
  constructor(filePath) {
    this.filePath = filePath;
    this.journalPath = filePath.replace(/(\.[^./\\]+)?$/, '.journal.jsonl');
    this.chatSessions = {};
  }

  async load() {
    try {
      const text = await fs.readFile(this.filePath, 'utf8');
      const parsed = JSON.parse(text);
      this.chatSessions = normalizeChatSessions(parsed?.chatSessions);
    } catch (error) {
      if (error?.code !== 'ENOENT') {
        throw error;
      }
      this.chatSessions = {};
    }
    try {
      const journalText = await fs.readFile(this.journalPath, 'utf8');
      const lines = journalText.split(/\r?\n/).map((line) => line.trim()).filter(Boolean);
      for (const line of lines) {
        const op = JSON.parse(line);
        this.#applyJournalOp(op);
      }
    } catch (error) {
      if (error?.code !== 'ENOENT') {
        throw error;
      }
    }
  }

  getChatSession(sessionKey) {
    const normalizedKey = String(sessionKey ?? '').trim();
    if (!normalizedKey) {
      throw new Error('sessionKey 不能为空');
    }
    if (!this.chatSessions[normalizedKey]) {
      this.chatSessions[normalizedKey] = normalizeSession({});
    }
    this.chatSessions[normalizedKey] = normalizeSession(this.chatSessions[normalizedKey]);
    return this.chatSessions[normalizedKey];
  }

  appendChatSessionEntry(sessionKey, entry, maxMessages = 80) {
    const session = this.getChatSession(sessionKey);
    session.messages.push(entry);
    if (session.messages.length > maxMessages) {
      session.messages = session.messages.slice(-maxMessages);
    }
    const timestamp = new Date().toISOString();
    session.updatedAt = timestamp;
    session.lastActivityAt = timestamp;
    return session;
  }

  setChatSessionHintedMessage(sessionKey, messageId) {
    const session = this.getChatSession(sessionKey);
    session.lastHintedMessageId = String(messageId ?? '').trim();
    session.updatedAt = new Date().toISOString();
    return session;
  }

  clearChatSession(sessionKey) {
    delete this.chatSessions[String(sessionKey ?? '').trim()];
  }

  async save() {
    await fs.mkdir(path.dirname(this.filePath), { recursive: true });
    let merged = {};
    try {
      merged = JSON.parse(await fs.readFile(this.filePath, 'utf8'));
    } catch (error) {
      if (error?.code !== 'ENOENT') {
        throw error;
      }
    }
    const normalized = merged && typeof merged === 'object' ? merged : {};
    normalized.version = Number(normalized.version ?? 6) || 6;
    normalized.chatSessions = this.chatSessions;
    await fs.writeFile(this.filePath, JSON.stringify(normalized, null, 2), 'utf8');
    await fs.rm(this.journalPath, { force: true }).catch(() => {});
  }

  #applyJournalOp(op) {
    const action = String(op?.op ?? '').trim();
    if (action === 'append_chat_session_entry') {
      this.appendChatSessionEntry(op?.session_key, op?.entry, Number(op?.max_messages ?? 80) || 80);
      return;
    }
    if (action === 'set_chat_session_hinted_message') {
      this.setChatSessionHintedMessage(op?.session_key, op?.message_id);
      return;
    }
    if (action === 'clear_chat_session') {
      this.clearChatSession(op?.session_key);
    }
  }
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

function parseLowInformationDecision(raw) {
  try {
    const parsed = JSON.parse(extractBalancedJsonObject(raw));
    return {
      allow: parsed?.allow !== false,
      fallback: String(parsed?.fallback ?? '').trim(),
      reason: String(parsed?.reason ?? '').trim(),
      startGroupFileDownload: parsed?.start_group_file_download === true,
      requestText: String(parsed?.request_text ?? '').trim()
    };
  } catch {
    return {
      allow: true,
      fallback: '',
      reason: 'parse-failed',
      startGroupFileDownload: false,
      requestText: ''
    };
  }
}

function buildLowInformationFallback(sourceText, replyText = '') {
  const source = String(sourceText ?? '').trim();
  const reply = String(replyText ?? '').trim();
  const combined = `${source}\n${reply}`;
  if (/(mindustry|mindustryx|mdt|牡丹亭|datapatch|方块|建筑|炮塔|单位|物品|液体|状态|星球|天气|字段|超速|投影|穹顶)/i.test(combined)) {
    return '还没定位到具体字段。';
  }
  if (/(模组|mod|插件|脚本|源码|仓库|项目|目录|构建|编译|报错|服务端|服务器)/i.test(combined)) {
    return '还没定位到具体位置。';
  }
  return '还没定位到具体答案。';
}

async function maybeFilterLowInformationReply(qaClient, logger, sourceText, replyText, options = {}) {
  const normalizedReply = String(replyText ?? '').trim();
  if (!normalizedReply) {
    return {
      text: '',
      startGroupFileDownload: false,
      requestText: '',
      reason: 'empty-reply'
    };
  }
  const normalizedSource = String(sourceText ?? '').trim();
  if (!normalizedSource) {
    return {
      text: normalizedReply,
      startGroupFileDownload: false,
      requestText: '',
      reason: ''
    };
  }

  try {
    const raw = await qaClient.complete([
      {
        role: 'system',
        content: [
          '你是聊天回复质检器，只判断这条回复该不该发出去。',
          '如果回复只是把用户问题换词重复、空泛复述、没有新增信息、没有具体定位、没有实际帮助，就判定 allow=false。',
          '如果回复给出了具体做法、具体定位、明确结论、有效下一步，判定 allow=true。',
          '当用户在问“怎么改/怎么做/在哪里/哪个字段”时，像“改对应字段”“看对应对象”“去改相关配置”这类话都算低信息空话。',
          '像“需要查文档再确认”“请提供更多上下文/配置名称我才能定位”“还没能读取对应文件/JSON，因此不敢确定”“收到，先读取某文件”这类把工作往后推、但没有给出读取结果的回复，一律判定 allow=false。',
          '如果这类问题本来就应该先读文件或调工具确认，而拟发送回复里既没有真实读取结果，也没有具体字段/路径/对象名/版本结论，也一律 allow=false。',
          '如果用户原话本身是要安装包、jar、zip、apk、客户端、最新版文件、release 资产、插件包、服务器插件，而拟发送回复只是“帮你交给下载流程”“等我给你找文件”“我去走下载流程”这种口头承诺但没有真实调用，那么应判定 allow=false，并设置 start_group_file_download=true。',
          '出现 start_group_file_download=true 时，request_text 默认填写用户原话；除非用户原话缺关键信息且你能更精确重写，否则不要改写。',
          '只输出 JSON：{"allow":boolean,"fallback":"可选的替代短句","reason":"简短原因","start_group_file_download":boolean,"request_text":"可选，默认用用户原话"}',
          'fallback 只在 allow=false 且需要替代短句时填写，否则留空。',
          '如果当前模式是 fallback，并且这条回复属于“先去查文档/先去读文件”的空话，fallback 应改成一句更硬的纠偏短句，明确要求先读取对应文件或工具结果后再回答，不要复述原空话。'
        ].join('\n')
      },
      {
        role: 'user',
        content: [
          `用户原话：${normalizedSource}`,
          `拟发送回复：${normalizedReply}`,
          `低信息时的处理模式：${options.onLowInformation === 'fallback' ? 'fallback' : 'suppress'}`
        ].join('\n')
      }
    ], {
      model: options?.lowInformationFilterModel || 'gpt-5.4-mini',
      temperature: 0.1
    });

    const decision = parseLowInformationDecision(raw);
    if (decision.allow) {
      return {
        text: normalizedReply,
        startGroupFileDownload: false,
        requestText: '',
        reason: decision.reason
      };
    }
    logger.info(`已拦截低信息回复：${decision.reason || 'no-reason'} | source=${normalizedSource.slice(0, 80)} | reply=${normalizedReply.slice(0, 80)}`);
    if (decision.startGroupFileDownload) {
      return {
        text: '',
        startGroupFileDownload: true,
        requestText: decision.requestText || normalizedSource,
        reason: decision.reason
      };
    }
    if (options.onLowInformation === 'fallback') {
      return {
        text: buildLowInformationFallback(normalizedSource, normalizedReply),
        startGroupFileDownload: false,
        requestText: '',
        reason: decision.reason
      };
    }
    return {
      text: '',
      startGroupFileDownload: false,
      requestText: '',
      reason: decision.reason
    };
  } catch (error) {
    logger.warn(`低信息回复判定失败，回退为原回复：${error.message}`);
    return {
      text: normalizedReply,
      startGroupFileDownload: false,
      requestText: '',
      reason: 'filter-error'
    };
  }
}

function normalizeContext(value = {}) {
  return {
    messageType: String(value?.messageType ?? '').trim() === 'group' ? 'group' : 'private',
    groupId: String(value?.groupId ?? '').trim(),
    userId: String(value?.userId ?? '').trim(),
    selfId: String(value?.selfId ?? '').trim()
  };
}

function normalizeChatInput(value = {}) {
  const runtimeContext = value?.runtimeContext && typeof value.runtimeContext === 'object'
    ? value.runtimeContext
    : {};
  return {
    text: String(value?.text ?? '').trim(),
    historyText: String(value?.historyText ?? value?.history_text ?? '').trim(),
    images: Array.isArray(value?.images) ? value.images.filter(Boolean) : [],
    runtimeContext: {
      senderName: String(runtimeContext?.senderName ?? runtimeContext?.sender_name ?? '').trim(),
      timelineText: String(runtimeContext?.timelineText ?? runtimeContext?.timeline_text ?? '').trim(),
      currentMessageId: String(runtimeContext?.currentMessageId ?? runtimeContext?.current_message_id ?? '').trim(),
      currentMessageSeq: String(runtimeContext?.currentMessageSeq ?? runtimeContext?.current_message_seq ?? '').trim(),
      currentTime: Number(runtimeContext?.currentTime ?? runtimeContext?.current_time ?? 0) || 0
    }
  };
}

async function readRecentMessages(napcatClient, params = {}) {
  const context = params?.context ?? {};
  const count = Math.max(1, Math.min(100, Number(params?.count ?? 20) || 20));
  if (context.messageType === 'group') {
    const payload = await napcatClient.getGroupMessageHistory(String(context.groupId ?? ''), {
      count: Math.max(count + 12, 20)
    });
    return Array.isArray(payload?.messages) ? payload.messages : [];
  }
  const payload = await napcatClient.getFriendMessageHistory(String(context.userId ?? ''), {
    count: Math.max(count + 12, 20)
  });
  return Array.isArray(payload?.messages) ? payload.messages : [];
}

async function readGroupMessages(napcatClient, params = {}) {
  const context = params?.context ?? {};
  const count = Math.max(1, Math.min(1000, Number(params?.count ?? 100) || 100));
  const payload = await napcatClient.getGroupMessageHistory(String(context.groupId ?? ''), {
    count: Math.max(count + 20, 30)
  });
  return Array.isArray(payload?.messages) ? payload.messages : [];
}

async function main() {
  const configPath = String(process.argv[2] ?? '').trim();
  if (!configPath) {
    throw new Error('缺少配置文件路径参数');
  }

  const workerLogger = createWorkerLogger();
  const loaded = await loadConfig(configPath);
  const runtimeConfigStore = new RuntimeConfigStore(
    loaded.config.bot.runtimeConfigFile,
    loaded.configDir,
    {
      qaExternalExclusiveGroupsFile: loaded.config.qa.externalExclusiveGroupsFile,
      qaExternalExclusiveGroupsRefreshMs: loaded.config.qa.externalExclusiveGroupsRefreshMs
    },
    workerLogger
  );
  await runtimeConfigStore.load();

  const chatStateStore = new MergingChatStateStore(loaded.config.bot.stateFile);
  await chatStateStore.load();

  const napcatClient = new NapCatClient(loaded.config.napcat, workerLogger);
  const qaClient = new OpenAiChatClient(loaded.config.qa.client, workerLogger);
  const localRagRetriever = new LocalRagRetriever(loaded.config.qa.answer, workerLogger);

  let pendingGroupFileDownloadRequest = null;
  const codexReadonlyTools = new CodexReadonlyTools(loaded.config.qa.answer, workerLogger, {
    ownerUserId: loaded.config.bot.ownerUserId,
    memoryFile: loaded.config.qa.answer.memoryFile,
    promptImageRoot: loaded.config.qa.answer.promptImageRoot,
    readRecentMessages: async (params) => await readRecentMessages(napcatClient, params),
    readGroupMessages: async (params) => await readGroupMessages(napcatClient, params),
    startGroupFileDownload: async (request, runtimeContext = {}) => {
      pendingGroupFileDownloadRequest = {
        requestText: String(request?.request_text ?? request?.text ?? request?.query ?? runtimeContext?.timelineText ?? '').trim(),
        request
      };
      return {
        started: true,
        delegated: true,
        request_text: pendingGroupFileDownloadRequest.requestText
      };
    },
    sendPromptImage: async ({ context, imagePath }) => {
      await napcatClient.sendContextImage(context, imagePath);
    }
  });

  const chatSessionManager = new ChatSessionManager(
    loaded.config.qa,
    qaClient,
    chatStateStore,
    workerLogger,
    codexReadonlyTools,
    runtimeConfigStore,
    localRagRetriever
  );

  const rl = readline.createInterface({
    input: process.stdin,
    crlfDelay: Infinity
  });

  for await (const line of rl) {
    const trimmed = String(line ?? '').trim();
    if (!trimmed) {
      continue;
    }

    let request = null;
    try {
      request = JSON.parse(trimmed);
      if (request?.action === 'shutdown') {
        process.stdout.write(`${JSON.stringify({ id: request.id ?? null, ok: true, stopped: true })}\n`);
        break;
      }

      await runtimeConfigStore.load().catch((error) => {
        workerLogger.warn(`重载运行时配置失败：${error?.message ?? error}`);
      });
      await chatStateStore.load().catch((error) => {
        workerLogger.warn(`重载聊天状态失败：${error?.message ?? error}`);
      });

      const payload = request?.payload ?? {};
      const context = normalizeContext(payload?.context ?? {});

      if (request?.action === 'is_group_enabled') {
        process.stdout.write(`${JSON.stringify({
          id: request.id ?? null,
          ok: true,
          data: { enabled: chatSessionManager.isGroupEnabled(String(payload?.groupId ?? '')) }
        })}\n`);
        continue;
      }

      if (request?.action === 'is_group_proactive_reply_enabled') {
        process.stdout.write(`${JSON.stringify({
          id: request.id ?? null,
          ok: true,
          data: { enabled: chatSessionManager.isGroupProactiveReplyEnabled(String(payload?.groupId ?? '')) }
        })}\n`);
        continue;
      }

      if (request?.action === 'get_group_prompt_status') {
        process.stdout.write(`${JSON.stringify({
          id: request.id ?? null,
          ok: true,
          data: chatSessionManager.getGroupPromptStatus(String(payload?.groupId ?? ''))
        })}\n`);
        continue;
      }

      if (request?.action === 'should_run_group_proactive_filter') {
        process.stdout.write(`${JSON.stringify({
          id: request.id ?? null,
          ok: true,
          data: chatSessionManager.shouldRunGroupProactiveFilter(String(payload?.groupId ?? ''))
        })}\n`);
        continue;
      }

      if (request?.action === 'reset_group_filter_heartbeat') {
        chatSessionManager.resetGroupFilterHeartbeat(String(payload?.groupId ?? ''));
        process.stdout.write(`${JSON.stringify({ id: request.id ?? null, ok: true, data: { reset: true } })}\n`);
        continue;
      }

      if (request?.action === 'record_incoming_message') {
        await chatSessionManager.recordIncomingMessage(context, payload?.event ?? {}, {
          summary: String(payload?.summary ?? '').trim() || undefined
        });
        process.stdout.write(`${JSON.stringify({ id: request.id ?? null, ok: true, data: { recorded: true } })}\n`);
        continue;
      }

      if (request?.action === 'mark_hinted') {
        await chatSessionManager.markHinted(context, String(payload?.messageId ?? ''));
        process.stdout.write(`${JSON.stringify({ id: request.id ?? null, ok: true, data: { marked: true } })}\n`);
        continue;
      }

      if (request?.action === 'chat') {
        pendingGroupFileDownloadRequest = null;
        const result = await chatSessionManager.chat(context, normalizeChatInput(payload?.input ?? {}));
        process.stdout.write(`${JSON.stringify({
          id: request.id ?? null,
          ok: true,
          data: {
            text: String(result?.text ?? '').trim(),
            notice: String(result?.notice ?? '').trim(),
            groupFileDownloadRequest: pendingGroupFileDownloadRequest
          }
        })}\n`);
        continue;
      }

      if (request?.action === 'should_suggest_reply') {
        const result = await chatSessionManager.shouldSuggestReply(context, payload?.event ?? {});
        process.stdout.write(`${JSON.stringify({
          id: request.id ?? null,
          ok: true,
          data: {
            shouldPrompt: result?.shouldPrompt === true,
            reason: String(result?.reason ?? '').trim()
          }
        })}\n`);
        continue;
      }

      if (request?.action === 'maybe_close_group_topic') {
        const result = await chatSessionManager.maybeCloseGroupTopic(String(payload?.groupId ?? ''));
        process.stdout.write(`${JSON.stringify({
          id: request.id ?? null,
          ok: true,
          data: {
            shouldEnd: result?.shouldEnd === true,
            reason: String(result?.reason ?? '').trim()
          }
        })}\n`);
        continue;
      }

      if (request?.action === 'disable_group_proactive_replies') {
        const status = await chatSessionManager.disableGroupProactiveReplies(String(payload?.groupId ?? ''));
        process.stdout.write(`${JSON.stringify({ id: request.id ?? null, ok: true, data: status })}\n`);
        continue;
      }

      if (request?.action === 'update_filter_prompt') {
        const result = await chatSessionManager.updateFilterPrompt(String(payload?.groupId ?? ''), String(payload?.instruction ?? ''));
        process.stdout.write(`${JSON.stringify({ id: request.id ?? null, ok: true, data: result })}\n`);
        continue;
      }

      if (request?.action === 'update_answer_prompt') {
        const result = await chatSessionManager.updateAnswerPrompt(String(payload?.groupId ?? ''), String(payload?.instruction ?? ''));
        process.stdout.write(`${JSON.stringify({ id: request.id ?? null, ok: true, data: result })}\n`);
        continue;
      }

      if (request?.action === 'maybe_capture_correction_memory') {
        const result = await chatSessionManager.maybeCaptureCorrectionMemory(context, payload?.event ?? {});
        process.stdout.write(`${JSON.stringify({ id: request.id ?? null, ok: true, data: result ?? null })}\n`);
        continue;
      }

      if (request?.action === 'review_low_information_reply') {
        const review = await maybeFilterLowInformationReply(
          qaClient,
          workerLogger,
          String(payload?.sourceText ?? ''),
          String(payload?.replyText ?? ''),
          {
            onLowInformation: String(payload?.onLowInformation ?? 'suppress'),
            lowInformationFilterModel: loaded.config.qa.lowInformationFilterModel
          }
        );
        process.stdout.write(`${JSON.stringify({ id: request.id ?? null, ok: true, data: review })}\n`);
        continue;
      }

      process.stdout.write(`${JSON.stringify({ id: request?.id ?? null, ok: false, error: `unknown action: ${request?.action ?? ''}` })}\n`);
    } catch (error) {
      writeStderr(`[qa-session-worker] [error]`, error?.stack || error?.message || error);
      process.stdout.write(`${JSON.stringify({
        id: request?.id ?? null,
        ok: false,
        error: String(error?.message ?? error ?? 'unknown error')
      })}\n`);
    }
  }
}

await main().catch((error) => {
  writeStderr('[qa-session-worker] [fatal]', error?.stack || error?.message || error);
  process.exitCode = 1;
});
