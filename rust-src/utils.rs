use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::Utc;
use serde_json::{Value, json};
use sha1::{Digest, Sha1};
use tokio::fs;
use url::Url;

pub async fn ensure_dir(dir_path: impl AsRef<Path>) -> Result<()> {
    fs::create_dir_all(dir_path.as_ref())
        .await
        .with_context(|| format!("创建目录失败: {}", dir_path.as_ref().display()))
}

pub async fn path_exists(target_path: impl AsRef<Path>) -> bool {
    fs::metadata(target_path.as_ref()).await.is_ok()
}

pub fn now_iso() -> String {
    Utc::now().to_rfc3339()
}

pub fn sha1_hex(value: impl AsRef<str>) -> String {
    let mut hasher = Sha1::new();
    hasher.update(value.as_ref().as_bytes());
    format!("{:x}", hasher.finalize())
}

pub async fn sleep_ms(ms: u64) {
    tokio::time::sleep(Duration::from_millis(ms)).await;
}

pub fn normalize_path(path: impl AsRef<Path>) -> PathBuf {
    path.as_ref().to_path_buf()
}

pub fn resolve_maybe_relative(base_dir: impl AsRef<Path>, target_path: impl AsRef<str>) -> Option<PathBuf> {
    let raw = target_path.as_ref().trim();
    if raw.is_empty() {
        return None;
    }
    let candidate = PathBuf::from(raw);
    if candidate.is_absolute() {
        Some(normalize_path(candidate))
    } else {
        Some(normalize_path(base_dir.as_ref().join(candidate)))
    }
}

pub fn join_url(base_url: &str, path_name: &str) -> Result<String> {
    let normalized = if base_url.ends_with('/') {
        base_url.to_owned()
    } else {
        format!("{base_url}/")
    };
    let base = Url::parse(&normalized).with_context(|| format!("非法 URL: {base_url}"))?;
    Ok(base
        .join(path_name)
        .with_context(|| format!("拼接 URL 失败: {base_url} + {path_name}"))?
        .to_string())
}

pub fn strip_cq_codes(text: &str) -> String {
    let chars = text.chars().collect::<Vec<_>>();
    let mut result = String::with_capacity(text.len());
    let mut index = 0usize;
    while index < chars.len() {
        if chars[index] == '[' {
            let looks_like_cq = index + 3 < chars.len()
                && chars[index + 1].eq_ignore_ascii_case(&'c')
                && chars[index + 2].eq_ignore_ascii_case(&'q')
                && chars[index + 3] == ':';
            if looks_like_cq {
                let mut cursor = index + 4;
                while cursor < chars.len() && chars[cursor] != ']' {
                    cursor += 1;
                }
                if cursor < chars.len() && chars[cursor] == ']' {
                    index = cursor + 1;
                    continue;
                }
            }
        }
        result.push(chars[index]);
        index += 1;
    }
    result.trim().to_string()
}

pub fn plain_text_from_message(message: &Value, raw_message: Option<&str>) -> String {
    if let Some(array) = message.as_array() {
        let text = array
            .iter()
            .filter(|segment| segment.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|segment| {
                segment
                    .get("data")
                    .and_then(|item| item.get("text"))
                    .and_then(Value::as_str)
            })
            .collect::<Vec<_>>()
            .join("")
            .trim()
            .to_string();
        if !text.is_empty() {
            return text;
        }
    }

    if let Some(object) = message.as_object()
        && object.get("type").and_then(Value::as_str) == Some("text")
        && let Some(text) = object
            .get("data")
            .and_then(|item| item.get("text"))
            .and_then(Value::as_str)
    {
        let normalized = text.trim().to_string();
        if !normalized.is_empty() {
            return normalized;
        }
    }

    if let Some(text) = message.as_str() {
        let normalized = strip_cq_codes(text);
        if !normalized.is_empty() {
            return normalized;
        }
    }

    strip_cq_codes(raw_message.unwrap_or_default())
}

