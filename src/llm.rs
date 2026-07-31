use futures::StreamExt;
use serde::{Deserialize, Serialize};
use std::time::Duration;
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
    #[serde(default)]
    pub delta: Delta,
    /// Present when the endpoint ignores `stream: true` and answers with a plain
    /// (non-SSE) completion body.
    #[serde(default)]
    pub message: Option<ChatMessage>,
}

#[derive(Deserialize, Default)]
pub struct Delta {
    #[serde(default)]
    pub content: Option<String>,
}

/// What the request task reports back to the event loop. Every request ends with
/// exactly one `Done` or `Error`, so the UI can never get stuck on "Streaming…".
#[derive(Debug)]
pub enum StreamEvent {
    Token(String),
    Done,
    Error(String),
}

/// Build the chat-completions URL, tolerating the shapes people actually paste in:
/// `https://host`, `https://host/`, `https://host/v1`, or the full endpoint path.
pub fn chat_completions_url(endpoint: &str) -> String {
    let base = endpoint.trim().trim_end_matches('/');
    if base.ends_with("/chat/completions") {
        base.to_string()
    } else if base.ends_with("/v1") || base.ends_with("/openai") {
        format!("{base}/chat/completions")
    } else {
        format!("{base}/v1/chat/completions")
    }
}

pub async fn stream_chat(
    endpoint: &str,
    model: &str,
    api_key: &str,
    history: Vec<(String, String)>,
    request_id: u64,
    tx: mpsc::Sender<(u64, StreamEvent)>,
) {
    let result = run(endpoint, model, api_key, history, request_id, &tx).await;
    let event = match result {
        Ok(()) => StreamEvent::Done,
        Err(e) => StreamEvent::Error(e),
    };
    let _ = tx.send((request_id, event)).await;
}

async fn run(
    endpoint: &str,
    model: &str,
    api_key: &str,
    history: Vec<(String, String)>,
    request_id: u64,
    tx: &mpsc::Sender<(u64, StreamEvent)>,
) -> Result<(), String> {
    let messages: Vec<ChatMessage> = history
        .into_iter()
        .map(|(role, content)| ChatMessage { role, content })
        .collect();

    if messages.is_empty() {
        return Err("Nothing to send.".to_string());
    }

    let request = ChatRequest {
        model: model.to_string(),
        messages,
        stream: true,
        max_tokens: 4096,
    };

    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| format!("Could not create HTTP client: {e}"))?;

    let url = chat_completions_url(endpoint);

    let mut req = client.post(&url).json(&request);
    if !api_key.is_empty() {
        req = req.bearer_auth(api_key);
    }

    let response = req.send().await.map_err(|e| {
        if e.is_connect() || e.is_timeout() {
            format!("Could not reach {url}: {e}\nCheck TUISAMPLE_ENDPOINT / config.toml.")
        } else {
            format!("Request to {url} failed: {e}")
        }
    })?;

    // Surface HTTP failures (401 bad key, 404 wrong path, 400 bad model) as text
    // in the transcript instead of silently ending the stream.
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        let detail = body.trim();
        let detail = if detail.is_empty() {
            String::new()
        } else {
            format!("\n{}", truncate(detail, 800))
        };
        return Err(format!("HTTP {status} from {url}{detail}"));
    }

    let streaming = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.contains("event-stream"))
        .unwrap_or(true);

    if !streaming {
        // Endpoint ignored `stream: true` and returned one JSON object.
        let body = response
            .text()
            .await
            .map_err(|e| format!("Could not read response body: {e}"))?;
        let parsed: StreamDelta = serde_json::from_str(&body)
            .map_err(|e| format!("Unexpected response from {url}: {e}\n{}", truncate(&body, 800)))?;
        if let Some(text) = parsed
            .choices
            .first()
            .and_then(|c| c.message.as_ref().map(|m| m.content.clone()))
        {
            let _ = tx.send((request_id, StreamEvent::Token(text))).await;
        }
        return Ok(());
    }

    let mut stream = response.bytes_stream();
    // Chunks split arbitrarily: mid-UTF-8-sequence and mid-SSE-line. Buffer bytes
    // and only consume whole lines, otherwise tokens get dropped or decoding fails.
    let mut buf: Vec<u8> = Vec::new();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("Stream interrupted: {e}"))?;
        buf.extend_from_slice(&chunk);

        while let Some(nl) = buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = buf.drain(..=nl).collect();
            let line = String::from_utf8_lossy(&line[..line.len() - 1]);
            match parse_sse_line(line.trim_end_matches('\r')) {
                SseLine::Done => return Ok(()),
                SseLine::Token(text) => {
                    if tx
                        .send((request_id, StreamEvent::Token(text)))
                        .await
                        .is_err()
                    {
                        return Ok(()); // receiver gone; app is shutting down
                    }
                }
                SseLine::Ignore => {}
            }
        }
    }

    // Trailing line without a final newline.
    if !buf.is_empty() {
        let line = String::from_utf8_lossy(&buf).to_string();
        if let SseLine::Token(text) = parse_sse_line(line.trim()) {
            let _ = tx.send((request_id, StreamEvent::Token(text))).await;
        }
    }

    Ok(())
}

pub enum SseLine {
    Token(String),
    Done,
    Ignore,
}

