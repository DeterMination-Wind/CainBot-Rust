use anyhow::{Result, bail};
use serde_json::{Value, json};

use crate::openai_chat_client::{ChatMessage, CompleteOptions, OpenAiChatClient};

#[derive(Clone)]
pub struct OpenAiTranslatorConfig {
    pub enabled: bool,
    pub model: String,
    pub target_language: String,
    pub temperature: f64,
    pub system_prompt: String,
}

#[derive(Clone)]
pub struct OpenAiTranslator {
    config: OpenAiTranslatorConfig,
    chat_client: OpenAiChatClient,
}

impl OpenAiTranslator {
    pub fn new(config: OpenAiTranslatorConfig, chat_client: OpenAiChatClient) -> Self {
        Self {
            config,
            chat_client,
        }
    }

    pub async fn translate(&self, input: TranslationInput) -> Result<String> {
        if self.config.model.trim().is_empty() {
            bail!("translation.model 未配置");
        }
        let normalized = input.normalize();
        if normalized.text.is_empty() && normalized.images.is_empty() {
            bail!("没有可翻译的内容");
        }

        let user_content = if normalized.images.is_empty() {
            Value::String(normalized.text)
        } else {
            let mut parts = vec![json!({
                "type": "text",
                "text": if normalized.text.is_empty() {
                    format!("请识别图片中的文字并翻译成{}。", self.config.target_language)
                } else {
                    normalized.text
                }
            })];
            parts.extend(normalized.images);
            Value::Array(parts)
        };

        self.chat_client
            .complete(
                &[
                    ChatMessage {
                        role: "system".to_string(),
                        content: Value::String(self.config.system_prompt.clone()),
                    },
                    ChatMessage {
                        role: "user".to_string(),
                        content: user_content,
                    },
                ],
                CompleteOptions {
                    model: Some(self.config.model.clone()),
                    temperature: Some(self.config.temperature),
                },
            )
            .await
    }
}

#[derive(Debug, Clone)]
pub enum TranslationInput {
    Text(String),
    Rich { text: String, images: Vec<Value> },
}

#[derive(Debug, Clone)]
struct NormalizedTranslationInput {
    text: String,
    images: Vec<Value>,
}

impl TranslationInput {
    fn normalize(self) -> NormalizedTranslationInput {
        match self {
            Self::Text(text) => NormalizedTranslationInput {
                text: text.trim().to_string(),
                images: Vec::new(),
            },
            Self::Rich { text, images } => NormalizedTranslationInput {
                text: text.trim().to_string(),
                images,
            },
        }
    }
}
