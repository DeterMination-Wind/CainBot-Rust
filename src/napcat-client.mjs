import fs from 'node:fs/promises';

import { buildForwardNodes, buildReplyMessage, joinUrl, sleep, splitText } from './utils.mjs';

function isMissingReplyTargetError(error) {
  const message = String(error?.message ?? '');
  if (!message) {
    return false;
  }
  return message.includes('消息不存在')
    || message.toLowerCase().includes('message not found')
    || message.toLowerCase().includes('msg not found');
}

function isReplySendRejectedError(error) {
  const message = String(error?.message ?? '');
  if (!message) {
    return false;
  }
  return /send_group_msg/i.test(message)
    && (/EventChecker Failed/i.test(message)
      || /NTEvent/i.test(message)
      || /result"\s*:\s*120/i.test(message)
      || /result\s*[:=]\s*120/i.test(message));
}

function flattenMessageText(message) {
  if (typeof message === 'string') {
    return message;
  }
  if (Array.isArray(message)) {
    return message
      .map((segment) => {
        if (typeof segment === 'string') {
          return segment;
        }
        if (segment?.type === 'text') {
          return String(segment?.data?.text ?? '');
        }
        return '';
      })
      .join('')
      .trim();
  }
  if (message && typeof message === 'object' && message.type === 'text') {
    return String(message?.data?.text ?? '').trim();
  }
  return '';
}

function sanitizeOutgoingText(text) {
  return String(text ?? '')
    .replace(/\u0000/g, '')
    .replace(/\r\n/g, '\n')
    .replace(/\n{3,}/g, '\n\n')
    .trim();
}

export class NapCatClient {
  constructor(config, logger) {
    this.config = config;
    this.logger = logger;
    this.stopped = false;
    this.pendingEventTasks = new Set();
    this.onReplySent = typeof config?.onReplySent === 'function' ? config.onReplySent : null;
  }

  stop() {
    this.stopped = true;
  }

  async call(action, params = {}) {
    const controller = new AbortController();
    const timeout = setTimeout(() => controller.abort(), this.config.requestTimeoutMs);
    try {
      const response = await fetch(joinUrl(this.config.baseUrl, action), {
        method: 'POST',
        headers: {
          'Content-Type': 'application/json',
          ...this.config.headers
        },
        body: JSON.stringify(params),
        signal: controller.signal
      });

      if (!response.ok) {
        throw new Error(`NapCat API ${action} 返回 HTTP ${response.status}`);
      }

      const payload = await response.json();
      if (payload?.status && payload.status !== 'ok') {
        throw new Error(`NapCat API ${action} 失败: ${payload?.message || payload?.wording || payload.status}`);
      }
      if (payload?.retcode != null && payload.retcode !== 0) {
        throw new Error(`NapCat API ${action} retcode=${payload.retcode}`);
      }
      return payload?.data ?? payload;
    } finally {
      clearTimeout(timeout);
    }
  }

  async getMessage(messageId) {
    return await this.call('get_msg', { message_id: String(messageId) });
  }

  async deleteMessage(messageId) {
    return await this.call('delete_msg', { message_id: String(messageId) });
  }

  async getFile(fileId) {
    return await this.call('get_file', { file_id: String(fileId) });
  }

  async getGroupMemberInfo(groupId, userId, noCache = true) {
    return await this.call('get_group_member_info', {
      group_id: String(groupId),
      user_id: String(userId),
      no_cache: Boolean(noCache)
    });
  }

  async getGroupNotice(groupId) {
    return await this.call('_get_group_notice', { group_id: String(groupId) });
  }

  async getEssenceMessageList(groupId) {
    return await this.call('get_essence_msg_list', { group_id: String(groupId) });
  }

  async getGroupMessageHistory(groupId, options = {}) {
    return await this.call('get_group_msg_history', {
      group_id: String(groupId),
      message_seq: options.messageSeq != null ? String(options.messageSeq) : undefined,
      count: Number(options.count ?? 20),
      reverse_order: Boolean(options.reverseOrder ?? false),
      disable_get_url: Boolean(options.disableGetUrl ?? true),
      parse_mult_msg: Boolean(options.parseMultMsg ?? false),
      quick_reply: Boolean(options.quickReply ?? false),
      reverseOrder: Boolean(options.reverseOrder ?? false)
    });
  }

