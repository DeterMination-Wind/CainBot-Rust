use anyhow::Result;
use serde_json::Value;

use crate::commands::{ParsedCommand, parse_command};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventContext {
    pub message_type: String,
    pub group_id: String,
    pub user_id: String,
    pub self_id: String,
}

// 统一把 NapCat 事件收敛成后续业务能复用的上下文，避免每个入口都手搓字段提取。
pub fn create_context_from_event(event: &Value) -> EventContext {
    let message_type = if event.get("message_type").and_then(Value::as_str) == Some("group") {
        "group".to_string()
    } else {
        "private".to_string()
    };
    EventContext {
        group_id: if message_type == "group" {
            event
                .get("group_id")
                .and_then(|value| value.as_i64().map(|item| item.to_string()).or_else(|| value.as_str().map(ToString::to_string)))
                .unwrap_or_default()
        } else {
            String::new()
        },
        user_id: event
            .get("user_id")
            .and_then(|value| value.as_i64().map(|item| item.to_string()).or_else(|| value.as_str().map(ToString::to_string)))
            .unwrap_or_default(),
        self_id: event
            .get("self_id")
            .and_then(|value| value.as_i64().map(|item| item.to_string()).or_else(|| value.as_str().map(ToString::to_string)))
            .unwrap_or_default(),
        message_type,
    }
}

pub fn get_sender_name(event: &Value) -> String {
    event
        .get("sender")
        .and_then(|sender| sender.get("card").or_else(|| sender.get("nickname")))
        .and_then(Value::as_str)
        .map(|item| item.trim().to_string())
        .filter(|item| !item.is_empty())
        .or_else(|| {
            event.get("user_id").and_then(|value| {
                value.as_i64()
                    .map(|item| item.to_string())
                    .or_else(|| value.as_str().map(ToString::to_string))
            })
        })
        .unwrap_or_else(|| "用户".to_string())
}

pub fn plain_text_from_event(event: &Value) -> String {
    plain_text_from_message(
        event.get("message").unwrap_or(&Value::Null),
        event.get("raw_message").and_then(Value::as_str),
    )
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
    if let Some(text) = raw_message {
        return strip_cq_codes(text);
    }
    String::new()
}

pub fn event_mentions_self(event: &Value, bot_display_name: &str) -> bool {
    let self_id = event
        .get("self_id")
        .and_then(|value| value.as_i64().map(|item| item.to_string()).or_else(|| value.as_str().map(ToString::to_string)))
        .unwrap_or_default();
    if !self_id.is_empty() {
        let segments = normalize_message_segments(event.get("message").unwrap_or(&Value::Null));
        if segments.iter().any(|segment| {
            segment.get("type").and_then(Value::as_str) == Some("at")
                && segment
                    .get("data")
                    .and_then(|data| data.get("qq"))
                    .and_then(|value| value.as_i64().map(|item| item.to_string()).or_else(|| value.as_str().map(ToString::to_string)))
                    .map(|qq| qq == self_id)
                    .unwrap_or(false)
        }) {
            return true;
        }
        let raw_message = event.get("raw_message").and_then(Value::as_str).unwrap_or_default();
        if raw_message.contains(&format!("[CQ:at,qq={self_id}")) {
            return true;
        }
    }
    text_looks_like_direct_bot_mention(&plain_text_from_event(event), bot_display_name)
}

pub fn event_mentions_other_user(event: &Value, bot_display_name: &str) -> bool {
    let self_id = event
        .get("self_id")
        .and_then(|value| value.as_i64().map(|item| item.to_string()).or_else(|| value.as_str().map(ToString::to_string)))
        .unwrap_or_default();
    for segment in normalize_message_segments(event.get("message").unwrap_or(&Value::Null)) {
        if segment.get("type").and_then(Value::as_str) != Some("at") {
            continue;
        }
        let qq = segment
            .get("data")
            .and_then(|data| data.get("qq"))
            .and_then(|value| value.as_i64().map(|item| item.to_string()).or_else(|| value.as_str().map(ToString::to_string)))
            .unwrap_or_default();
        if !qq.is_empty() && qq != self_id {
            return true;
        }
    }
    let plain = plain_text_from_event(event);
    let bot_name = bot_display_name.trim().to_ascii_lowercase();
    plain.split_whitespace().any(|item| {
        item.strip_prefix('@')
            .map(|name| {
                let normalized = name.trim().to_ascii_lowercase();
                !normalized.is_empty() && normalized != bot_name
            })
            .unwrap_or(false)
    })
}

