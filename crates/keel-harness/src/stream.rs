//! Parses Claude Code's `stream-json` output. The audit log comes from here, not from hooks,
//! which let everything through if they crash.

use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub enum Parsed {
    Init {
        session_id: Option<String>,
        model: Option<String>,
        tools: Vec<String>,
        api_key_source: Option<String>,
        permission_mode: Option<String>,
        cli_version: Option<String>,
    },
    AssistantText {
        text: String,
    },
    ToolCall {
        id: String,
        name: String,
        input: Value,
        parent: Option<String>,
    },
    ToolResult {
        id: String,
        is_error: bool,
        text: String,
        /// Bash reports failures only as `Exit code N`. Success has no code, so it's `None`.
        exit_code: Option<i32>,
    },
    ApiRetry {
        attempt: Option<i64>,
        error: Option<String>,
    },
    PermissionDenied {
        tool: Option<String>,
        detail: Value,
    },
    RateLimit {
        status: Option<String>,
        resets_at: Option<i64>,
        kind: Option<String>,
        /// The plan's limit is used up and the request went through on extra usage. The status
        /// stays `rejected`, about the plan, while the step goes on working.
        using_overage: bool,
    },
    Result(ResultEvent),
    Other {
        kind: String,
    },
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ResultEvent {
    pub subtype: String,
    /// The field to trust. A rejected API key arrives with `subtype: success`.
    pub is_error: bool,
    pub text: Option<String>,
    pub errors: Vec<String>,
    pub session_id: Option<String>,
    pub turns: Option<i64>,
    pub duration_ms: Option<i64>,
    pub cost_usd: Option<f64>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub models: Vec<String>,
    pub denials: Vec<String>,
    pub structured_output: Option<Value>,
}

fn text_of(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_owned)
}

fn block_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn exit_code(is_error: bool, text: &str) -> Option<i32> {
    if !is_error {
        return None;
    }
    let rest = text.strip_prefix("Exit code ")?;
    rest.split_whitespace().next()?.parse().ok()
}