  async getFriendMessageHistory(userId, options = {}) {
    return await this.call('get_friend_msg_history', {
      user_id: String(userId),
      message_seq: options.messageSeq != null ? String(options.messageSeq) : undefined,
      count: Number(options.count ?? 20),
      reverse_order: Boolean(options.reverseOrder ?? false),
      disable_get_url: Boolean(options.disableGetUrl ?? true),
      parse_mult_msg: Boolean(options.parseMultMsg ?? false),
      quick_reply: Boolean(options.quickReply ?? false),
      reverseOrder: Boolean(options.reverseOrder ?? false)
    });
  }

  async getGroupSystemMessages(count = 50) {
    return await this.call('get_group_system_msg', { count });
  }

  async setGroupCard(groupId, userId, card = '') {
    return await this.call('set_group_card', {
      group_id: String(groupId),
      user_id: String(userId),
      card: String(card ?? '')
    });
  }

  async setGroupAddRequest(flag, approve = true, reason = '', count = 100, subType = '') {
    return await this.call('set_group_add_request', {
      flag: String(flag),
      approve,
      reason,
      count,
      ...(String(subType ?? '').trim() ? { sub_type: String(subType).trim() } : {})
    });
  }

  async sendGroupMessage(groupId, message) {
    try {
      return await this.call('send_group_msg', {
        group_id: String(groupId),
        message
      });
    } catch (error) {
      if (!isReplySendRejectedError(error)) {
        throw error;
      }
      const flattenedText = sanitizeOutgoingText(flattenMessageText(message));
      if (!flattenedText) {
        throw error;
      }
      this.logger.warn(`群消息发送被拒，回退为纯文本重试：${error.message}`);
      return await this.call('send_group_msg', {
        group_id: String(groupId),
        message: flattenedText
      });
    }
  }

  async sendPrivateMessage(userId, message) {
    return await this.call('send_private_msg', {
      user_id: String(userId),
      message
    });
  }

  async uploadGroupFile(params, options = {}) {
    const maxAttempts = Math.max(1, Math.trunc(Number(options.maxAttempts ?? this.config.uploadRetryAttempts ?? 6) || 6));
    const retryDelayMs = Math.max(200, Math.trunc(Number(options.retryDelayMs ?? this.config.uploadRetryDelayMs ?? 2500) || 2500));
    const stableWaitMs = Math.max(200, Math.trunc(Number(options.stableWaitMs ?? this.config.uploadStableWaitMs ?? 1500) || 1500));
    const filePath = String(params?.file ?? '').trim();

    for (let attempt = 1; attempt <= maxAttempts; attempt += 1) {
      try {
        if (filePath) {
          await this.#waitForStableFile(filePath, stableWaitMs);
        }
        return await this.call('upload_group_file', params);
      } catch (error) {
        if (attempt >= maxAttempts || !this.#isRetriableUploadError(error)) {
          throw error;
        }
        this.logger.warn(`群文件上传失败，准备重试（${attempt}/${maxAttempts}）：${error.message}`);
        await sleep(retryDelayMs * attempt);
      }
    }

    throw new Error('群文件上传失败：超过最大重试次数');
  }

  async ensureGroupFolder(groupId, folderName) {
    const normalizedGroupId = String(groupId ?? '').trim();
    const normalizedFolderName = String(folderName ?? '').trim();
    if (!normalizedFolderName) {
      return '';
    }

    const root = await this.call('get_group_root_files', {
      group_id: normalizedGroupId,
      file_count: 500
    });
    const existing = Array.isArray(root?.folders)
      ? root.folders.find((folder) => folder?.folder_name === normalizedFolderName)
      : null;
    if (existing?.folder_id) {
      return String(existing.folder_id);
    }

    await this.call('create_group_file_folder', {
      group_id: normalizedGroupId,
      folder_name: normalizedFolderName
    });

    const refreshed = await this.call('get_group_root_files', {
      group_id: normalizedGroupId,
      file_count: 500
    });
    const created = Array.isArray(refreshed?.folders)
      ? refreshed.folders.find((folder) => folder?.folder_name === normalizedFolderName)
      : null;
    if (!created?.folder_id) {
      throw new Error(`创建群文件夹失败：${normalizedGroupId}/${normalizedFolderName}`);
    }
    return String(created.folder_id);
  }

