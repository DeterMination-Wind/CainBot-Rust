import path from 'node:path';
import fs from 'node:fs/promises';

import { ensureDir } from './utils.mjs';

function createDefaultState() {
  return {
    version: 6,
    chatSessions: {},
    msavReplyContexts: {},
    webui: {},
    issueRepair: {
      offers: {},
      sessions: {}
    }
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

function normalizeIssueRepairState(state) {
  const source = state && typeof state === 'object' ? state : {};
  const offers = source.offers && typeof source.offers === 'object' ? source.offers : {};
  const sessions = source.sessions && typeof source.sessions === 'object' ? source.sessions : {};
  return {
    offers,
    sessions
  };
}

export class StateStore {
  constructor(filePath, logger) {
    this.filePath = filePath;
    this.logger = logger;
    this.state = createDefaultState();
    this.#savePromise = Promise.resolve();
  }

  async load() {
    try {
      const text = await fs.readFile(this.filePath, 'utf8');
      const parsed = JSON.parse(text);
      this.state = {
        ...createDefaultState(),
        ...parsed,
        chatSessions: parsed?.chatSessions ?? {},
        msavReplyContexts: parsed?.msavReplyContexts ?? {},
        webui: parsed?.webui ?? {},
        issueRepair: normalizeIssueRepairState(parsed?.issueRepair)
      };
      this.logger.info('已加载状态文件。');
    } catch (error) {
      if (error?.code === 'ENOENT') {
        await ensureDir(path.dirname(this.filePath));
        this.logger.info('未发现状态文件，将在首次保存时创建。');
        return;
      }
      throw error;
    }
  }

  getChatSession(sessionKey) {
    const normalizedKey = String(sessionKey ?? '').trim();
    if (!normalizedKey) {
      throw new Error('sessionKey 不能为空');
    }
    if (!this.state.chatSessions[normalizedKey]) {
      this.state.chatSessions[normalizedKey] = normalizeSession({});
    }
    this.state.chatSessions[normalizedKey] = normalizeSession(this.state.chatSessions[normalizedKey]);
    return this.state.chatSessions[normalizedKey];
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
    delete this.state.chatSessions[String(sessionKey ?? '').trim()];
  }

  getIssueRepairState() {
    this.state.issueRepair = normalizeIssueRepairState(this.state.issueRepair);
    return this.state.issueRepair;
  }

  listIssueRepairOffers() {
    return Object.values(this.getIssueRepairState().offers);
  }

  getIssueRepairOffer(offerId) {
    return this.getIssueRepairState().offers[String(offerId ?? '').trim()] ?? null;
  }

  setIssueRepairOffer(offer) {
    const normalizedId = String(offer?.id ?? '').trim();
    if (!normalizedId) {
      throw new Error('issue repair offer id 不能为空');
    }
    this.getIssueRepairState().offers[normalizedId] = offer;
    return offer;
  }

  deleteIssueRepairOffer(offerId) {
    delete this.getIssueRepairState().offers[String(offerId ?? '').trim()];
  }

  listIssueRepairSessions() {
    return Object.values(this.getIssueRepairState().sessions);
  }

  getIssueRepairSession(sessionId) {
    return this.getIssueRepairState().sessions[String(sessionId ?? '').trim()] ?? null;
  }

  setIssueRepairSession(session) {
    const normalizedId = String(session?.id ?? '').trim();
    if (!normalizedId) {
      throw new Error('issue repair session id 不能为空');
    }
    this.getIssueRepairState().sessions[normalizedId] = session;
    return session;
  }

  deleteIssueRepairSession(sessionId) {
    delete this.getIssueRepairState().sessions[String(sessionId ?? '').trim()];
  }

  async save() {
    this.#savePromise = this.#savePromise.then(async () => {
      await ensureDir(path.dirname(this.filePath));
      await fs.writeFile(this.filePath, JSON.stringify(this.state, null, 2), 'utf8');
    });
    await this.#savePromise;
  }

  #savePromise;
}