pub fn build_reply_message(reply_to_message_id: Option<&str>, text: impl AsRef<str>, enable_mentions: bool) -> Value {
    build_message_payload(text.as_ref(), reply_to_message_id, enable_mentions)
}

pub fn build_message_payload(text: &str, reply_to_message_id: Option<&str>, enable_mentions: bool) -> Value {
    let mut segments = Vec::new();
    if let Some(message_id) = reply_to_message_id.map(str::trim).filter(|item| !item.is_empty()) {
        segments.push(json!({
            "type": "reply",
            "data": { "id": message_id }
        }));
    }
    segments.extend(parse_outgoing_text_segments(text, enable_mentions));
    Value::Array(segments)
}

pub fn split_message_payloads(text: &str, max_length: usize, enable_mentions: bool) -> Vec<Value> {
    let normalized_max = max_length.max(1);
    let mut chunks = Vec::<Vec<Value>>::new();
    let mut current = Vec::<Value>::new();
    let mut current_len = 0usize;

    for segment in parse_outgoing_tokens(text, enable_mentions) {
        match segment {
            OutgoingToken::Text(content) => {
                let mut remaining = content.as_str();
                while !remaining.is_empty() {
                    if current_len >= normalized_max && !current.is_empty() {
                        chunks.push(current);
                        current = Vec::new();
                        current_len = 0;
                    }
                    let available = normalized_max.saturating_sub(current_len).max(1);
                    let (piece, consumed) = take_text_prefix(remaining, available);
                    if !piece.is_empty() {
                        current.push(json!({
                            "type": "text",
                            "data": { "text": piece }
                        }));
                        current_len += piece.len();
                    }
                    if consumed >= remaining.len() {
                        break;
                    }
                    remaining = remaining[consumed..].trim_start();
                    if !current.is_empty() {
                        chunks.push(current);
                        current = Vec::new();
                        current_len = 0;
                    }
                }
            }
            OutgoingToken::At(qq) => {
                let segment_len = 0usize;
                if current_len > 0 && current_len + segment_len > normalized_max && !current.is_empty() {
                    chunks.push(current);
                    current = Vec::new();
                    current_len = 0;
                }
                current.push(json!({
                    "type": "at",
                    "data": { "qq": qq }
                }));
                current_len += segment_len;
            }
        }
    }

    if current.is_empty() {
        chunks.push(vec![json!({
            "type": "text",
            "data": { "text": "" }
        })]);
    } else {
        chunks.push(current);
    }

    chunks.into_iter().map(Value::Array).collect()
}

