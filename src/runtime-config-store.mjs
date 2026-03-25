import path from 'node:path';
import fs from 'node:fs/promises';

import { ensureDir, nowIso } from './utils.mjs';

function normalizePromptText(value) {
  if (typeof value !== 'string') {
    return '';
  }
  return value.replace(/\r\n/g, '\n').trim();
}

function normalizeQaGroup(entry) {
  const groupId = String(entry?.groupId ?? '').trim();
  return {
    groupId,
    enabled: entry?.enabled !== false,
    proactiveReplyEnabled: entry?.proactiveReplyEnabled !== false,
    fileDownloadEnabled: entry?.fileDownloadEnabled === true,
    fileDownloadFolderName: String(entry?.fileDownloadFolderName ?? entry?.fileDownloadFolder ?? '').trim(),
    createdAt: String(entry?.createdAt ?? ''),
    updatedAt: String(entry?.updatedAt ?? '')
  };
}

function normalizeGroupQaOverride(entry) {
  const groupId = String(entry?.groupId ?? '').trim();
  return {
    groupId,
    filterPrompt: normalizePromptText(entry?.filterPrompt ?? entry?.disguiseSystemPrompt),
    answerPrompt: normalizePromptText(entry?.answerPrompt ?? entry?.chatSystemPrompt),
    createdAt: String(entry?.createdAt ?? ''),
    updatedAt: String(entry?.updatedAt ?? '')
  };
}

function createDefaultData() {
  return {
    version: 6,
    qaGroups: [],
    groupQaOverrides: []
  };
}

export class RuntimeConfigStore {
  constructor(filePath, configDir, defaults, logger) {
    this.filePath = filePath;
    this.configDir = configDir;
    this.defaults = defaults;
    this.logger = logger;
    this.data = createDefaultData();
  }

  async load() {
    try {
      const text = await fs.readFile(this.filePath, 'utf8');
      const parsed = JSON.parse(text);
      const qaGroups = Array.isArray(parsed?.qaGroups)
        ? parsed.qaGroups.map((item) => normalizeQaGroup(item)).filter((item) => item.groupId)
        : [];

      const migratedQaGroups = qaGroups.length > 0
        ? qaGroups
        : (Array.isArray(parsed?.disguiseGroups)
          ? parsed.disguiseGroups.map((item) => normalizeQaGroup(item)).filter((item) => item.groupId && item.enabled !== false)
          : []);

      const groupQaOverrides = Array.isArray(parsed?.groupQaOverrides)
        ? parsed.groupQaOverrides.map((item) => normalizeGroupQaOverride(item)).filter((item) => item.groupId)
        : [];

      const migratedOverrides = groupQaOverrides.length > 0
        ? groupQaOverrides
        : (Array.isArray(parsed?.groupPromptOverrides)
          ? parsed.groupPromptOverrides.map((item) => normalizeGroupQaOverride(item)).filter((item) => item.groupId)
          : []);

      this.data = {
        ...createDefaultData(),
        ...parsed,
        qaGroups: migratedQaGroups,
        groupQaOverrides: migratedOverrides
      };
    } catch (error) {
      if (error?.code !== 'ENOENT') {
        throw error;
      }
      await this.save();
    }
  }

  getQaGroups() {
    return this.data.qaGroups
      .map((item) => normalizeQaGroup(item))
      .filter((item) => item.groupId);
  }

  isQaGroupEnabled(groupId, staticGroupIds = []) {
    const normalizedGroupId = String(groupId ?? '').trim();
    if (!normalizedGroupId) {
      return false;
    }
    const override = this.getQaGroups().find((item) => item.groupId === normalizedGroupId);
    if (override) {
      return override.enabled !== false;
    }
    return (Array.isArray(staticGroupIds) ? staticGroupIds : [])
      .map((item) => String(item ?? '').trim())
      .includes(normalizedGroupId);
  }

  isQaGroupProactiveReplyEnabled(groupId, staticGroupIds = []) {
    const normalizedGroupId = String(groupId ?? '').trim();
    if (!normalizedGroupId) {
      return false;
    }
    if (!this.isQaGroupEnabled(normalizedGroupId, staticGroupIds)) {
      return false;
    }
    const override = this.getQaGroups().find((item) => item.groupId === normalizedGroupId);
    if (override) {
      return override.proactiveReplyEnabled !== false;
    }
    return true;
  }

