use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedCommand {
    pub raw_name: String,
    pub name: String,
    pub raw_args: String,
    pub argument: String,
    pub flags: BTreeMap<String, String>,
    pub positionals: Vec<String>,
    pub prefix: String,
}

// 先兼容原版最常用入口，未识别命令直接让上层继续走普通消息分流。
pub fn parse_command(text: &str) -> Option<ParsedCommand> {
    let trimmed = text.trim();
    if trimmed.eq_ignore_ascii_case("#help") || trimmed == "#帮助" {
        return Some(ParsedCommand {
            raw_name: "help".to_string(),
            name: "help".to_string(),
            raw_args: String::new(),
            argument: String::new(),
            flags: BTreeMap::new(),
            positionals: Vec::new(),
            prefix: "#".to_string(),
        });
    }
    if trimmed.eq_ignore_ascii_case("#status") {
        return Some(ParsedCommand {
            raw_name: "status".to_string(),
            name: "status".to_string(),
            raw_args: String::new(),
            argument: String::new(),
            flags: BTreeMap::new(),
            positionals: Vec::new(),
            prefix: "#".to_string(),
        });
    }

    if !trimmed.starts_with('/') {
        return None;
    }

    let without_prefix = trimmed[1..].trim_start();
    if without_prefix.is_empty() {
        return None;
    }

    let mut pieces = without_prefix.splitn(2, char::is_whitespace);
    let raw_name = pieces.next()?.trim().to_string();
    if raw_name.is_empty() {
        return None;
    }
    let raw_args = pieces.next().map(str::trim).unwrap_or_default().to_string();
    let lowered = raw_name.to_ascii_lowercase();
    let name = match lowered.as_str() {
        "help" => "help",
        "status" => "status",
        "chat" => "chat",
        "agent" => "agent",
        "tr" => "translate",
        "e" => "edit",
        _ => return None,
    }
    .to_string();

    let tokenized = tokenize_command_line(&raw_args);
    let (flags, positionals) = parse_option_tokens(&tokenized);
    Some(ParsedCommand {
        raw_name,
        name,
        raw_args: raw_args.clone(),
        argument: raw_args,
        flags,
        positionals,
        prefix: "/".to_string(),
    })
}

pub fn tokenize_command_line(input: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_quote: Option<char> = None;
    let mut escaped = false;

    for ch in input.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        if let Some(quote) = in_quote {
            if ch == quote {
                in_quote = None;
            } else {
                current.push(ch);
            }
            continue;
        }
        if ch == '"' || ch == '\'' {
            in_quote = Some(ch);
            continue;
        }
        if ch.is_whitespace() {
            if !current.is_empty() {
                tokens.push(std::mem::take(&mut current));
            }
            continue;
        }
        current.push(ch);
    }

    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

pub fn parse_option_tokens(tokens: &[String]) -> (BTreeMap<String, String>, Vec<String>) {
    let mut flags = BTreeMap::new();
    let mut positionals = Vec::new();
    let mut index = 0usize;

    while index < tokens.len() {
        let token = &tokens[index];
        if !token.starts_with("--") {
            positionals.push(token.clone());
            index += 1;
            continue;
        }

        let option = &token[2..];
        if let Some((key, value)) = option.split_once('=') {
            flags.insert(key.to_string(), value.to_string());
            index += 1;
            continue;
        }

        if index + 1 < tokens.len() && !tokens[index + 1].starts_with("--") {
            flags.insert(option.to_string(), tokens[index + 1].clone());
            index += 2;
        } else {
            flags.insert(option.to_string(), "true".to_string());
            index += 1;
        }
    }

    (flags, positionals)
}

#[cfg(test)]
mod tests {
    use super::{parse_command, tokenize_command_line};

    #[test]
    fn parses_slash_command_and_flags() {
        let command = parse_command("/chat --group 123 \"hello world\" tail").expect("command");
        assert_eq!(command.name, "chat");
        assert_eq!(command.flags.get("group").map(String::as_str), Some("123"));
        assert_eq!(
            command.positionals,
            vec!["hello world".to_string(), "tail".to_string()]
        );
    }

    #[test]
    fn parses_hash_help_alias() {
        let command = parse_command("#帮助").expect("command");
        assert_eq!(command.name, "help");
        assert_eq!(command.prefix, "#");
    }

    #[test]
    fn parses_status_aliases() {
        let hash = parse_command("#status").expect("hash status");
        assert_eq!(hash.name, "status");
        assert_eq!(hash.prefix, "#");

        let slash = parse_command("/status").expect("slash status");
        assert_eq!(slash.name, "status");
        assert_eq!(slash.prefix, "/");
    }

    #[test]
    fn tokenizes_quoted_input() {
        let tokens = tokenize_command_line(r#"--name "Cain Bot" 'hello world'"#);
        assert_eq!(
            tokens,
            vec![
                "--name".to_string(),
                "Cain Bot".to_string(),
                "hello world".to_string()
            ]
        );
    }

    #[test]
    fn parses_agent_command() {
        let command = parse_command("/agent 帮我把这个任务做完").expect("command");
        assert_eq!(command.name, "agent");
        assert_eq!(command.argument, "帮我把这个任务做完");
    }
}
