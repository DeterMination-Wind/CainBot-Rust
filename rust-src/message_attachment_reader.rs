use std::path::Path;

use anyhow::{Context, Result, bail};
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::fs;

use crate::napcat_client::NapCatClient;

const TEXT_EXTENSIONS: &[&str] = &[
    ".txt", ".md", ".markdown", ".json", ".jsonc", ".yaml", ".yml", ".toml", ".ini", ".cfg", ".conf",
    ".js", ".mjs", ".cjs", ".ts", ".tsx", ".jsx", ".java", ".kt", ".kts", ".gradle", ".properties",
    ".xml", ".html", ".css", ".scss", ".less", ".py", ".rb", ".php", ".go", ".rs", ".cpp", ".c", ".h",
    ".hpp", ".cs", ".sh", ".ps1", ".bat", ".cmd", ".sql", ".csv", ".env", ".gitignore", ".gitattributes",
    ".vue", ".svelte", ".lua", ".log",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadTextFileResult {
    pub file_id: String,
    pub file_name: String,
    pub file_size: u64,
    pub text: String,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ReadTextFilesResult {
    pub files: Vec<ReadTextFileResult>,
    pub errors: Vec<String>,
}

// 只抽取 file 段，避免上层消息管线自己重复解析 OneBot segment。
pub fn extract_file_segments(message: &Value) -> Vec<Value> {
    message
        .as_array()
        .map(|segments| {
            segments
                .iter()
                .filter(|segment| segment.get("type").and_then(Value::as_str) == Some("file"))
                .filter_map(|segment| segment.get("data").cloned())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

pub async fn read_text_files_from_message(
    napcat_client: &NapCatClient,
    message: &Value,
    max_files: usize,
    max_chars: usize,
) -> ReadTextFilesResult {
    let mut results = ReadTextFilesResult::default();
    for segment in extract_file_segments(message).into_iter().take(max_files) {
        match read_text_file_from_segment(napcat_client, &segment, max_chars).await {
            Ok(item) => results.files.push(item),
            Err(error) => results.errors.push(format!("{error:#}")),
        }
    }
    results
}

pub async fn read_text_file_from_segment(
    napcat_client: &NapCatClient,
    file_segment: &Value,
    max_chars: usize,
) -> Result<ReadTextFileResult> {
    let file_id = file_segment
        .get("file_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .ok_or_else(|| anyhow::anyhow!("该消息文件缺少 file_id，无法读取"))?
        .to_string();

    let file_info = napcat_client
        .call("get_file", serde_json::json!({ "file_id": file_id }))
        .await?;
    let file_name = file_info
        .get("file_name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .or_else(|| file_segment.get("name").and_then(Value::as_str))
        .unwrap_or("未命名文件")
        .to_string();

    let text = if let Some(base64) = file_info.get("base64").and_then(Value::as_str) {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(base64)
            .context("解码文件 base64 失败")?;
        decode_text_bytes(&bytes, &file_name)?
    } else if let Some(path) = file_info.get("file").and_then(Value::as_str) {
        let bytes = fs::read(path).await.with_context(|| format!("读取文件失败: {path}"))?;
        decode_text_bytes(&bytes, &file_name)?
    } else if let Some(url) = file_info.get("url").and_then(Value::as_str) {
        let bytes = reqwest::get(url)
            .await
            .with_context(|| format!("下载附件失败: {url}"))?
            .error_for_status()
            .with_context(|| format!("下载附件返回非成功状态: {url}"))?
            .bytes()
            .await
            .with_context(|| format!("读取附件响应体失败: {url}"))?;
        decode_text_bytes(bytes.as_ref(), &file_name)?
    } else {
        bail!("NapCat 没有返回可读取的文件内容");
    };

    let normalized = text.replace('\0', "").trim().to_string();
    if normalized.is_empty() {
        bail!("文件为空，或无法解码为可读文本：{file_name}");
    }
    let (text, truncated) = truncate_text(&normalized, max_chars);
    Ok(ReadTextFileResult {
        file_id,
        file_name,
        file_size: file_info.get("file_size").and_then(Value::as_u64).unwrap_or(text.len() as u64),
        text,
        truncated,
    })
}

fn decode_text_bytes(bytes: &[u8], file_name: &str) -> Result<String> {
    if !has_text_extension(file_name) && !is_probably_text_buffer(bytes) {
        bail!("只支持纯文本文件，当前文件看起来不是文本：{file_name}");
    }
    if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return Ok(String::from_utf8_lossy(&bytes[3..]).to_string());
    }
    // QQ 群文件里 UTF-16 LE/BE 都见过，这里直接兼容两种 BOM。
    if bytes.starts_with(&[0xFF, 0xFE]) {
        let words = bytes[2..]
            .chunks(2)
            .map(|chunk| u16::from_le_bytes([chunk[0], *chunk.get(1).unwrap_or(&0)]))
            .collect::<Vec<_>>();
        return Ok(String::from_utf16_lossy(&words));
    }
    if bytes.starts_with(&[0xFE, 0xFF]) {
        let words = bytes[2..]
            .chunks(2)
            .map(|chunk| u16::from_be_bytes([chunk[0], *chunk.get(1).unwrap_or(&0)]))
            .collect::<Vec<_>>();
        return Ok(String::from_utf16_lossy(&words));
    }
    Ok(String::from_utf8_lossy(bytes).to_string())
}

fn has_text_extension(file_name: &str) -> bool {
    let lower = file_name.trim().to_ascii_lowercase();
    let extension = Path::new(&lower)
        .extension()
        .and_then(|item| item.to_str())
        .map(|item| format!(".{item}"))
        .unwrap_or_default();
    TEXT_EXTENSIONS.contains(&extension.as_str())
}

fn is_probably_text_buffer(bytes: &[u8]) -> bool {
    if bytes.is_empty() {
        return true;
    }
    let sample = &bytes[..bytes.len().min(4096)];
    let mut suspicious = 0usize;
    for byte in sample {
        if *byte == 0 {
            return false;
        }
        let is_allowed_control = matches!(*byte, 9 | 10 | 13);
        let is_printable_ascii = (32..=126).contains(byte);
        let is_utf8 = *byte >= 128;
        if !is_allowed_control && !is_printable_ascii && !is_utf8 {
            suspicious += 1;
        }
    }
    (suspicious as f64 / sample.len() as f64) < 0.08
}

fn truncate_text(text: &str, max_chars: usize) -> (String, bool) {
    if text.chars().count() <= max_chars {
        return (text.to_string(), false);
    }
    (
        format!("{}\n...(已截断)", text.chars().take(max_chars).collect::<String>()),
        true,
    )
}
