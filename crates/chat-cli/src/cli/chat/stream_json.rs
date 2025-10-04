use eyre::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEvent {
    System(SystemEvent),
    User(UserEvent),
    Assistant(AssistantEvent),
    ToolUse(ToolUseEvent),
    ToolResult(ToolResultEvent),
    Result(ResultEvent),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SystemEvent {
    pub subtype: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UserEvent {
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AssistantEvent {
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolUseEvent {
    pub tool_use_id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolResultEvent {
    pub tool_use_id: String,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ResultEvent {
    pub subtype: ResultSubtype,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_cost_usd: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ResultSubtype {
    Success,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

pub fn emit_event(event: &StreamEvent) -> Result<()> {
    let json = serde_json::to_string(event)?;
    println!("{}", json);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_system_event_serialization() {
        let event = StreamEvent::System(SystemEvent {
            subtype: "init".to_string(),
            session_id: Some("q-123".to_string()),
            model: Some("claude-3".to_string()),
        });

        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""type":"system"#));
        assert!(json.contains(r#""subtype":"init"#));
        assert!(json.contains(r#""session_id":"q-123"#));
    }

    #[test]
    fn test_assistant_event_serialization() {
        let event = StreamEvent::Assistant(AssistantEvent {
            content: "Hello".to_string(),
        });

        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""type":"assistant"#));
        assert!(json.contains(r#""content":"Hello"#));
    }

    #[test]
    fn test_tool_use_event_serialization() {
        let event = StreamEvent::ToolUse(ToolUseEvent {
            tool_use_id: "tool-123".to_string(),
            name: "fs_read".to_string(),
            input: Some(serde_json::json!({"path": "/test"})),
        });

        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""type":"tool_use"#));
        assert!(json.contains(r#""tool_use_id":"tool-123"#));
        assert!(json.contains(r#""name":"fs_read"#));
    }

    #[test]
    fn test_result_event_success_serialization() {
        let event = StreamEvent::Result(ResultEvent {
            subtype: ResultSubtype::Success,
            duration_ms: Some(532),
            total_cost_usd: None,
            usage: Some(Usage {
                input_tokens: 100,
                output_tokens: 50,
            }),
            error: None,
        });

        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""type":"result"#));
        assert!(json.contains(r#""subtype":"success"#));
        assert!(json.contains(r#""duration_ms":532"#));
        assert!(json.contains(r#""input_tokens":100"#));
    }

    #[test]
    fn test_result_event_error_serialization() {
        let event = StreamEvent::Result(ResultEvent {
            subtype: ResultSubtype::Error,
            duration_ms: None,
            total_cost_usd: None,
            usage: None,
            error: Some("Connection timeout".to_string()),
        });

        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""type":"result"#));
        assert!(json.contains(r#""subtype":"error"#));
        assert!(json.contains(r#""error":"Connection timeout"#));
    }

    #[test]
    fn test_print_all_event_examples() {
        println!("\n=== StreamJSON Output Examples ===\n");
        
        let system = StreamEvent::System(SystemEvent {
            subtype: "init".to_string(),
            session_id: Some("q-abc123".to_string()),
            model: Some("claude-3-sonnet".to_string()),
        });
        println!("System: {}", serde_json::to_string(&system).unwrap());
        
        let user = StreamEvent::User(UserEvent {
            content: "Hello".to_string(),
        });
        println!("User: {}", serde_json::to_string(&user).unwrap());
        
        let assistant = StreamEvent::Assistant(AssistantEvent {
            content: "Hi! How can I help?".to_string(),
        });
        println!("Assistant: {}", serde_json::to_string(&assistant).unwrap());
        
        let tool_use = StreamEvent::ToolUse(ToolUseEvent {
            tool_use_id: "toolu_123".to_string(),
            name: "fs_read".to_string(),
            input: Some(serde_json::json!({"path": "/test.txt"})),
        });
        println!("ToolUse: {}", serde_json::to_string(&tool_use).unwrap());
        
        let tool_result = StreamEvent::ToolResult(ToolResultEvent {
            tool_use_id: "toolu_123".to_string(),
            content: "File contents".to_string(),
            status: Some("success".to_string()),
        });
        println!("ToolResult: {}", serde_json::to_string(&tool_result).unwrap());
        
        let result = StreamEvent::Result(ResultEvent {
            subtype: ResultSubtype::Success,
            duration_ms: Some(532),
            total_cost_usd: None,
            usage: Some(Usage { input_tokens: 100, output_tokens: 50 }),
            error: None,
        });
        println!("Result: {}", serde_json::to_string(&result).unwrap());
    }
}
