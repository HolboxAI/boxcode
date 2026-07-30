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
            if let Some(json_str) = line.strip_prefix("data: ") {
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