/// One input line can hold several things, such as text next to a tool call.
pub fn parse_line(line: &str) -> Vec<Parsed> {
    let Ok(value) = serde_json::from_str::<Value>(line) else {
        return Vec::new();
    };
    let kind = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let subtype = value
        .get("subtype")
        .and_then(Value::as_str)
        .unwrap_or_default();
    match (kind, subtype) {
        ("system", "init") => vec![Parsed::Init {
            session_id: text_of(&value, "session_id"),
            model: text_of(&value, "model"),
            tools: value
                .get("tools")
                .and_then(Value::as_array)
                .map(|tools| {
                    tools
                        .iter()
                        .filter_map(|t| t.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default(),
            api_key_source: text_of(&value, "apiKeySource"),
            permission_mode: text_of(&value, "permissionMode"),
            cli_version: text_of(&value, "claude_code_version"),
        }],
        ("system", "api_retry") => vec![Parsed::ApiRetry {
            attempt: value.get("attempt").and_then(Value::as_i64),
            error: text_of(&value, "error"),
        }],
        ("assistant", _) => {
            let parent = text_of(&value, "parent_tool_use_id");
            content_blocks(&value)
                .iter()
                .filter_map(|block| match block.get("type").and_then(Value::as_str) {
                    Some("text") => block
                        .get("text")
                        .and_then(Value::as_str)
                        .filter(|t| !t.trim().is_empty())
                        .map(|t| Parsed::AssistantText { text: t.to_owned() }),
                    Some("tool_use") => Some(Parsed::ToolCall {
                        id: text_of(block, "id").unwrap_or_default(),
                        name: text_of(block, "name").unwrap_or_default(),
                        input: block.get("input").cloned().unwrap_or(Value::Null),
                        parent: parent.clone(),
                    }),
                    _ => None,
                })
                .collect()
        }
        ("user", _) => content_blocks(&value)
            .iter()
            .filter(|block| block.get("type").and_then(Value::as_str) == Some("tool_result"))
            .map(|block| {
                let is_error = block
                    .get("is_error")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let text = block_text(block.get("content"));
                Parsed::ToolResult {
                    id: text_of(block, "tool_use_id").unwrap_or_default(),
                    is_error,
                    exit_code: exit_code(is_error, &text),
                    text,
                }
            })
            .collect(),
        ("permission_denied", _) | ("system", "permission_denied") => {
            vec![Parsed::PermissionDenied {
                tool: text_of(&value, "tool_name"),
                detail: value,
            }]
        }
        ("rate_limit_event", _) => {
            let info = value.get("rate_limit_info").cloned().unwrap_or(Value::Null);
            vec![Parsed::RateLimit {
                status: text_of(&info, "status"),
                resets_at: info.get("resetsAt").and_then(Value::as_i64),
                kind: text_of(&info, "rateLimitType"),
                using_overage: info.get("isUsingOverage").and_then(Value::as_bool) == Some(true),
            }]
        }
        ("result", _) => vec![Parsed::Result(parse_result(&value))],
        _ => vec![Parsed::Other {
            kind: if subtype.is_empty() {
                kind.to_owned()
            } else {
                format!("{kind}/{subtype}")
            },
        }],
    }
}

fn content_blocks(value: &Value) -> Vec<Value> {
    value
        .pointer("/message/content")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn parse_result(value: &Value) -> ResultEvent {
    let usage = value.get("usage");
    let tokens = |key: &str| usage.and_then(|u| u.get(key)).and_then(Value::as_i64);
    ResultEvent {
        subtype: text_of(value, "subtype").unwrap_or_default(),
        is_error: value
            .get("is_error")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        text: text_of(value, "result"),
        errors: value
            .get("errors")
            .and_then(Value::as_array)
            .map(|errors| {
                errors
                    .iter()
                    .map(|e| {
                        e.as_str()
                            .map(str::to_owned)
                            .unwrap_or_else(|| e.to_string())
                    })
                    .collect()
            })
            .unwrap_or_default(),
        session_id: text_of(value, "session_id"),
        turns: value.get("num_turns").and_then(Value::as_i64),
        duration_ms: value.get("duration_ms").and_then(Value::as_i64),
        cost_usd: value.get("total_cost_usd").and_then(Value::as_f64),
        input_tokens: Some(
            tokens("input_tokens").unwrap_or(0)
                + tokens("cache_read_input_tokens").unwrap_or(0)
                + tokens("cache_creation_input_tokens").unwrap_or(0),
        ),
        output_tokens: tokens("output_tokens"),
        models: value
            .get("modelUsage")
            .and_then(Value::as_object)
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default(),
        denials: value
            .get("permission_denials")
            .and_then(Value::as_array)
            .map(|d| d.iter().filter_map(|x| text_of(x, "tool_name")).collect())
            .unwrap_or_default(),
        structured_output: value
            .get("structured_output")
            .filter(|v| !v.is_null())
            .cloned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Vec<Parsed> {
        let path = format!(
            "{}/../../spikes/m0/fixtures/{name}.jsonl",
            env!("CARGO_MANIFEST_DIR")
        );
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .flat_map(parse_line)
            .collect()
    }

    fn result(events: &[Parsed]) -> Option<&ResultEvent> {
        events.iter().find_map(|e| match e {
            Parsed::Result(r) => Some(r),
            _ => None,
        })
    }

    #[test]
    fn success_reports_model_cost_and_subscription_auth() {
        let events = fixture("success");
        let Some(Parsed::Init {
            api_key_source,
            tools,
            model,
            ..
        }) = events.first()
        else {
            panic!("init comes first");
        };
        assert_eq!(api_key_source.as_deref(), Some("none"));
        assert_eq!(tools, &["Bash", "Read"]);
        assert!(model.as_deref().unwrap_or_default().contains("haiku"));
        let r = result(&events).unwrap();
        assert!(!r.is_error);
        assert_eq!(r.subtype, "success");
        assert!(r.cost_usd.unwrap() > 0.0);
        assert!(r.input_tokens.unwrap() > 0);
        assert!(r.models.iter().any(|m| m.contains("haiku")));
    }

    #[test]
    fn a_failed_command_exposes_its_exit_code() {
        let events = fixture("bash-exit-3");
        let code = events.iter().find_map(|e| match e {
            Parsed::ToolResult {
                is_error: true,
                exit_code,
                ..
            } => *exit_code,
            _ => None,
        });
        assert_eq!(code, Some(3));
    }

    #[test]
    fn tool_calls_keep_their_input() {
        let events = fixture("bash-exit-3");
        let call = events.iter().find_map(|e| match e {
            Parsed::ToolCall { name, input, .. } if name == "Bash" => Some(input.clone()),
            _ => None,
        });
        assert!(
            call.unwrap()["command"]
                .as_str()
                .unwrap()
                .contains("exit 3")
        );
    }

    #[test]
    fn error_results_have_their_subtype() {
        assert_eq!(
            result(&fixture("max-turns")).unwrap().subtype,
            "error_max_turns"
        );
        assert_eq!(
            result(&fixture("max-budget")).unwrap().subtype,
            "error_max_budget_usd"
        );
        assert!(result(&fixture("max-budget")).unwrap().is_error);
    }

    #[test]
    fn sigint_leaves_a_result_and_sigterm_does_not() {
        let interrupted = fixture("killed-SIGINT");
        let r = result(&interrupted).unwrap();
        assert_eq!(r.subtype, "error_during_execution");
        assert!(r.is_error);
        assert!(result(&fixture("killed-SIGTERM")).is_none());
    }

    #[test]
    fn structured_output_is_captured() {
        let r = result(&fixture("structured")).map(|r| r.structured_output.clone());
        assert_eq!(r.unwrap().unwrap()["verdict"], "approved");
    }

    #[test]
    fn rate_limit_events_are_understood() {
        let found = fixture("max-turns").into_iter().find_map(|e| match e {
            Parsed::RateLimit {
                status, resets_at, ..
            } => Some((status, resets_at)),
            _ => None,
        });
        let (status, resets) = found.unwrap();
        assert_eq!(status.as_deref(), Some("allowed"));
        assert!(resets.is_some());
    }

    #[test]
    fn a_rejected_key_is_an_error_even_with_subtype_success() {
        let line =
            r#"{"type":"result","subtype":"success","is_error":true,"result":"Invalid API key"}"#;
        let events = parse_line(line);
        let r = result(&events).unwrap();
        assert!(r.is_error);
        assert_eq!(r.subtype, "success");
    }

    #[test]
    fn garbage_lines_are_ignored() {
        assert!(parse_line("not json").is_empty());
        assert_eq!(
            parse_line(r#"{"type":"mystery"}"#),
            vec![Parsed::Other {
                kind: "mystery".into()
            }]
        );
    }
}
