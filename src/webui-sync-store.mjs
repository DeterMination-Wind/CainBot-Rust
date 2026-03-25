import path from 'node:path';
import fs from 'node:fs/promises';

import { ensureDir } from './utils.mjs';

function normalizeText(value, fallback = '') {
  const normalized = String(value ?? '').trim();
  return normalized || fallback;
}

function createDefaultState() {
  return {
    version: 2,
    msavTasks: []
  };
}

export class WebUiSyncStore {
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
        msavTasks: Array.isArray(parsed?.msavTasks)
          ? parsed.msavTasks.map((item) => this.#normalizeMsavTask(item)).filter(Boolean)
          : []
      };
    } catch (error) {
      if (error?.code === 'ENOENT') {
        await this.save();
        return;
      }
      throw error;
    }
  }

  async upsertMsavTask(task) {
    const normalized = this.#normalizeMsavTask(task);
    if (!normalized) {
      throw new Error('无效的 .msav 任务');
    }
    const index = this.state.msavTasks.findIndex((item) => item.id === normalized.id);
    if (index >= 0) {
      this.state.msavTasks[index] = {
        ...this.state.msavTasks[index],
        ...normalized,
        updatedAt: new Date().toISOString()
      };
    } else {
      this.state.msavTasks.unshift({
        ...normalized,
        updatedAt: new Date().toISOString()
      });
    }
    this.state.msavTasks = this.state.msavTasks
      .sort((left, right) => Date.parse(String(right.updatedAt ?? right.createdAt ?? '')) - Date.parse(String(left.updatedAt ?? left.createdAt ?? '')))
      .slice(0, 200);
    await this.save();
    return normalized;
  }

  async markMsavTaskFinished(taskId, updates = {}) {
    return await this.upsertMsavTask({
      ...updates,
      id: String(taskId ?? '').trim()
    });
  }

  #normalizeMsavTask(task) {
    const id = normalizeText(task?.id);
    if (!id) {
      return null;
    }
    return {
      id,
      type: 'msav-analysis',
      fileName: normalizeText(task?.fileName, '未知.msav'),
      sourceMessageId: normalizeText(task?.sourceMessageId),
      noticeMessageId: normalizeText(task?.noticeMessageId),
      replyMessageId: normalizeText(task?.replyMessageId),
      messageType: normalizeText(task?.messageType),
      groupId: normalizeText(task?.groupId),
      userId: normalizeText(task?.userId),
      status: normalizeText(task?.status, 'running'),
      stage: normalizeText(task?.stage),
      message: normalizeText(task?.message),
      error: normalizeText(task?.error),
      resultPreview: normalizeText(task?.resultPreview),
      createdAt: normalizeText(task?.createdAt, new Date().toISOString()),
      updatedAt: normalizeText(task?.updatedAt, new Date().toISOString())
    };
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
