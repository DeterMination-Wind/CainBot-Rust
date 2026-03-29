use std::path::Path;

use anyhow::Result;
use base64::Engine;
use chrono::{DateTime, Local, TimeZone, Utc};
use serde_json::{Value, json};
use tokio::fs;

use crate::event_utils::{EventContext, create_context_from_event, get_sender_name, plain_text_from_message};
use crate::message_attachment_reader::{ReadTextFilesResult, read_text_files_from_message};
use crate::napcat_client::NapCatClient;
use crate::openai_translator::TranslationInput;
use crate::utils::path_exists;

const IMAGE_MIME_BY_EXTENSION: &[(&str, &str)] = &[
    (".png", "image/png"),
    (".jpg", "image/jpeg"),
    (".jpeg", "image/jpeg"),
    (".gif", "image/gif"),
    (".webp", "image/webp"),
    (".bmp", "image/bmp"),
    (".svg", "image/svg+xml"),
];

#[derive(Debug, Clone, Default)]
pub struct BuildChatInputOptions {
    pub argument: String,
    pub allow_current_text_fallback: bool,
    pub ai_runtime_prefix: String,
}

#[derive(Debug, Clone)]
pub struct ChatRuntimeContext {
    pub context: EventContext,
    pub sender_name: String,
    pub timeline_text: String,
    pub current_message_id: String,
    pub current_time: i64,
}

#[derive(Debug, Clone)]
pub struct ChatInput {
    pub text: String,
    pub images: Vec<Value>,
    pub history_text: String,
    pub runtime_context: ChatRuntimeContext,
}

#[derive(Debug, Clone, Default)]
pub struct TranslationSource {
    pub text: String,
    pub images: Vec<Value>,
}

impl ChatInput {
    pub fn has_content(&self) -> bool {
        !self.text.trim().is_empty() || !self.images.is_empty()
    }

    pub fn to_openai_user_content(&self) -> Value {
        if self.images.is_empty() {
            return Value::String(self.text.trim().to_string());
        }

        let mut parts = Vec::new();
        let prompt_text = if self.text.trim().is_empty() {
            format!("请结合这 {} 张图片回答用户。", self.images.len())
        } else {
            self.text.trim().to_string()
        };
        parts.push(json!({
            "type": "text",
            "text": prompt_text
        }));
        parts.extend(self.images.iter().cloned());
        Value::Array(parts)
    }
}

impl TranslationSource {
    pub fn has_content(&self) -> bool {
        !self.text.trim().is_empty() || !self.images.is_empty()
    }

    pub fn into_translation_input(self) -> TranslationInput {
        if self.images.is_empty() {
            TranslationInput::Text(self.text)
        } else {
            TranslationInput::Rich {
                text: self.text,
                images: self.images,
            }
        }
    }
}

