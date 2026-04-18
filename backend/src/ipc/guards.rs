use anyhow::{ensure, Result};

use crate::llm::types::Message;

const MAX_CHAT_CHARS: usize = 12_000;
const HEAD_KEEP_CHARS: usize = 7_000;
const TAIL_KEEP_CHARS: usize = 2_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SanitizedChatInput {
    pub content: String,
    pub warnings: Vec<String>,
}

pub fn sanitize_chat_message(input: &str) -> Result<SanitizedChatInput> {
    let normalized_newlines = input.replace("\r\n", "\n").replace('\r', "\n");
    let trimmed = normalized_newlines.trim();
    ensure!(!trimmed.is_empty(), "Message is empty after trimming");

    let mut warnings: Vec<String> = Vec::new();
    let mut compacted_lines: Vec<String> = Vec::new();
    let mut previous_line = String::new();
    let mut repeated_count = 0usize;

    for raw_line in trimmed.lines() {
        let line = raw_line.split_whitespace().collect::<Vec<_>>().join(" ");
        if line.is_empty() {
            if compacted_lines
                .last()
                .map(|s| s.is_empty())
                .unwrap_or(false)
            {
                continue;
            }
            compacted_lines.push(String::new());
            continue;
        }

        if line.eq_ignore_ascii_case(&previous_line) {
            repeated_count += 1;
            if repeated_count >= 2 {
                if !warnings.iter().any(|w| w.contains("Repeated lines")) {
                    warnings
                        .push("Repeated lines were compacted to avoid looped context.".to_string());
                }
                continue;
            }
        } else {
            previous_line = line.clone();
            repeated_count = 0;
        }

        compacted_lines.push(line);
    }

    let mut content = compacted_lines.join("\n").trim().to_string();
    ensure!(!content.is_empty(), "Message is empty after sanitization");

    if content.chars().count() > MAX_CHAT_CHARS {
        warnings.push(format!(
            "Input exceeded {} characters and was truncated.",
            MAX_CHAT_CHARS
        ));
        let head: String = content.chars().take(HEAD_KEEP_CHARS).collect();
        let tail: String = content
            .chars()
            .rev()
            .take(TAIL_KEEP_CHARS)
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        content = format!(
            "{}\n\n[... truncated to avoid runaway context ...]\n\n{}",
            head, tail
        );
    }

    if looks_repetitive(&content) {
        warnings.push("Highly repetitive content detected; context was compacted.".to_string());
        if !content.contains("[... truncated to avoid runaway context ...]") {
            let words: Vec<&str> = content.split_whitespace().take(400).collect();
            content = format!(
                "{}\n\n[... repetitive content compacted ...]",
                words.join(" ")
            );
        }
    }

    Ok(SanitizedChatInput { content, warnings })
}

#[allow(dead_code)]
pub fn build_chat_messages(input: &str) -> Result<(Vec<Message>, Vec<String>)> {
    build_chat_messages_with_context(input, None)
}

pub fn build_chat_messages_with_context(
    input: &str,
    curated_context: Option<&str>,
) -> Result<(Vec<Message>, Vec<String>)> {
    let sanitized = sanitize_chat_message(input)?;
    let mut user_content = sanitized.content.clone();

    if !sanitized.warnings.is_empty() {
        user_content = format!(
            "Input notes: {}\n\n{}",
            sanitized.warnings.join(" "),
            user_content
        );
    }

    let mut messages = vec![Message::text("system",
        "You are Rook. Avoid repetitive or looped replies. Use only relevant context. If the request is missing context, say what is missing instead of guessing. Be concise and actionable.",
    )];

    if let Some(context) = curated_context {
        if !context.trim().is_empty() {
            messages.push(Message::text("system", context));
        }
    }

    messages.push(Message::text("user", user_content));

    Ok((messages, sanitized.warnings))
}

fn looks_repetitive(text: &str) -> bool {
    let words: Vec<String> = text
        .split_whitespace()
        .map(|w| w.to_ascii_lowercase())
        .collect();

    if words.len() < 20 {
        return false;
    }

    let unique = words.iter().collect::<std::collections::HashSet<_>>().len();
    unique * 3 < words.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_empty_messages() {
        let result = sanitize_chat_message("   \n \n  ");
        assert!(result.is_err());
    }

    #[test]
    fn compacts_repeated_lines() {
        let result = sanitize_chat_message("repeat\nrepeat\nrepeat\nuseful line").unwrap();
        assert!(result.content.contains("repeat"));
        assert!(result.content.contains("useful line"));
        assert!(result.warnings.iter().any(|w| w.contains("Repeated lines")));
    }

    #[test]
    fn truncates_very_long_messages() {
        let long = "abc ".repeat(5000);
        let result = sanitize_chat_message(&long).unwrap();
        assert!(result.content.contains("truncated"));
    }

    #[test]
    fn build_messages_adds_system_prompt() {
        let (messages, _) = build_chat_messages("hello backend").unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, "system");
        assert_eq!(messages[1].role, "user");
    }

    #[test]
    fn build_messages_includes_curated_context() {
        let (messages, _) = build_chat_messages_with_context(
            "hello backend",
            Some("1. Important node [concept] score=0.91"),
        )
        .unwrap();

        assert_eq!(messages.len(), 3);
        assert!(messages[1].content.contains("Important node"));
    }
}