  listEnabledQaGroups(staticGroupIds = []) {
    const merged = new Map();
    for (const groupId of Array.isArray(staticGroupIds) ? staticGroupIds : []) {
      const normalizedGroupId = String(groupId ?? '').trim();
      if (normalizedGroupId) {
        merged.set(normalizedGroupId, {
          groupId: normalizedGroupId,
          enabled: true,
          proactiveReplyEnabled: true,
          fileDownloadEnabled: false,
          source: 'static'
        });
      }
    }
    for (const item of this.getQaGroups()) {
      if (item.enabled === false) {
        merged.delete(item.groupId);
        continue;
      }
      merged.set(item.groupId, {
        groupId: item.groupId,
        enabled: true,
        proactiveReplyEnabled: item.proactiveReplyEnabled !== false,
        fileDownloadEnabled: item.fileDownloadEnabled === true,
        source: 'runtime',
        createdAt: item.createdAt,
        updatedAt: item.updatedAt
      });
    }
    return Array.from(merged.values()).sort((left, right) => left.groupId.localeCompare(right.groupId, 'zh-CN'));
  }

  async setQaGroupEnabled(groupId, enabled, options = {}) {
    const normalizedGroupId = String(groupId ?? '').trim();
    if (!normalizedGroupId) {
      throw new Error('groupId 不能为空');
    }
    const nextEnabled = enabled !== false;
    const index = this.data.qaGroups.findIndex((item) => String(item?.groupId ?? '').trim() === normalizedGroupId);
    if (index >= 0) {
      this.data.qaGroups[index] = {
        ...this.data.qaGroups[index],
        groupId: normalizedGroupId,
        enabled: nextEnabled,
        proactiveReplyEnabled: nextEnabled
          ? (options.proactiveReplyEnabled !== false)
          : (this.data.qaGroups[index]?.proactiveReplyEnabled !== false),
        fileDownloadEnabled: this.data.qaGroups[index]?.fileDownloadEnabled === true,
        updatedAt: nowIso()
      };
      await this.save();
      return { action: 'updated', entry: normalizeQaGroup(this.data.qaGroups[index]) };
    }

    const record = {
      groupId: normalizedGroupId,
      enabled: nextEnabled,
      proactiveReplyEnabled: nextEnabled
        ? (options.proactiveReplyEnabled !== false)
        : true,
      fileDownloadEnabled: options.fileDownloadEnabled === true,
      createdAt: nowIso(),
      updatedAt: nowIso()
    };
    this.data.qaGroups.push(record);
    await this.save();
    return { action: 'created', entry: normalizeQaGroup(record) };
  }

  async setQaGroupProactiveReplyEnabled(groupId, enabled, staticGroupIds = []) {
    const normalizedGroupId = String(groupId ?? '').trim();
    if (!normalizedGroupId) {
      throw new Error('groupId 不能为空');
    }
    const currentEnabled = this.isQaGroupEnabled(normalizedGroupId, staticGroupIds);
    const index = this.data.qaGroups.findIndex((item) => String(item?.groupId ?? '').trim() === normalizedGroupId);
    if (index >= 0) {
      this.data.qaGroups[index] = {
        ...this.data.qaGroups[index],
        groupId: normalizedGroupId,
        enabled: currentEnabled || this.data.qaGroups[index]?.enabled !== false,
        proactiveReplyEnabled: enabled !== false,
        fileDownloadEnabled: this.data.qaGroups[index]?.fileDownloadEnabled === true,
        updatedAt: nowIso()
      };
      await this.save();
      return { action: 'updated', entry: normalizeQaGroup(this.data.qaGroups[index]) };
    }

    const record = {
      groupId: normalizedGroupId,
      enabled: currentEnabled,
      proactiveReplyEnabled: enabled !== false,
      fileDownloadEnabled: false,
      createdAt: nowIso(),
      updatedAt: nowIso()
    };
    this.data.qaGroups.push(record);
    await this.save();
    return { action: 'created', entry: normalizeQaGroup(record) };
  }