pub fn is_question_intent_text(text: &str) -> bool {
    let normalized = text.trim();
    if normalized.is_empty() {
        return false;
    }
    let compact: String = normalized.split_whitespace().collect();
    if compact.is_empty() {
        return false;
    }
    if compact.contains('?') || compact.contains('？') {
        return true;
    }
    if ["谁", "什么", "怎么", "咋", "如何", "为什么", "为啥", "哪里", "哪儿", "哪个", "哪位", "是否", "是不是"]
        .iter()
        .any(|prefix| compact.starts_with(prefix))
    {
        return true;
    }
    [
        "请问", "求问", "求助", "求解", "请教", "帮忙", "help", "报错", "错误", "bug", "故障",
        "没反应", "不生效", "无效", "失效", "什么意思", "是什么", "是啥", "怎么弄", "怎么做",
        "怎么打", "怎么过", "怎么配", "怎么改", "在哪", "还是",
    ]
    .iter()
    .any(|needle| compact.to_ascii_lowercase().contains(&needle.to_ascii_lowercase()))
}

pub fn parse_command_from_event(event: &Value) -> Option<ParsedCommand> {
    parse_command(&plain_text_from_event(event))
}

pub fn build_help_text(bot_display_name: &str) -> String {
    [
        format!("{bot_display_name} 当前可用命令："),
        "/help 查看帮助".to_string(),
        "/chat <问题> 显式发起问答".to_string(),
        "/tr <文本> 翻译文本或结合引用消息翻译".to_string(),
        "/e 状态 查看当前群开关与 prompt 状态".to_string(),
        "/e 过滤 <要求> 修改过滤 prompt".to_string(),
        "/e 聊天 <要求> 修改聊天 prompt".to_string(),
        "/e 启用 | /e 禁用 开关群问答".to_string(),
        "/e 过滤心跳 启用 [N] | 关闭".to_string(),
        "/e 文件下载 启用 [群文件夹名] | 关闭".to_string(),
        "".to_string(),
        "远程运维命令：".to_string(),
        "napcat-start-assassin  启动刺客插件".to_string(),
        "napcat-stop-assassin   停止刺客插件".to_string(),
    ]
    .join("\n")
}

pub fn ensure_message_event(event: &Value) -> Result<bool> {
    Ok(event.get("post_type").and_then(Value::as_str) == Some("message"))
}

fn normalize_message_segments(message: &Value) -> Vec<&Value> {
    message.as_array().map(|items| items.iter().collect()).unwrap_or_default()
}

fn strip_cq_codes(text: &str) -> String {
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

fn text_looks_like_direct_bot_mention(text: &str, display_name: &str) -> bool {
    let normalized_text = text.trim();
    let normalized_display_name = display_name.trim();
    if normalized_text.is_empty() || normalized_display_name.is_empty() {
        return false;
    }
    for token in normalized_text.split_whitespace() {
        let stripped = token
            .trim_matches(|ch: char| matches!(ch, ':' | '：' | ',' | '，' | '.' | '。' | '!' | '！' | '?' | '？'))
            .trim_start_matches('@')
            .trim_start_matches('>');
        if stripped.eq_ignore_ascii_case(normalized_display_name) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{event_mentions_other_user, event_mentions_self, is_question_intent_text, plain_text_from_event};

    #[test]
    fn detects_self_mention_from_segment() {
        let event = json!({
            "self_id": "42",
            "message": [
                { "type": "at", "data": { "qq": "42" } },
                { "type": "text", "data": { "text": " 帮我看下" } }
            ]
        });
        assert!(event_mentions_self(&event, "Cain"));
    }

    #[test]
    fn detects_other_user_mention() {
        let event = json!({
            "self_id": "42",
            "message": [
                { "type": "at", "data": { "qq": "43" } },
                { "type": "text", "data": { "text": " hi" } }
            ]
        });
        assert!(event_mentions_other_user(&event, "Cain"));
    }

    #[test]
    fn strips_cq_codes_from_raw_message() {
        let event = json!({
            "raw_message": "[CQ:at,qq=42] Cain 这是什么？"
        });
        assert_eq!(plain_text_from_event(&event), "Cain 这是什么？");
    }

    #[test]
    fn detects_question_intent() {
        assert!(is_question_intent_text("这个报错怎么解决"));
        assert!(is_question_intent_text("why does it fail?"));
        assert!(!is_question_intent_text("今天上线"));
    }
}