  async sendLocalFileToGroup(params = {}, options = {}) {
    const groupId = String(params?.groupId ?? params?.group_id ?? '').trim();
    const filePath = String(params?.filePath ?? params?.file ?? '').trim();
    const fileName = String(params?.fileName ?? params?.name ?? '').trim();
    const folderName = String(params?.folderName ?? params?.folder_name ?? '').trim();
    const notifyText = String(params?.notifyText ?? '').trim();

    if (!groupId) {
      throw new Error('groupId 不能为空');
    }
    if (!filePath) {
      throw new Error('filePath 不能为空');
    }

    const fileStat = await fs.stat(filePath).catch(() => null);
    if (!fileStat?.isFile?.()) {
      throw new Error(`文件不存在或不是普通文件：${filePath}`);
    }

    const folderId = folderName ? await this.ensureGroupFolder(groupId, folderName) : '';
    const uploadResult = await this.uploadGroupFile({
      group_id: groupId,
      file: filePath,
      ...(fileName ? { name: fileName } : {}),
      ...(folderId ? { folder: folderId } : {}),
      upload_file: true
    }, options);

    const result = {
      groupId,
      filePath,
      fileName,
      folderName,
      folderId,
      uploadResult
    };

    if (notifyText) {
      result.messageResult = await this.sendGroupMessage(groupId, notifyText);
    }

    return result;
  }

  async sendLocalFileToContext(context, params = {}, options = {}) {
    if (context?.messageType === 'group') {
      return await this.sendLocalFileToGroup({
        groupId: context.groupId,
        filePath: params.filePath,
        fileName: params.fileName,
        folderName: params.folderName,
        notifyText: params.notifyText
      }, options);
    }

    const filePath = String(params?.filePath ?? '').trim();
    if (!filePath) {
      throw new Error('filePath 不能为空');
    }
    return await this.sendPrivateMessage(context.userId, [{
      type: 'file',
      data: {
        file: filePath,
        name: String(params?.fileName ?? '').trim() || undefined
      }
    }]);
  }

  async sendContextMessage(context, message) {
    if (context.messageType === 'group') {
      return await this.sendGroupMessage(context.groupId, message);
    }
    return await this.sendPrivateMessage(context.userId, message);
  }

  async sendContextImage(context, imagePath) {
    return await this.sendContextMessage(context, [{
      type: 'image',
      data: {
        file: String(imagePath)
      }
    }]);
  }

  async replyText(context, replyToMessageId, text) {
    if (String(text ?? '').length > (this.config.forwardThresholdChars ?? 300)) {
      try {
        const forwarded = await this.sendForwardText(context, text);
        return forwarded ? [forwarded] : [];
      } catch (error) {
        this.logger.warn(`发送合并转发失败，回退为普通分段消息：${error.message}`);
      }
    }

    const parts = splitText(text, 1400);
    const results = [];
    for (let index = 0; index < parts.length; index += 1) {
      const part = parts[index];
      const useReply = index === 0 && replyToMessageId != null && String(replyToMessageId).trim();
      const message = buildReplyMessage(useReply ? replyToMessageId : null, part);
      try {
        results.push(await this.sendContextMessage(context, message));
      } catch (error) {
        if (useReply && (isMissingReplyTargetError(error) || isReplySendRejectedError(error))) {
          this.logger.warn(`引用回复发送失败，回退为普通消息发送：${error.message}`);
          results.push(await this.sendContextMessage(context, part));
          continue;
        }
        throw error;
      }
    }

    if (this.onReplySent && replyToMessageId != null && String(replyToMessageId).trim()) {
      try {
        await this.onReplySent({
          context,
          replyToMessageId: String(replyToMessageId).trim(),
          results
        });
      } catch (error) {
        this.logger.warn(`replyText 回调失败：${error.message}`);
      }
    }

    return results;
  }