fn parse_outgoing_text_segments(text: &str, enable_mentions: bool) -> Vec<Value> {
    parse_outgoing_tokens(text, enable_mentions)
        .into_iter()
        .map(|token| match token {
            OutgoingToken::Text(content) => json!({
                "type": "text",
                "data": { "text": content }
            }),
            OutgoingToken::At(qq) => json!({
                "type": "at",
                "data": { "qq": qq }
            }),
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum OutgoingToken {
    Text(String),
    At(String),
}

fn parse_outgoing_tokens(text: &str, enable_mentions: bool) -> Vec<OutgoingToken> {
    if !enable_mentions {
        return vec![OutgoingToken::Text(text.to_string())];
    }

    let mut tokens = Vec::new();
    let mut index = 0usize;
    while index < text.len() {
        let Some(marker_offset) = text[index..].find("<<at:") else {
            push_text_token(&mut tokens, &text[index..]);
            break;
        };
        let marker_start = index + marker_offset;
        if marker_start > index {
            push_text_token(&mut tokens, &text[index..marker_start]);
        }
        let candidate = &text[marker_start..];
        let Some(marker_end) = candidate.find(">>") else {
            push_text_token(&mut tokens, candidate);
            break;
        };
        let qq = candidate["<<at:".len()..marker_end].trim();
        if qq.is_empty() || !qq.chars().all(|ch| ch.is_ascii_digit()) {
            push_text_token(&mut tokens, &candidate[..marker_end + 2]);
        } else {
            tokens.push(OutgoingToken::At(qq.to_string()));
        }
        index = marker_start + marker_end + 2;
    }

    if tokens.is_empty() {
        tokens.push(OutgoingToken::Text(String::new()));
    }
    tokens
}

fn push_text_token(tokens: &mut Vec<OutgoingToken>, text: &str) {
    if text.is_empty() {
        return;
    }
    if let Some(OutgoingToken::Text(existing)) = tokens.last_mut() {
        existing.push_str(text);
    } else {
        tokens.push(OutgoingToken::Text(text.to_string()));
    }
}

fn take_text_prefix(text: &str, max_length: usize) -> (String, usize) {
    if text.len() <= max_length {
        return (text.to_string(), text.len());
    }

    let mut safe_limit = max_length.min(text.len());
    while safe_limit > 0 && !text.is_char_boundary(safe_limit) {
        safe_limit -= 1;
    }
    if safe_limit == 0 {
        safe_limit = text.char_indices().nth(1).map(|(index, _)| index).unwrap_or(text.len());
    }

    let mut candidate = text[..safe_limit]
        .rfind('\n')
        .filter(|index| *index >= safe_limit / 2)
        .unwrap_or(safe_limit);
    while candidate > 0 && !text.is_char_boundary(candidate) {
        candidate -= 1;
    }
    if candidate == 0 {
        candidate = text.char_indices().nth(1).map(|(index, _)| index).unwrap_or(text.len());
    }
    (text[..candidate].trim().to_string(), candidate)
}

pub fn split_text(text: &str, max_length: usize) -> Vec<String> {
    let normalized_max = max_length.max(1);
    if text.len() <= normalized_max {
        return vec![text.to_string()];
    }

    let mut parts = Vec::new();
    let mut remaining = text.trim().to_string();
    while remaining.len() > normalized_max {
        let (head, consumed) = take_text_prefix(&remaining, normalized_max);
        if !head.is_empty() {
            parts.push(head);
        }
        if consumed >= remaining.len() {
            remaining.clear();
            break;
        }
        remaining = remaining[consumed..].trim_start().to_string();
    }
    if !remaining.is_empty() {
        parts.push(remaining);
    }
    parts
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{build_reply_message, split_message_payloads};

    #[test]
    fn reply_message_converts_at_placeholder_into_segment() {
        let message = build_reply_message(Some("42"), "hello <<at:123456>> world", true);
        assert_eq!(
            message,
            json!([
                { "type": "reply", "data": { "id": "42" } },
                { "type": "text", "data": { "text": "hello " } },
                { "type": "at", "data": { "qq": "123456" } },
                { "type": "text", "data": { "text": " world" } }
            ])
        );
    }

    #[test]
    fn reply_message_keeps_invalid_at_placeholder_as_text() {
        let message = build_reply_message(None, "hello <<at:abc>>", true);
        assert_eq!(
            message,
            json!([
                { "type": "text", "data": { "text": "hello <<at:abc>>" } }
            ])
        );
    }

    #[test]
    fn split_message_payloads_preserve_at_placeholder_boundaries() {
        let chunks = split_message_payloads("12345<<at:99>>67890", 6, true);
        assert_eq!(
            chunks,
            vec![
                json!([
                    { "type": "text", "data": { "text": "12345" } },
                    { "type": "at", "data": { "qq": "99" } },
                    { "type": "text", "data": { "text": "6" } }
                ]),
                json!([
                    { "type": "text", "data": { "text": "7890" } }
                ])
            ]
        );
    }

    #[test]
    fn split_text_handles_utf8_char_boundary() {
        let input = format!("{}{}", "a".repeat(299), "把后续内容发出来");
        let parts = super::split_text(&input, 300);
        assert!(!parts.is_empty());
        assert_eq!(parts.concat(), input);
    }
}