/// Parse one SSE line. Accepts `data: {...}` and `data:{...}` (both are legal).
pub fn parse_sse_line(line: &str) -> SseLine {
    let line = line.trim_end();
    let Some(payload) = line.strip_prefix("data:") else {
        return SseLine::Ignore; // comments (`:`), `event:` lines, blank separators
    };
    let payload = payload.trim_start();

    if payload == "[DONE]" {
        return SseLine::Done;
    }

    match serde_json::from_str::<StreamDelta>(payload) {
        Ok(delta) => delta
            .choices
            .first()
            .and_then(|c| c.delta.content.clone())
            .filter(|s| !s.is_empty())
            .map(SseLine::Token)
            .unwrap_or(SseLine::Ignore),
        Err(_) => SseLine::Ignore,
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max).collect();
    format!("{cut}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn token(line: &str) -> Option<String> {
        match parse_sse_line(line) {
            SseLine::Token(t) => Some(t),
            _ => None,
        }
    }

    #[test]
    fn url_building_tolerates_the_shapes_people_paste() {
        assert_eq!(
            chat_completions_url("https://llm.internal:8443"),
            "https://llm.internal:8443/v1/chat/completions"
        );
        assert_eq!(
            chat_completions_url("https://llm.internal:8443/"),
            "https://llm.internal:8443/v1/chat/completions"
        );
        assert_eq!(
            chat_completions_url("https://llm.internal/v1"),
            "https://llm.internal/v1/chat/completions"
        );
        assert_eq!(
            chat_completions_url("https://llm.internal/v1/chat/completions"),
            "https://llm.internal/v1/chat/completions"
        );
    }

    #[test]
    fn sse_lines_parse_with_and_without_a_space_after_data() {
        assert_eq!(
            token(r#"data: {"choices":[{"delta":{"content":"Hi"}}]}"#).as_deref(),
            Some("Hi")
        );
        assert_eq!(
            token(r#"data:{"choices":[{"delta":{"content":"Hi"}}]}"#).as_deref(),
            Some("Hi")
        );
        assert!(matches!(parse_sse_line("data: [DONE]"), SseLine::Done));
        assert!(matches!(parse_sse_line(": keep-alive"), SseLine::Ignore));
        assert!(matches!(parse_sse_line(""), SseLine::Ignore));
        // Role-only opening delta carries no content.
        assert!(matches!(
            parse_sse_line(r#"data: {"choices":[{"delta":{"role":"assistant"}}]}"#),
            SseLine::Ignore
        ));
    }

    /// Serve one canned HTTP response on an ephemeral port and return its address.
    async fn serve(response: Vec<u8>, chunked_bytes: usize) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let _ = socket.read(&mut buf).await; // consume the request
            for piece in response.chunks(chunked_bytes.max(1)) {
                if socket.write_all(piece).await.is_err() {
                    return;
                }
                let _ = socket.flush().await;
            }
            let _ = socket.shutdown().await;
        });
        format!("http://{addr}")
    }

    async fn collect(endpoint: &str) -> Vec<StreamEvent> {
        let (tx, mut rx) = mpsc::channel(64);
        stream_chat(
            endpoint,
            "test-model",
            "sk-test",
            vec![("user".to_string(), "hi".to_string())],
            1,
            tx,
        )
        .await;
        let mut events = Vec::new();
        while let Ok((_, e)) = rx.try_recv() {
            events.push(e);
        }
        events
    }

    fn text_of(events: &[StreamEvent]) -> String {
        events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::Token(t) => Some(t.as_str()),
                _ => None,
            })
            .collect()
    }

    /// End-to-end against a live socket, delivered 7 bytes at a time so SSE lines
    /// and multi-byte characters are split across TCP chunks.
    #[tokio::test]
    async fn streams_tokens_across_split_chunks() {
        let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"}}]}\n\n",
            ": keep-alive\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\" wörld→\"}}]}\n\n",
            "data: [DONE]\n\n",
        );
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{body}"
        );
        let addr = serve(response.into_bytes(), 7).await;

        let events = collect(&addr).await;
        assert_eq!(text_of(&events), "Hello wörld→");
        assert!(matches!(events.last(), Some(StreamEvent::Done)));
    }

    /// Endpoints that ignore `stream: true` must still produce an answer.
    #[tokio::test]
    async fn falls_back_to_a_non_streaming_json_response() {
        let body = r#"{"choices":[{"message":{"role":"assistant","content":"Plain reply"}}]}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let addr = serve(response.into_bytes(), 4096).await;

        let events = collect(&addr).await;
        assert_eq!(text_of(&events), "Plain reply");
        assert!(matches!(events.last(), Some(StreamEvent::Done)));
    }

    #[tokio::test]
    async fn http_errors_are_reported_with_the_body() {
        let body = r#"{"error":{"message":"Invalid API key"}}"#;
        let response = format!(
            "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let addr = serve(response.into_bytes(), 4096).await;

        let events = collect(&addr).await;
        match events.last() {
            Some(StreamEvent::Error(e)) => {
                assert!(e.contains("401"), "{e}");
                assert!(e.contains("Invalid API key"), "{e}");
            }
            other => panic!("expected an Error event, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn unreachable_endpoints_report_an_error_rather_than_hanging() {
        // Port 1 on loopback refuses connections immediately.
        let events = collect("http://127.0.0.1:1").await;
        match events.last() {
            Some(StreamEvent::Error(e)) => assert!(e.contains("Could not reach"), "{e}"),
            other => panic!("expected an Error event, got {other:?}"),
        }
    }
}
