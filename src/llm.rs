use futures::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

#[derive(Serialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    pub stream: bool,
    pub max_tokens: u32,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

#[derive(Deserialize)]
pub struct StreamDelta {
    pub choices: Vec<Choice>,
}

#[derive(Deserialize)]
pub struct Choice {
    pub delta: Delta,
}

#[derive(Deserialize)]
pub struct Delta {
    pub content: Option<String>,
}

pub async fn stream_chat(
    endpoint: &str,
    model: &str,
    api_key: &str,
    prompt: &str,
    tx: mpsc::Sender<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let request = ChatRequest {
        model: model.to_string(),
        messages: vec![ChatMessage {
            role: "user".to_string(),
            content: prompt.to_string(),
        }],
        stream: true,
        max_tokens: 4096,
    };

    let client = reqwest::Client::new();
    let url = format!("{}/v1/chat/completions", endpoint);

    let response = client
        .post(&url)
        .bearer_auth(api_key)
        .json(&request)
        .send()
        .await?;

    let mut stream = response.bytes_stream();

    while let Some(chunk) = stream.next().await {
        let chunk_bytes = chunk?;
        let chunk_str = String::from_utf8(chunk_bytes.to_vec())?;

        for line in chunk_str.lines() {
            if line.starts_with("data: ") {
                let json_str = &line[6..];
                if json_str == "[DONE]" {
                    break;
                }

                if let Ok(delta) = serde_json::from_str::<StreamDelta>(json_str) {
                    if let Some(content) = delta.choices.first().and_then(|c| c.delta.content.as_ref()) {
                        let _ = tx.send(content.clone()).await;
                    }
                }
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_chat_message_creation() {
        let msg = ChatMessage {
            role: "user".to_string(),
            content: "Hello".to_string(),
        };

        assert_eq!(msg.role, "user");
        assert_eq!(msg.content, "Hello");
    }

    #[test]
    fn test_chat_request_serialization() {
        let request = ChatRequest {
            model: "gpt-4".to_string(),
            messages: vec![ChatMessage {
                role: "user".to_string(),
                content: "What is AI?".to_string(),
            }],
            stream: true,
            max_tokens: 1024,
        };

        let json = serde_json::to_string(&request).expect("Should serialize");
        assert!(json.contains("gpt-4"));
        assert!(json.contains("What is AI?"));
        assert!(json.contains("true"));
        assert!(json.contains("1024"));
    }

    #[test]
    fn test_delta_deserialization() {
        let json = r#"{"content": "Hello"}"#;
        let delta: Delta = serde_json::from_str(json).expect("Should deserialize");
        assert_eq!(delta.content, Some("Hello".to_string()));
    }

    #[test]
    fn test_delta_empty_content() {
        let json = r#"{}"#;
        let delta: Delta = serde_json::from_str(json).expect("Should deserialize");
        assert_eq!(delta.content, None);
    }

    #[test]
    fn test_stream_delta_structure() {
        let json = r#"{"choices": [{"delta": {"content": "test"}}]}"#;
        let stream_delta: StreamDelta =
            serde_json::from_str(json).expect("Should deserialize");

        assert_eq!(stream_delta.choices.len(), 1);
        assert_eq!(
            stream_delta.choices[0].delta.content,
            Some("test".to_string())
        );
    }
}