// 这里复刻原版 buildChatInput / buildTranslationInput 的核心路径：
// 命令参数、引用消息、图片、文本附件会被收敛成给 OpenAI 的统一输入。
pub async fn build_chat_input(
    napcat_client: &NapCatClient,
    event: &Value,
    options: BuildChatInputOptions,
) -> Result<ChatInput> {
    let context = create_context_from_event(event);
    let explicit_text = options.argument.trim().to_string();
    let reply_id = extract_reply_id(
        event.get("message").unwrap_or(&Value::Null),
        event.get("raw_message").and_then(Value::as_str),
    );
    let replied_message = try_get_message(napcat_client, reply_id.as_deref()).await;

    let current_images = extract_openai_images_from_message(event.get("message").unwrap_or(&Value::Null)).await;
    let reply_images = if let Some(replied_message) = replied_message.as_ref() {
        extract_openai_images_from_message(replied_message.get("message").unwrap_or(&Value::Null)).await
    } else {
        Vec::new()
    };
    let current_files = read_text_files_from_message(
        napcat_client,
        event.get("message").unwrap_or(&Value::Null),
        3,
        12_000,
    )
    .await;
    let reply_files = if let Some(replied_message) = replied_message.as_ref() {
        read_text_files_from_message(
            napcat_client,
            replied_message.get("message").unwrap_or(&Value::Null),
            3,
            12_000,
        )
        .await
    } else {
        ReadTextFilesResult::default()
    };

    let current_text = if options.allow_current_text_fallback {
        plain_text_from_message(
            event.get("message").unwrap_or(&Value::Null),
            event.get("raw_message").and_then(Value::as_str),
        )
    } else {
        String::new()
    };
    let reply_text = replied_message
        .as_ref()
        .map(|message| {
            plain_text_from_message(
                message.get("message").unwrap_or(&Value::Null),
                message.get("raw_message").and_then(Value::as_str),
            )
        })
        .unwrap_or_default();
    let effective_text = if !explicit_text.is_empty() {
        explicit_text
    } else {
        current_text
    };
    let combined_images = [reply_images, current_images].concat();
    let timeline_text = if !effective_text.trim().is_empty() {
        effective_text.clone()
    } else if !reply_text.trim().is_empty() {
        reply_text.clone()
    } else if !combined_images.is_empty() {
        format!("附带图片 {} 张", combined_images.len())
    } else if !current_files.files.is_empty() {
        format!("附带文本文件 {} 个", current_files.files.len())
    } else {
        "发起了一次问答请求".to_string()
    };

    let mut sections = Vec::new();
    if !options.ai_runtime_prefix.trim().is_empty() {
        sections.push(options.ai_runtime_prefix.trim().to_string());
    }
    sections.push(build_request_context_prefix(&context, event));
    if !effective_text.is_empty() {
        sections.push(format!("用户当前消息：{effective_text}"));
    }
    if !reply_text.is_empty() {
        sections.push(format!("引用消息文本：{reply_text}"));
    }
    sections.extend(format_text_file_sections("当前消息附件文本", &current_files));
    sections.extend(format_text_file_sections("引用消息附件文本", &reply_files));

    let mut text = sections
        .into_iter()
        .filter(|item| !item.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
        .trim()
        .to_string();
    if text.is_empty() && !combined_images.is_empty() {
        text = format!("请结合这 {} 张图片回答用户。", combined_images.len());
    } else if !text.is_empty() && !combined_images.is_empty() {
        text = format!("{text}\n\n请结合附带的 {} 张图片一起回答。", combined_images.len());
    }

    Ok(ChatInput {
        history_text: text.clone(),
        runtime_context: ChatRuntimeContext {
            context,
            sender_name: get_sender_name(event),
            timeline_text,
            current_message_id: event
                .get("message_id")
                .map(value_to_compact_string)
                .unwrap_or_default(),
            current_time: event.get("time").and_then(Value::as_i64).unwrap_or_default(),
        },
        text,
        images: combined_images,
    })
}

pub async fn build_translation_input(
    napcat_client: &NapCatClient,
    event: &Value,
    explicit_text: &str,
) -> Result<TranslationSource> {
    let reply_id = extract_reply_id(
        event.get("message").unwrap_or(&Value::Null),
        event.get("raw_message").and_then(Value::as_str),
    );
    let replied_message = try_get_message(napcat_client, reply_id.as_deref()).await;
    let reply_text = replied_message
        .as_ref()
        .map(|message| {
            plain_text_from_message(
                message.get("message").unwrap_or(&Value::Null),
                message.get("raw_message").and_then(Value::as_str),
            )
        })
        .unwrap_or_default();
    let current_files = read_text_files_from_message(
        napcat_client,
        event.get("message").unwrap_or(&Value::Null),
        3,
        12_000,
    )
    .await;
    let reply_files = if let Some(replied_message) = replied_message.as_ref() {
        read_text_files_from_message(
            napcat_client,
            replied_message.get("message").unwrap_or(&Value::Null),
            3,
            12_000,
        )
        .await
    } else {
        ReadTextFilesResult::default()
    };
    let current_images = extract_openai_images_from_message(event.get("message").unwrap_or(&Value::Null)).await;
    let reply_images = if let Some(replied_message) = replied_message.as_ref() {
        extract_openai_images_from_message(replied_message.get("message").unwrap_or(&Value::Null)).await
    } else {
        Vec::new()
    };

    let mut parts = Vec::new();
    if !explicit_text.trim().is_empty() {
        parts.push(explicit_text.trim().to_string());
    } else if !reply_text.is_empty() {
        parts.push(reply_text);
    }
    parts.extend(format_text_file_sections("当前消息附件文本", &current_files));
    parts.extend(format_text_file_sections("引用消息附件文本", &reply_files));

    Ok(TranslationSource {
        text: parts
            .into_iter()
            .filter(|item| !item.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n\n")
            .trim()
            .to_string(),
        images: [reply_images, current_images].concat(),
    })
}

fn build_request_context_prefix(context: &EventContext, event: &Value) -> String {
    let target = if context.message_type == "group" {
        format!("群 {}", context.group_id)
    } else {
        format!("私聊 {}", context.user_id)
    };
    [
        format!(
            "当前请求时间：{}",
            format_epoch_time(event.get("time").and_then(Value::as_i64).unwrap_or_default())
        ),
        format!("当前消息来源：{target}"),
        format!("当前发送者：{} ({})", get_sender_name(event), context.user_id),
        format!(
            "当前消息ID：{}",
            event.get("message_id").map(value_to_compact_string).unwrap_or_else(|| "-".to_string())
        ),
    ]
    .join("\n")
}

fn format_text_file_sections(label: &str, result: &ReadTextFilesResult) -> Vec<String> {
    let mut sections = Vec::new();
    for (index, file) in result.files.iter().enumerate() {
        let mut lines = vec![format!("{label}{}：{}", index + 1, file.file_name)];
        if file.truncated {
            lines.push("以下内容已截断。".to_string());
        }
        lines.push(file.text.clone());
        sections.push(lines.join("\n"));
    }
    if !result.errors.is_empty() {
        sections.push(format!("{label}读取失败：{}", result.errors.join("；")));
    }
    sections
}

async fn try_get_message(napcat_client: &NapCatClient, message_id: Option<&str>) -> Option<Value> {
    let normalized = message_id.map(str::trim).unwrap_or_default();
    if normalized.is_empty() {
        return None;
    }
    napcat_client.get_message(normalized).await.ok()
}

fn extract_reply_id(message: &Value, raw_message: Option<&str>) -> Option<String> {
    if let Some(items) = message.as_array()
        && let Some(reply) = items.iter().find(|segment| {
            segment.get("type").and_then(Value::as_str) == Some("reply")
                && segment.get("data").and_then(|data| data.get("id")).is_some()
        })
    {
        return reply
            .get("data")
            .and_then(|data| data.get("id"))
            .map(value_to_compact_string)
            .filter(|item| !item.is_empty());
    }
    if let Some(message_type) = message.get("type").and_then(Value::as_str)
        && message_type == "reply"
        && let Some(reply_id) = message.get("data").and_then(|data| data.get("id"))
    {
        return Some(value_to_compact_string(reply_id));
    }
    raw_message.and_then(|raw| {
        let text = raw.trim();
        let marker = "[CQ:reply,id=";
        let start = text.find(marker)?;
        let remain = &text[start + marker.len()..];
        let end = remain.find([',', ']']).unwrap_or(remain.len());
        let reply_id = remain[..end].trim();
        (!reply_id.is_empty()).then(|| reply_id.to_string())
    })
}

async fn extract_openai_images_from_message(message: &Value) -> Vec<Value> {
    let mut images = Vec::new();
    if let Some(items) = message.as_array() {
        for segment in items {
            if segment.get("type").and_then(Value::as_str) != Some("image") {
                continue;
            }
            if let Some(data) = segment.get("data")
                && let Some(image_content) = image_segment_to_openai_content(data).await
            {
                images.push(image_content);
            }
        }
    } else if let Some(message_type) = message.get("type").and_then(Value::as_str)
        && message_type == "image"
        && let Some(data) = message.get("data")
        && let Some(image_content) = image_segment_to_openai_content(data).await
    {
        images.push(image_content);
    }
    images
}

async fn image_segment_to_openai_content(data: &Value) -> Option<Value> {
    for key in ["url", "file", "path", "thumb"] {
        let Some(candidate) = data.get(key).and_then(Value::as_str).map(str::trim) else {
            continue;
        };
        if candidate.is_empty() {
            continue;
        }
        if candidate.starts_with("data:") || candidate.starts_with("http://") || candidate.starts_with("https://") {
            return Some(json!({
                "type": "image_url",
                "image_url": { "url": candidate }
            }));
        }
        if let Some(local_path) = candidate.strip_prefix("file://")
            && path_exists(local_path).await
            && let Ok(data_url) = to_data_url_from_local_image(local_path).await
        {
            return Some(json!({
                "type": "image_url",
                "image_url": { "url": data_url }
            }));
        }
        if Path::new(candidate).is_absolute()
            && path_exists(candidate).await
            && let Ok(data_url) = to_data_url_from_local_image(candidate).await
        {
            return Some(json!({
                "type": "image_url",
                "image_url": { "url": data_url }
            }));
        }
    }
    None
}

async fn to_data_url_from_local_image(file_path: &str) -> Result<String> {
    let buffer = fs::read(file_path).await?;
    let extension = Path::new(file_path)
        .extension()
        .and_then(|value| value.to_str())
        .map(|value| format!(".{}", value.to_ascii_lowercase()))
        .unwrap_or_default();
    let mime = IMAGE_MIME_BY_EXTENSION
        .iter()
        .find(|(item, _)| *item == extension)
        .map(|(_, mime)| *mime)
        .unwrap_or("application/octet-stream");
    Ok(format!(
        "data:{mime};base64,{}",
        base64::engine::general_purpose::STANDARD.encode(buffer)
    ))
}

fn format_epoch_time(seconds: i64) -> String {
    if seconds <= 0 {
        return "-".to_string();
    }
    if let Some(utc) = Utc.timestamp_opt(seconds, 0).single() {
        let local: DateTime<Local> = utc.with_timezone(&Local);
        return local.format("%Y-%m-%d %H:%M:%S").to_string();
    }
    "-".to_string()
}

fn value_to_compact_string(value: &Value) -> String {
    match value {
        Value::String(text) => text.trim().to_string(),
        Value::Number(number) => number.to_string(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use serde_json::Value;

    use super::extract_reply_id;

    #[test]
    fn extracts_reply_id_from_segment() {
        let message = json!([
            { "type": "reply", "data": { "id": "12345" } }
        ]);
        assert_eq!(extract_reply_id(&message, None).as_deref(), Some("12345"));
    }

    #[test]
    fn extracts_reply_id_from_raw_message() {
        assert_eq!(
            extract_reply_id(&Value::Null, Some("[CQ:reply,id=54321][CQ:at,qq=1] hi")).as_deref(),
            Some("54321")
        );
    }
}
