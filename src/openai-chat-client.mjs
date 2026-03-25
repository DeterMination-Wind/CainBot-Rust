import { isNonEmptyString, joinUrl, sleep } from './utils.mjs';

const RETRYABLE_HTTP_STATUS = new Set([408, 409, 425, 429, 500, 502, 503, 504]);

function createChatError(message, extra = {}) {
  const error = new Error(message);
  Object.assign(error, extra);
  return error;
}

function normalizeErrorText(text) {
  const source = String(text ?? '').trim();
  if (!source) {
    return '';
  }

  try {
    const parsed = JSON.parse(source);
    const message = parsed?.error?.message ?? parsed?.message ?? parsed?.detail;
    if (typeof message === 'string' && message.trim()) {
      return message.trim().slice(0, 600);
    }
  } catch {
  }

  const compact = source
    .replace(/<script[\s\S]*?<\/script>/gi, ' ')
    .replace(/<style[\s\S]*?<\/style>/gi, ' ')
    .replace(/<[^>]+>/g, ' ')
    .replace(/\s+/g, ' ')
    .trim();

  return compact.slice(0, 280);
}

function isRetryableError(error) {
  if (!error) {
    return false;
  }
  if (error.name === 'AbortError' || error.name === 'TimeoutError') {
    return true;
  }
  return /fetch failed|network|socket|timed? out|econnreset|enotfound|eai_again/i.test(String(error.message ?? ''));
}

function normalizeThrowableMessage(error) {
  if (!error) {
    return '未知错误';
  }
  const direct = String(error.message ?? error).trim();
  const causeCode = String(error.cause?.code ?? '').trim();
  const causeMessage = String(error.cause?.message ?? '').trim();
  if (causeCode && causeMessage && !causeMessage.includes(causeCode)) {
    return `${direct} (${causeCode}: ${causeMessage})`;
  }
  if (causeCode) {
    return `${direct} (${causeCode})`;
  }
  if (causeMessage && causeMessage !== direct) {
    return `${direct} (${causeMessage})`;
  }
  return direct;
}

function buildHttpError(status, errorText) {
  const normalized = normalizeErrorText(errorText);
  return createChatError(
    normalized
      ? `聊天接口返回 HTTP ${status}：${normalized}`
      : `聊天接口返回 HTTP ${status}`,
    {
      code: 'CHAT_BACKEND_HTTP_ERROR',
      silent: true
    }
  );
}

function extractAssistantText(payload) {
  const content = payload?.choices?.[0]?.message?.content;
  if (typeof content === 'string') {
    return content.trim();
  }
  if (Array.isArray(content)) {
    return content
      .map((item) => {
        if (typeof item === 'string') {
          return item;
        }
        if (item?.type === 'text') {
          return item.text ?? '';
        }
        return '';
      })
      .join('')
      .trim();
  }
  return '';
}

export class OpenAiChatClient {
  constructor(config, logger) {
    this.config = config;
    this.logger = logger;
    this.cooldownUntil = 0;
    this.cooldownReason = '';
  }

  get enabled() {
    return this.config.enabled !== false;
  }

  validate() {
    if (!isNonEmptyString(this.config.baseUrl)) {
      throw new Error('chat.baseUrl 未配置');
    }
    if (!isNonEmptyString(this.config.model)) {
      throw new Error('chat.model 未配置');
    }
  }

  getRetryConfig() {
    return {
      maxAttempts: Math.max(1, Number(this.config.retryAttempts ?? 3) || 3),
      baseDelayMs: Math.max(200, Number(this.config.retryDelayMs ?? 1500) || 1500),
      requestTimeoutMs: Math.max(5000, Number(this.config.requestTimeoutMs ?? 90000) || 90000),
      failureCooldownMs: Math.max(1000, Number(this.config.failureCooldownMs ?? 60000) || 60000)
    };
  }

  buildCooldownError(reason = '') {
    const remainingMs = Math.max(0, this.cooldownUntil - Date.now());
    const seconds = Math.max(1, Math.ceil(remainingMs / 1000));
    return createChatError(
      `聊天接口暂时不可用，已进入 ${seconds} 秒冷却${reason ? `：${reason}` : ''}`,
      {
        code: 'CHAT_BACKEND_COOLDOWN',
        transient: true,
        silent: true,
        cooldownUntil: this.cooldownUntil
      }
    );
  }

  enterCooldown(reason, cooldownMs) {
    this.cooldownUntil = Date.now() + cooldownMs;
    this.cooldownReason = reason;
    this.logger.warn(`聊天接口暂时不可用，${Math.ceil(cooldownMs / 1000)} 秒内暂停新请求：${reason}`);
    return this.buildCooldownError(reason);
  }

  async complete(messages, options = {}) {
    this.validate();
    const retryConfig = this.getRetryConfig();

    if (Date.now() < this.cooldownUntil) {
      throw this.buildCooldownError(this.cooldownReason);
    }
    this.cooldownUntil = 0;
    this.cooldownReason = '';

    const headers = {
      'Content-Type': 'application/json'
    };
    if (isNonEmptyString(this.config.apiKey)) {
      headers.Authorization = `Bearer ${this.config.apiKey}`;
    }

    let lastError = null;
    for (let attempt = 1; attempt <= retryConfig.maxAttempts; attempt += 1) {
      try {
        const response = await fetch(joinUrl(this.config.baseUrl, 'chat/completions'), {
          method: 'POST',
          headers,
          signal: AbortSignal.timeout(retryConfig.requestTimeoutMs),
          body: JSON.stringify({
            model: options.model ?? this.config.model,
            temperature: options.temperature ?? this.config.temperature ?? 0.7,
            messages
          })
        });

        if (!response.ok) {
          const errorText = await response.text();
          const httpError = buildHttpError(response.status, errorText);
          if (attempt < retryConfig.maxAttempts && RETRYABLE_HTTP_STATUS.has(response.status)) {
            const delayMs = retryConfig.baseDelayMs * attempt;
            this.logger.warn(`聊天接口暂时失败，准备重试（${attempt}/${retryConfig.maxAttempts}，HTTP ${response.status}）`);
            await sleep(delayMs);
            continue;
          }
          if (RETRYABLE_HTTP_STATUS.has(response.status)) {
            throw this.enterCooldown(httpError.message, retryConfig.failureCooldownMs);
          }
          throw httpError;
        }

        const payload = await response.json();
        const assistantText = extractAssistantText(payload);
        if (!assistantText) {
          throw createChatError('聊天接口未返回可用文本', {
            code: 'CHAT_BACKEND_INVALID_RESPONSE',
            silent: true
          });
        }
        return assistantText;
      } catch (error) {
        lastError = error;
        if (attempt < retryConfig.maxAttempts && isRetryableError(error)) {
          const delayMs = retryConfig.baseDelayMs * attempt;
          this.logger.warn(`聊天接口请求异常，准备重试（${attempt}/${retryConfig.maxAttempts}）：${normalizeThrowableMessage(error)}`);
          await sleep(delayMs);
          continue;
        }
        if (isRetryableError(error)) {
          throw this.enterCooldown(normalizeThrowableMessage(error), retryConfig.failureCooldownMs);
        }
        throw error;
      }
    }

    throw lastError ?? createChatError('聊天接口调用失败', {
      code: 'CHAT_BACKEND_UNKNOWN',
      silent: true
    });
  }
}