  isQaGroupFileDownloadEnabled(groupId) {
    const normalizedGroupId = String(groupId ?? '').trim();
    if (!normalizedGroupId) {
      return false;
    }
    const override = this.getQaGroups().find((item) => item.groupId === normalizedGroupId);
    return override?.fileDownloadEnabled === true;
  }

  getQaGroupFileDownloadFolderName(groupId) {
    const normalizedGroupId = String(groupId ?? '').trim();
    if (!normalizedGroupId) {
      return '';
    }
    const override = this.getQaGroups().find((item) => item.groupId === normalizedGroupId);
    return String(override?.fileDownloadFolderName ?? '').trim();
  }

  async setQaGroupFileDownloadEnabled(groupId, enabled, staticGroupIds = [], folderName = '') {
    const normalizedGroupId = String(groupId ?? '').trim();
    if (!normalizedGroupId) {
      throw new Error('groupId 不能为空');
    }
    const normalizedFolderName = String(folderName ?? '').trim();
    const currentEnabled = this.isQaGroupEnabled(normalizedGroupId, staticGroupIds);
    const index = this.data.qaGroups.findIndex((item) => String(item?.groupId ?? '').trim() === normalizedGroupId);
    if (index >= 0) {
      this.data.qaGroups[index] = {
        ...this.data.qaGroups[index],
        groupId: normalizedGroupId,
        enabled: currentEnabled || this.data.qaGroups[index]?.enabled !== false,
        proactiveReplyEnabled: this.data.qaGroups[index]?.proactiveReplyEnabled !== false,
        fileDownloadEnabled: enabled === true,
        fileDownloadFolderName: normalizedFolderName || String(this.data.qaGroups[index]?.fileDownloadFolderName ?? '').trim(),
        updatedAt: nowIso()
      };
      await this.save();
      return { action: 'updated', entry: normalizeQaGroup(this.data.qaGroups[index]) };
    }

    const record = {
      groupId: normalizedGroupId,
      enabled: currentEnabled,
      proactiveReplyEnabled: true,
      fileDownloadEnabled: enabled === true,
      fileDownloadFolderName: normalizedFolderName,
      createdAt: nowIso(),
      updatedAt: nowIso()
    };
    this.data.qaGroups.push(record);
    await this.save();
    return { action: 'created', entry: normalizeQaGroup(record) };
  }

  getGroupQaOverride(groupId) {
    const normalizedGroupId = String(groupId ?? '').trim();
    if (!normalizedGroupId) {
      return null;
    }
    const found = this.data.groupQaOverrides.find((item) => String(item?.groupId ?? '').trim() === normalizedGroupId);
    return found ? normalizeGroupQaOverride(found) : null;
  }

  async setGroupQaOverride(entry) {
    const normalized = normalizeGroupQaOverride(entry);
    if (!normalized.groupId) {
      throw new Error('groupId 不能为空');
    }

    const hasAnyPrompt = Boolean(normalized.filterPrompt || normalized.answerPrompt);
    const index = this.data.groupQaOverrides.findIndex((item) => String(item?.groupId ?? '').trim() === normalized.groupId);

    if (!hasAnyPrompt) {
      if (index >= 0) {
        this.data.groupQaOverrides.splice(index, 1);
        await this.save();
        return { action: 'removed', entry: null };
      }
      return { action: 'noop', entry: null };
    }

    if (index >= 0) {
      this.data.groupQaOverrides[index] = {
        ...this.data.groupQaOverrides[index],
        ...normalized,
        updatedAt: nowIso()
      };
      await this.save();
      return { action: 'updated', entry: normalizeGroupQaOverride(this.data.groupQaOverrides[index]) };
    }

    const record = {
      ...normalized,
      createdAt: nowIso(),
      updatedAt: nowIso()
    };
    this.data.groupQaOverrides.push(record);
    await this.save();
    return { action: 'created', entry: normalizeGroupQaOverride(record) };
  }

  async save() {
    await ensureDir(path.dirname(this.filePath));
    await fs.writeFile(this.filePath, JSON.stringify(this.data, null, 2), 'utf8');
  }
}
