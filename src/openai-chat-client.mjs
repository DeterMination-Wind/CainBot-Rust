import { isNonEmptyString, joinUrl, sleep } from './utils.mjs';

const RETRYABLE_HTTP_STATUS = new Set([408, 409, 425, 429, 500, 502, 503, 504]);
const RESPONSE_FALLBACK_HTTP_STATUS = new Set([400, 404, 405, 408, 409, 415, 425, 429, 500, 502, 503, 504]);
const TRANSPORT_SUPPRESS_MS = 10 * 60 * 1000;
const MODEL_ALIAS_CANDIDATES = new Map([
  ['gpt-5-codex-mini', ['gpt-5.1-codex-mini']],
  ['gpt-5-codex', ['gpt-5.1-codex']]
]);

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
      silent: true,
      httpStatus: Number(status) || 0
    }
  );
}

function isInvalidResponseError(error) {
  return error?.code === 'CHAT_BACKEND_INVALID_RESPONSE';
}

function shouldFallbackModel(error) {
  if (!error) {
    return false;
  }
  if (isInvalidResponseError(error) || isRetryableError(error)) {
    return true;
  }
  const status = Number(error.httpStatus ?? 0);
  return status === 400 || status === 404 || status === 405;
}

function buildModelCandidates(model) {
  const normalized = String(model ?? '').trim();
  if (!normalized) {
    return [];
  }
  return Array.from(new Set([
    normalized,
    ...(MODEL_ALIAS_CANDIDATES.get(normalized) ?? [])
  ]));
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

function extractResponseTextFromContentItems(items) {
  return (Array.isArray(items) ? items : [])
    .map((item) => {
      if (typeof item === 'string') {
        return item;
      }
      if (!item || typeof item !== 'object') {
        return '';
      }
      if (typeof item.text === 'string') {
        return item.text;
      }
      if (typeof item.output_text === 'string') {
        return item.output_text;
      }
      if (typeof item.value === 'string') {
        return item.value;
      }
      return '';
    })
    .join('');
}

function extractResponsesApiText(payload) {
  const directOutputText = payload?.output_text;
  if (typeof directOutputText === 'string') {
    return directOutputText.trim();
  }
  if (Array.isArray(directOutputText)) {
    const joined = extractResponseTextFromContentItems(directOutputText).trim();
    if (joined) {
      return joined;
    }
  }

  const outputs = Array.isArray(payload?.output)
    ? payload.output
    : Array.isArray(payload?.response?.output)
      ? payload.response.output
      : [];
  const joinedOutput = outputs
    .map((item) => extractResponseTextFromContentItems(item?.content))
    .join('')
    .trim();
  if (joinedOutput) {
    return joinedOutput;
  }

  const completedResponse = payload?.response;
  if (completedResponse && completedResponse !== payload) {
    return extractResponsesApiText(completedResponse);
  }

  return '';
}

function normalizeMessageRole(role) {
  const normalized = String(role ?? '').trim().toLowerCase();
  if (normalized === 'system' || normalized === 'developer' || normalized === 'assistant' || normalized === 'tool') {
    return normalized;
  }
  return 'user';
}

function messageContentToResponsesInput(content) {
  if (typeof content === 'string') {
    const text = content.trim();
    return text
      ? [{ type: 'input_text', text }]
      : [];
  }

  return (Array.isArray(content) ? content : [])
    .flatMap((item) => {
      if (typeof item === 'string') {
        const text = item.trim();
        return text ? [{ type: 'input_text', text }] : [];
      }
      if (!item || typeof item !== 'object') {
        return [];
      }
      if (item.type === 'text' || item.type === 'input_text') {
        const text = String(item.text ?? '').trim();
        return text ? [{ type: 'input_text', text }] : [];
      }
      if (item.type === 'image_url' || item.type === 'input_image') {
        const imageUrl = typeof item.image_url === 'string'
          ? item.image_url
          : item.image_url?.url;
        const normalizedUrl = String(imageUrl ?? '').trim();
        if (!normalizedUrl) {
          return [];
        }
        return [{
          type: 'input_image',
          image_url: normalizedUrl
        }];
      }
      return [];
    });
}

function buildResponsesInput(messages) {
  return (Array.isArray(messages) ? messages : [])
    .map((message) => {
      const content = messageContentToResponsesInput(message?.content);
      if (content.length === 0) {
        return null;
      }
      return {
        role: normalizeMessageRole(message?.role),
        content
      };
    })
    .filter(Boolean);
}

function messageContentToPlainText(content) {
  if (typeof content === 'string') {
    return content.trim();
  }
  return (Array.isArray(content) ? content : [])
    .map((item) => {
      if (typeof item === 'string') {
        return item.trim();
      }
      if (!item || typeof item !== 'object') {
        return '';
      }
      if (item.type === 'text' || item.type === 'input_text') {
        return String(item.text ?? '').trim();
      }
      return '';
    })
    .filter(Boolean)
    .join('\n');
}

function canFlattenMessages(messages) {
  return (Array.isArray(messages) ? messages : []).every((message) => {
    const content = message?.content;
    if (typeof content === 'string') {
      return true;
    }
    return (Array.isArray(content) ? content : []).every((item) => {
      if (typeof item === 'string') {
        return true;
      }
      return item?.type === 'text' || item?.type === 'input_text';
    });
  });
}

function buildFlattenedResponsesInput(messages) {
  if (!canFlattenMessages(messages)) {
    return '';
  }
  return (Array.isArray(messages) ? messages : [])
    .map((message) => {
      const role = normalizeMessageRole(message?.role).toUpperCase();
      const text = messageContentToPlainText(message?.content);
      return text ? `${role}:\n${text}` : '';
    })
    .filter(Boolean)
    .join('\n\n')
    .trim();
}

function buildResponsesRequestVariants(messages, baseUrl) {
  const structuredInput = buildResponsesInput(messages);
  const variants = [];
  if (structuredInput.length > 0) {
    variants.push({
      name: 'structured-input_text',
      body: { input: structuredInput }
    });
  }

  const flattenedInput = buildFlattenedResponsesInput(messages);
  if (flattenedInput && !isCcSwitchProxy(baseUrl)) {
    variants.push({
      name: 'flattened-string',
      body: { input: flattenedInput }
    });
  }

  return variants;
}

function shouldPreferResponsesApi(baseUrl) {
  const normalizedBaseUrl = String(baseUrl ?? '').trim();
  if (!normalizedBaseUrl) {
    return false;
  }
  try {
    const parsed = new URL(normalizedBaseUrl);
    const host = String(parsed.hostname ?? '').trim().toLowerCase();
    const port = String(parsed.port ?? '').trim();
    const pathname = String(parsed.pathname ?? '').trim().toLowerCase();
    return (host === '127.0.0.1' || host === 'localhost') && port === '15721' && pathname.includes('/v1');
  } catch {
    return /127\.0\.0\.1:15721\/v1|localhost:15721\/v1/i.test(normalizedBaseUrl);
  }
}

function isCcSwitchProxy(baseUrl) {
  const normalizedBaseUrl = String(baseUrl ?? '').trim();
  if (!normalizedBaseUrl) {
    return false;
  }
  try {
    const parsed = new URL(normalizedBaseUrl);
    const host = String(parsed.hostname ?? '').trim().toLowerCase();
    const port = String(parsed.port ?? '').trim();
    const pathname = String(parsed.pathname ?? '').trim().toLowerCase();
    return (host === '127.0.0.1' || host === 'localhost') && port === '15721' && pathname.includes('/v1');
  } catch {
    return /127\.0\.0\.1:15721\/v1|localhost:15721\/v1/i.test(normalizedBaseUrl);
  }
}

function getTransportOrder(baseUrl) {
  return shouldPreferResponsesApi(baseUrl)
    ? ['responses', 'chat']
    : ['chat', 'responses'];
}

function shouldFallbackTransport(error) {
  if (!error) {
    return false;
  }
  return isRetryableError(error)
    || error.code === 'CHAT_BACKEND_HTTP_ERROR'
    || RESPONSE_FALLBACK_HTTP_STATUS.has(Number(error.httpStatus ?? 0));
}

async function readResponseText(response) {
  const contentType = String(response.headers.get('content-type') ?? '').toLowerCase();
  if (contentType.includes('text/event-stream')) {
    return await readResponsesEventStream(response);
  }

  const payload = await response.json();
  const assistantText = extractResponsesApiText(payload);
  if (!assistantText) {
    throw createChatError('聊天接口未返回可用文本', {
      code: 'CHAT_BACKEND_INVALID_RESPONSE',
      silent: true
    });
  }
  return assistantText;
}

function getEventDataType(payload, fallbackType = '') {
  return String(payload?.type ?? fallbackType ?? '').trim();
}

async function readResponsesEventStream(response) {
  if (!response.body) {
    throw createChatError('聊天接口返回了空流', {
      code: 'CHAT_BACKEND_INVALID_RESPONSE',
      silent: true
    });
  }

  const decoder = new TextDecoder();
  let buffer = '';
  let deltaText = '';
  let doneText = '';
  let completedPayload = null;

  const processEventBlock = (block) => {
    const normalizedBlock = String(block ?? '').trim();
    if (!normalizedBlock) {
      return;
    }
    let eventName = '';
    const dataLines = [];
    for (const rawLine of normalizedBlock.split(/\r?\n/)) {
      const line = String(rawLine ?? '');
      if (line.startsWith('event:')) {
        eventName = line.slice(6).trim();
        continue;
      }
      if (line.startsWith('data:')) {
        dataLines.push(line.slice(5).trimStart());
      }
    }
    const dataText = dataLines.join('\n').trim();
    if (!dataText || dataText === '[DONE]') {
      return;
    }
    let payload = null;
    try {
      payload = JSON.parse(dataText);
    } catch {
      return;
    }

    const eventType = getEventDataType(payload, eventName);
    if (eventType === 'response.output_text.delta') {
      deltaText += String(payload?.delta ?? payload?.text_delta ?? '');
      return;
    }
    if (eventType === 'response.output_text.done') {
      if (!deltaText) {
        doneText += String(payload?.text ?? payload?.delta ?? payload?.output_text ?? '');
      }
      return;
    }
    if (eventType === 'response.completed') {
      completedPayload = payload?.response ?? payload;
    }
  };

  for await (const chunk of response.body) {
    buffer += decoder.decode(chunk, { stream: true });
    let separatorIndex = buffer.indexOf('\n\n');
    while (separatorIndex >= 0) {
      const eventBlock = buffer.slice(0, separatorIndex);
      buffer = buffer.slice(separatorIndex + 2);
      processEventBlock(eventBlock);
      separatorIndex = buffer.indexOf('\n\n');
    }
  }
  buffer += decoder.decode();
  if (buffer.trim()) {
    processEventBlock(buffer);
  }

  const finalText = deltaText.trim()
    || doneText.trim()
    || extractResponsesApiText(completedPayload);
  if (!finalText) {
    throw createChatError('聊天接口未返回可用文本', {
      code: 'CHAT_BACKEND_INVALID_RESPONSE',
      silent: true
    });
  }
  return finalText;
}

export class OpenAiChatClient {
  constructor(config, logger) {
    this.config = config;
    this.logger = logger;
    this.cooldownUntil = 0;
    this.cooldownReason = '';
    this.retryableFailureStreak = 0;
    this.transportSuppressedUntil = new Map();
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
      failureCooldownMs: Math.max(1000, Number(this.config.failureCooldownMs ?? 60000) || 60000),
      failureCooldownThreshold: Math.max(1, Number(this.config.failureCooldownThreshold ?? 2) || 2)
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

  #isTransportSuppressed(transport) {
    const until = Number(this.transportSuppressedUntil.get(transport) ?? 0);
    return until > Date.now();
  }

  #suppressTransport(transport, reason, durationMs = TRANSPORT_SUPPRESS_MS) {
    const until = Date.now() + durationMs;
    const previousUntil = Number(this.transportSuppressedUntil.get(transport) ?? 0);
    if (previousUntil >= until) {
      return;
    }
    this.transportSuppressedUntil.set(transport, until);
    this.logger.warn(
      `聊天接口 ${transport} 暂时熔断 ${Math.ceil(durationMs / 1000)} 秒：${reason}`
    );
  }

  #restoreTransport(transport) {
    this.transportSuppressedUntil.delete(transport);
  }

  #getAvailableTransports() {
    const preferred = getTransportOrder(this.config.baseUrl);
    const available = preferred.filter((transport) => !this.#isTransportSuppressed(transport));
    return available.length > 0 ? available : preferred;
  }

  async #completeViaChatCompletions(messages, options, headers, retryConfig) {
    const modelCandidates = buildModelCandidates(options.model ?? this.config.model);

    for (let modelIndex = 0; modelIndex < modelCandidates.length; modelIndex += 1) {
      const model = modelCandidates[modelIndex];
      for (let attempt = 1; attempt <= retryConfig.maxAttempts; attempt += 1) {
        try {
          const response = await fetch(joinUrl(this.config.baseUrl, 'chat/completions'), {
            method: 'POST',
            headers,
            signal: AbortSignal.timeout(retryConfig.requestTimeoutMs),
            body: JSON.stringify({
              model,
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
          if (attempt < retryConfig.maxAttempts && isRetryableError(error)) {
            const delayMs = retryConfig.baseDelayMs * attempt;
            this.logger.warn(`聊天接口请求异常，准备重试（${attempt}/${retryConfig.maxAttempts}）：${normalizeThrowableMessage(error)}`);
            await sleep(delayMs);
            continue;
          }
          const hasAlternateModel = modelIndex < modelCandidates.length - 1;
          if (hasAlternateModel && shouldFallbackModel(error)) {
            const nextModel = modelCandidates[modelIndex + 1];
            this.logger.warn(`聊天接口 chat 当前模型 ${model} 不稳定，切换到 ${nextModel}：${normalizeThrowableMessage(error)}`);
            break;
          }
          throw error;
        }
      }
    }

    throw createChatError('聊天接口调用失败', {
      code: 'CHAT_BACKEND_UNKNOWN',
      silent: true
    });
  }

  async #completeViaResponsesApi(messages, options, headers, retryConfig) {
    const variants = buildResponsesRequestVariants(messages, this.config.baseUrl);
    if (variants.length === 0) {
      throw createChatError('聊天接口未提供可发送内容', {
        code: 'CHAT_BACKEND_INVALID_REQUEST',
        silent: true
      });
    }

    const modelCandidates = buildModelCandidates(options.model ?? this.config.model);
    for (let variantIndex = 0; variantIndex < variants.length; variantIndex += 1) {
      const variant = variants[variantIndex];
      for (let modelIndex = 0; modelIndex < modelCandidates.length; modelIndex += 1) {
        const model = modelCandidates[modelIndex];
        for (let attempt = 1; attempt <= retryConfig.maxAttempts; attempt += 1) {
          try {
            const response = await fetch(joinUrl(this.config.baseUrl, 'responses'), {
              method: 'POST',
              headers,
              signal: AbortSignal.timeout(retryConfig.requestTimeoutMs),
              body: JSON.stringify({
                model,
                temperature: options.temperature ?? this.config.temperature ?? 0.7,
                ...variant.body
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
              throw httpError;
            }

            return await readResponseText(response);
          } catch (error) {
            if (attempt < retryConfig.maxAttempts && isRetryableError(error)) {
              const delayMs = retryConfig.baseDelayMs * attempt;
              this.logger.warn(`聊天接口请求异常，准备重试（${attempt}/${retryConfig.maxAttempts}）：${normalizeThrowableMessage(error)}`);
              await sleep(delayMs);
              continue;
            }
            const hasAlternateModel = modelIndex < modelCandidates.length - 1;
            if (hasAlternateModel && shouldFallbackModel(error)) {
              const nextModel = modelCandidates[modelIndex + 1];
              this.logger.warn(`聊天接口 responses 当前模型 ${model} 不稳定，切换到 ${nextModel}：${normalizeThrowableMessage(error)}`);
              break;
            }
            const hasAlternateVariant = variantIndex < variants.length - 1;
            if (!hasAlternateModel && hasAlternateVariant && shouldFallbackTransport(error)) {
              const nextVariant = variants[variantIndex + 1];
              this.logger.warn(`聊天接口 responses 当前载荷 ${variant.name} 不稳定，切换到 ${nextVariant.name}：${normalizeThrowableMessage(error)}`);
              break;
            }
            throw error;
          }
        }
      }
    }

    throw createChatError('聊天接口调用失败', {
      code: 'CHAT_BACKEND_UNKNOWN',
      silent: true
    });
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
    if (isCcSwitchProxy(this.config.baseUrl)) {
      headers.Connection = 'close';
    }

    const transports = this.#getAvailableTransports();
    const proxyManagedFailover = isCcSwitchProxy(this.config.baseUrl);
    let lastError = null;

    for (let index = 0; index < transports.length; index += 1) {
      const transport = transports[index];
      try {
        if (transport === 'responses') {
          const text = await this.#completeViaResponsesApi(messages, options, headers, retryConfig);
          this.retryableFailureStreak = 0;
          this.#restoreTransport(transport);
          return text;
        }
        const text = await this.#completeViaChatCompletions(messages, options, headers, retryConfig);
        this.retryableFailureStreak = 0;
        this.#restoreTransport(transport);
        return text;
      } catch (error) {
        lastError = error;
        if (proxyManagedFailover && transport === 'chat' && (isRetryableError(error) || isInvalidResponseError(error))) {
          this.#suppressTransport('chat', normalizeThrowableMessage(error));
        }
        const hasAlternateTransport = index < transports.length - 1;
        if (hasAlternateTransport && shouldFallbackTransport(error)) {
          const nextTransport = transports[index + 1];
          this.logger.warn(`聊天接口 ${transport} 不稳定，切换到 ${nextTransport}：${normalizeThrowableMessage(error)}`);
          continue;
        }
        break;
      }
    }

    if (isRetryableError(lastError) || RETRYABLE_HTTP_STATUS.has(Number(lastError?.httpStatus ?? 0))) {
      this.retryableFailureStreak += 1;
      if (!proxyManagedFailover && this.retryableFailureStreak >= retryConfig.failureCooldownThreshold) {
        throw this.enterCooldown(normalizeThrowableMessage(lastError), retryConfig.failureCooldownMs);
      }
      this.logger.warn(
        proxyManagedFailover
          ? `聊天接口失败，CC Switch 代理已接管整流，跳过本地冷却：${normalizeThrowableMessage(lastError)}`
          : `聊天接口连续失败 ${this.retryableFailureStreak}/${retryConfig.failureCooldownThreshold}，暂不进入冷却：${normalizeThrowableMessage(lastError)}`
      );
      throw lastError;
    }
    this.retryableFailureStreak = 0;
    throw lastError ?? createChatError('聊天接口调用失败', {
      code: 'CHAT_BACKEND_UNKNOWN',
      silent: true
    });
  }
}
