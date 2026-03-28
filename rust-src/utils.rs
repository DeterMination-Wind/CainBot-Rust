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
    let mut result = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '[' {
            let mut probe = String::new();
            probe.push(ch);
            let mut matched = false;
            while let Some(next) = chars.peek().copied() {
                probe.push(next);
                chars.next();
                if probe.starts_with("[CQ:") && next == ']' {
                    matched = true;
                    break;
                }
                if probe.len() > 512 {
                    break;
                }
            }
            if !matched {
                result.push_str(&probe);
            }
            continue;
        }
        result.push(ch);
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

pub fn build_reply_message(reply_to_message_id: Option<&str>, text: impl AsRef<str>) -> Value {
    let mut segments = Vec::new();
    if let Some(message_id) = reply_to_message_id.map(str::trim).filter(|item| !item.is_empty()) {
        segments.push(json!({
            "type": "reply",
            "data": { "id": message_id }
        }));
    }
    segments.push(json!({
        "type": "text",
        "data": { "text": text.as_ref() }
    }));
    Value::Array(segments)
}

pub fn split_text(text: &str, max_length: usize) -> Vec<String> {
    if text.len() <= max_length {
        return vec![text.to_string()];
    }

    let mut parts = Vec::new();
    let mut remaining = text.trim().to_string();
    while remaining.len() > max_length {
        let cut = remaining[..max_length]
            .rfind('\n')
            .filter(|index| *index >= max_length / 2)
            .unwrap_or(max_length);
        let head = remaining[..cut].trim().to_string();
        if !head.is_empty() {
            parts.push(head);
        }
        remaining = remaining[cut..].trim_start().to_string();
    }
    if !remaining.is_empty() {
        parts.push(remaining);
    }
    parts
}