  async sendForwardText(context, text) {
    const nodes = buildForwardNodes(text, {
      userId: context.selfId || this.config.botUserId || '0',
      nickname: this.config.forwardNickname || 'Cain'
    });

    if (context.messageType === 'group') {
      return await this.call('send_group_forward_msg', {
        group_id: String(context.groupId),
        messages: nodes
      });
    }

    return await this.call('send_private_forward_msg', {
      user_id: String(context.userId),
      messages: nodes
    });
  }

  async startEventLoop(onEvent) {
    let backoffMs = 2000;
    while (!this.stopped) {
      try {
        await this.#runEventStream(onEvent);
        backoffMs = 2000;
      } catch (error) {
        if (this.stopped) {
          break;
        }
        this.logger.warn(`SSE 连接断开：${error.message}`);
        await sleep(backoffMs);
        backoffMs = Math.min(backoffMs * 2, 30000);
      }
    }
  }

  async #runEventStream(onEvent) {
    const response = await fetch(joinUrl(this.config.eventBaseUrl || this.config.baseUrl, this.config.eventPath), {
      method: 'GET',
      headers: {
        Accept: 'text/event-stream',
        ...this.config.headers
      }
    });

    if (!response.ok || !response.body) {
      throw new Error(`NapCat SSE 返回 HTTP ${response.status}`);
    }

    this.logger.info('NapCat SSE 已连接。');
    const reader = response.body.getReader();
    const decoder = new TextDecoder();
    let buffer = '';

    while (!this.stopped) {
      const { done, value } = await reader.read();
      if (done) {
        throw new Error('NapCat SSE 连接已结束');
      }
      buffer += decoder.decode(value, { stream: true }).replace(/\r/g, '');

      let delimiterIndex = buffer.indexOf('\n\n');
      while (delimiterIndex >= 0) {
        const chunk = buffer.slice(0, delimiterIndex);
        buffer = buffer.slice(delimiterIndex + 2);
        delimiterIndex = buffer.indexOf('\n\n');
        const event = this.#parseSseEvent(chunk);
        if (event) {
          await this.#dispatchEvent(onEvent, event);
        }
      }
    }
  }

  async #dispatchEvent(onEvent, event) {
    const maxConcurrentEvents = Number(this.config.maxConcurrentEvents ?? 24) || 24;
    if (this.pendingEventTasks.size >= maxConcurrentEvents) {
      await Promise.race(this.pendingEventTasks);
    }

    const task = Promise.resolve()
      .then(() => onEvent(event))
      .catch((error) => {
        this.logger.error(`事件处理失败：${error.stack || error.message}`);
      })
      .finally(() => {
        this.pendingEventTasks.delete(task);
      });

    this.pendingEventTasks.add(task);
  }

  async #waitForStableFile(filePath, stableWaitMs) {
    let previous = null;
    for (let attempt = 0; attempt < 3; attempt += 1) {
      const current = await fs.stat(filePath);
      if (previous && previous.size === current.size && previous.mtimeMs === current.mtimeMs) {
        return;
      }
      previous = current;
      await sleep(stableWaitMs);
    }
  }

  #isRetriableUploadError(error) {
    const message = String(error?.message ?? '').toLowerCase();
    if (!message.includes('upload_group_file')) {
      return false;
    }
    return [
      'rich media transfer failed',
      'timeout',
      'timed out',
      'econnreset',
      'socket hang up',
      'http 5',
      'network',
      'fetch failed'
    ].some((keyword) => message.includes(keyword));
  }

  #parseSseEvent(chunk) {
    const dataLines = [];
    for (const line of chunk.split('\n')) {
      if (!line || line.startsWith(':')) {
        continue;
      }
      if (line.startsWith('data:')) {
        dataLines.push(line.slice(5).trimStart());
      }
    }

    if (dataLines.length === 0) {
      return null;
    }

    const data = dataLines.join('\n');
    try {
      return JSON.parse(data);
    } catch {
      this.logger.warn('收到无法解析的 SSE 数据。');
      return null;
    }
  }
}


