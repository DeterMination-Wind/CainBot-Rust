import { isNonEmptyString } from './utils.mjs';
import { OpenAiChatClient } from './openai-chat-client.mjs';

function normalizeInput(input) {
  if (typeof input === 'string') {
    const text = String(input).trim();
    return {
      text,
      images: []
    };
  }

  return {
    text: String(input?.text ?? '').trim(),
    images: Array.isArray(input?.images) ? input.images.filter(Boolean) : []
  };
}

export class OpenAiTranslator {
  constructor(config, logger) {
    this.config = config;
    this.logger = logger;
    this.chatClient = new OpenAiChatClient(config, logger);
  }

  get enabled() {
    return this.config.enabled !== false;
  }

  validate() {
    if (!isNonEmptyString(this.config.baseUrl)) {
      throw new Error('translation.baseUrl 未配置');
    }
    if (!isNonEmptyString(this.config.model)) {
      throw new Error('translation.model 未配置');
    }
  }

  async translate(input) {
    this.validate();
    const normalized = normalizeInput(input);
    if (!normalized.text && normalized.images.length === 0) {
      throw new Error('没有可翻译的内容');
    }

    const systemPrompt = isNonEmptyString(this.config.systemPrompt)
      ? this.config.systemPrompt
      : `你是专业翻译助手。请把用户提供的文本或图片中的文字翻译成${this.config.targetLanguage || '简体中文'}，只返回译文。`;

    const userContent = normalized.images.length > 0
      ? [
          {
            type: 'text',
            text: normalized.text || `请识别图片中的文字并翻译成${this.config.targetLanguage || '简体中文'}。`
          },
          ...normalized.images
        ]
      : normalized.text;

    const translated = await this.chatClient.complete([
      {
        role: 'system',
        content: systemPrompt
      },
      {
        role: 'user',
        content: userContent
      }
    ], {
      model: this.config.model,
      temperature: this.config.temperature ?? 0.2
    });
    if (!translated) {
      throw new Error('翻译接口未返回可用文本');
    }
    return translated;
  }
}
