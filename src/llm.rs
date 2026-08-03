use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;
use tokio::sync::mpsc;

#[derive(Serialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    pub stream: bool,
    pub max_tokens: u32,
    /// Omitted entirely when empty: an endpoint that has never heard of tool
    /// calling should see exactly the request it saw before this feature landed.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<Value>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ChatMessage {
    pub role: String,
    /// `Option`, not `String`: an assistant message that only carries tool calls
    /// has no content, and several providers reject `""` where they expect null
    /// or an absent field.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    /// Which call a `role: "tool"` message is answering.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl ChatMessage {
    pub fn text(role: &str, content: impl Into<String>) -> Self {
        Self {
            role: role.to_string(),
            content: Some(content.into()),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub function: FunctionCall,
}

impl Default for ToolCall {
    fn default() -> Self {
        Self {
            id: String::new(),
            kind: "function".to_string(),
            function: FunctionCall::default(),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct FunctionCall {
    pub name: String,
    /// Raw JSON, as the model produced it. Kept as a string because that is what
    /// the wire format uses, and because it may be malformed -- which is the
    /// tool's problem to report, not this layer's.
    pub arguments: String,
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
    #[serde(default)]
    pub tool_calls: Vec<ToolCallDelta>,
}

/// One fragment of a tool call. See `accumulate_tool_call`.
#[derive(Deserialize, Default)]
pub struct ToolCallDelta {
    #[serde(default)]
    pub index: usize,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(rename = "type", default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub function: Option<FunctionDelta>,
}

#[derive(Deserialize, Default)]
pub struct FunctionDelta {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub arguments: Option<String>,
}

/// What the request task reports back to the event loop. Every request ends with
/// exactly one `Done` or `Error`, so the UI can never get stuck on "Streaming…".
///
/// `ToolCalls` always arrives *before* the terminating `Done`, which is what lets
/// the app switch out of `Streaming` and have the trailing `Done` fall through
/// its own state guard.
#[derive(Debug)]
pub enum StreamEvent {
    Token(String),
    ToolCalls(Vec<ToolCall>),
    /// Not from the endpoint: the local command runner reports back on the same
    /// channel, so the event loop has one place to drain and one stale-id guard
    /// covering both sources.
    ToolsFinished(Vec<crate::tools::ToolOutcome>),
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
    messages: Vec<ChatMessage>,
    tools: Vec<Value>,
    request_id: u64,
    tx: mpsc::Sender<(u64, StreamEvent)>,
) {
    let result = run(endpoint, model, api_key, messages, tools, request_id, &tx).await;
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
    messages: Vec<ChatMessage>,
    tools: Vec<Value>,
    request_id: u64,
    tx: &mpsc::Sender<(u64, StreamEvent)>,
) -> Result<(), String> {
    // A lone system prompt is not a conversation.
    if messages.iter().all(|m| m.role == "system") {
        return Err("Nothing to send.".to_string());
    }

    let sent_tools = !tools.is_empty();
    let request = ChatRequest {
        model: model.to_string(),
        messages,
        stream: true,
        max_tokens: 4096,
        tools,
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
        // The most likely cause of a 400 the moment file tools ship is an
        // endpoint that does not implement tool calling, so name the fix.
        let hint = if status == reqwest::StatusCode::BAD_REQUEST && sent_tools {
            "\n\nIf this endpoint does not support tool calling, disable file tools:\nset `enabled = false` under [tools] in ~/.tuisample-code/config.toml."
        } else {
            ""
        };
        return Err(format!("HTTP {status} from {url}{detail}{hint}"));
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
        if let Some(message) = parsed.choices.into_iter().next().and_then(|c| c.message) {
            if let Some(text) = message.content.filter(|t| !t.is_empty()) {
                let _ = tx.send((request_id, StreamEvent::Token(text))).await;
            }
            let calls = finalize_tool_calls(message.tool_calls);
            if !calls.is_empty() {
                let _ = tx.send((request_id, StreamEvent::ToolCalls(calls))).await;
            }
        }
        return Ok(());
    }

    let mut stream = response.bytes_stream();
    // Chunks split arbitrarily: mid-UTF-8-sequence and mid-SSE-line. Buffer bytes
    // and only consume whole lines, otherwise tokens get dropped or decoding fails.
    let mut buf: Vec<u8> = Vec::new();
    let mut pending: Vec<ToolCall> = Vec::new();

    'read: while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("Stream interrupted: {e}"))?;
        buf.extend_from_slice(&chunk);

        while let Some(nl) = buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = buf.drain(..=nl).collect();
            let line = String::from_utf8_lossy(&line[..line.len() - 1]);
            match parse_sse_line(line.trim_end_matches('\r')) {
                SseLine::Done => break 'read,
                SseLine::Delta(delta) => {
                    if !apply_delta(delta, &mut pending, request_id, tx).await {
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
        if let SseLine::Delta(delta) = parse_sse_line(line.trim()) {
            apply_delta(delta, &mut pending, request_id, tx).await;
        }
    }

    // Tool calls are only complete once the stream is, since `arguments` is
    // assembled from fragments and is not valid JSON before then.
    let calls = finalize_tool_calls(pending);
    if !calls.is_empty() {
        let _ = tx.send((request_id, StreamEvent::ToolCalls(calls))).await;
    }

    Ok(())
}

/// Returns false if the receiver is gone.
async fn apply_delta(
    delta: Delta,
    pending: &mut Vec<ToolCall>,
    request_id: u64,
    tx: &mpsc::Sender<(u64, StreamEvent)>,
) -> bool {
    if let Some(text) = delta.content.filter(|s| !s.is_empty()) {
        if tx.send((request_id, StreamEvent::Token(text))).await.is_err() {
            return false;
        }
    }
    for fragment in delta.tool_calls {
        accumulate_tool_call(pending, fragment);
    }
    true
}

/// Tool calls do not arrive whole. The id and function name come in one delta,
/// then `arguments` dribbles out a few characters at a time across many more --
/// all correlated only by `index`, and not parseable as JSON until the last
/// fragment lands. Reassembling them is this function's whole job.
fn accumulate_tool_call(pending: &mut Vec<ToolCall>, fragment: ToolCallDelta) {
    if pending.len() <= fragment.index {
        pending.resize_with(fragment.index + 1, ToolCall::default);
    }
    let slot = &mut pending[fragment.index];

    if let Some(id) = fragment.id.filter(|s| !s.is_empty()) {
        slot.id = id;
    }
    if let Some(kind) = fragment.kind.filter(|s| !s.is_empty()) {
        slot.kind = kind;
    }
    if let Some(function) = fragment.function {
        if let Some(name) = function.name {
            slot.function.name.push_str(&name);
        }
        if let Some(arguments) = function.arguments {
            slot.function.arguments.push_str(&arguments);
        }
    }
}

/// Drop half-formed calls and give every survivor an id.
///
/// An id is mandatory: the `tool` message answering a call has to quote it back,
/// and providers reject the conversation if it does not match. Some endpoints
/// still omit it, so synthesize one rather than send an empty string.
fn finalize_tool_calls(calls: Vec<ToolCall>) -> Vec<ToolCall> {
    calls
        .into_iter()
        .filter(|c| !c.function.name.is_empty())
        .enumerate()
        .map(|(i, mut c)| {
            if c.id.is_empty() {
                c.id = format!("call_{i}");
            }
            if c.function.arguments.trim().is_empty() {
                c.function.arguments = "{}".to_string();
            }
            c
        })
        .collect()
}

pub enum SseLine {
    Delta(Delta),
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
        Ok(parsed) => parsed
            .choices
            .into_iter()
            .next()
            .map(|c| SseLine::Delta(c.delta))
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
            SseLine::Delta(delta) => delta.content.filter(|s| !s.is_empty()),
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
        assert!(token(r#"data: {"choices":[{"delta":{"role":"assistant"}}]}"#).is_none());
    }

    /// The reassembly this whole feature rests on: name and id once, arguments
    /// smeared across five fragments, none of them valid JSON alone.
    #[test]
    fn tool_call_fragments_reassemble_into_one_call() {
        let mut pending = Vec::new();
        for line in [
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_abc","type":"function","function":{"name":"read_file","arguments":""}}]}}]}"#,
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"pa"}}]}}]}"#,
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"th\": \"sr"}}]}}]}"#,
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"c/main"}}]}}]}"#,
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":".rs\"}"}}]}}]}"#,
        ] {
            if let SseLine::Delta(delta) = parse_sse_line(line) {
                for fragment in delta.tool_calls {
                    accumulate_tool_call(&mut pending, fragment);
                }
            }
        }

        let calls = finalize_tool_calls(pending);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_abc");
        assert_eq!(calls[0].function.name, "read_file");
        assert_eq!(calls[0].function.arguments, r#"{"path": "src/main.rs"}"#);
        // Must be parseable, or the tool layer has nothing to work with.
        let args: serde_json::Value = serde_json::from_str(&calls[0].function.arguments).unwrap();
        assert_eq!(args["path"], "src/main.rs");
    }

    #[test]
    fn parallel_tool_calls_are_kept_apart_by_index() {
        let mut pending = Vec::new();
        for line in [
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"a","function":{"name":"read_file","arguments":"{\"path\":"}}]}}]}"#,
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":1,"id":"b","function":{"name":"list_dir","arguments":"{\"path\":"}}]}}]}"#,
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":1,"function":{"arguments":"\"src\"}"}}]}}]}"#,
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"a.rs\"}"}}]}}]}"#,
        ] {
            if let SseLine::Delta(delta) = parse_sse_line(line) {
                for fragment in delta.tool_calls {
                    accumulate_tool_call(&mut pending, fragment);
                }
            }
        }

        let calls = finalize_tool_calls(pending);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].function.name, "read_file");
        assert_eq!(calls[0].function.arguments, r#"{"path":"a.rs"}"#);
        assert_eq!(calls[1].function.name, "list_dir");
        assert_eq!(calls[1].function.arguments, r#"{"path":"src"}"#);
    }

    #[test]
    fn calls_missing_an_id_or_arguments_are_repaired_and_nameless_ones_dropped() {
        let calls = finalize_tool_calls(vec![
            ToolCall {
                id: String::new(),
                kind: "function".to_string(),
                function: FunctionCall {
                    name: "list_dir".to_string(),
                    arguments: String::new(),
                },
            },
            ToolCall::default(), // no name: never happened, drop it
        ]);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_0");
        assert_eq!(calls[0].function.arguments, "{}");
    }

    /// An assistant message carrying only tool calls must serialize without a
    /// `content` key at all -- several providers reject `"content": ""` there.
    #[test]
    fn a_tool_call_message_serializes_without_a_content_field() {
        let message = ChatMessage {
            role: "assistant".to_string(),
            content: None,
            tool_calls: vec![ToolCall {
                id: "call_1".to_string(),
                kind: "function".to_string(),
                function: FunctionCall {
                    name: "read_file".to_string(),
                    arguments: r#"{"path":"a.rs"}"#.to_string(),
                },
            }],
            tool_call_id: None,
        };
        let json = serde_json::to_string(&message).unwrap();
        assert!(!json.contains("content"), "{json}");
        assert!(json.contains(r#""tool_calls""#), "{json}");
    }

    #[test]
    fn an_ordinary_message_carries_no_tool_fields() {
        let json = serde_json::to_string(&ChatMessage::text("user", "hi")).unwrap();
        assert_eq!(json, r#"{"role":"user","content":"hi"}"#);
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
            vec![ChatMessage::text("user", "hi")],
            Vec::new(),
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

    fn tool_calls_of(events: &[StreamEvent]) -> Vec<ToolCall> {
        events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::ToolCalls(calls) => Some(calls.clone()),
                _ => None,
            })
            .flatten()
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

    /// The same 7-byte splitting applied to tool calls, where a fragment boundary
    /// can land in the middle of an escaped JSON string.
    #[tokio::test]
    async fn streams_a_tool_call_across_split_chunks() {
        let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Let me look.\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"read_file\",\"arguments\":\"\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"path\\\"\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\": \\\"src/main.rs\\\"}\"}}]}}]}\n\n",
            "data: [DONE]\n\n",
        );
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{body}"
        );
        let addr = serve(response.into_bytes(), 7).await;

        let events = collect(&addr).await;
        assert_eq!(text_of(&events), "Let me look.");

        let calls = tool_calls_of(&events);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "read_file");
        assert_eq!(calls[0].function.arguments, r#"{"path": "src/main.rs"}"#);

        // Ordering matters: the app leaves `Streaming` on ToolCalls, and relies on
        // the trailing Done arriving afterwards to be ignored.
        let kinds: Vec<&str> = events
            .iter()
            .map(|e| match e {
                StreamEvent::Token(_) => "token",
                StreamEvent::ToolCalls(_) => "tools",
                StreamEvent::ToolsFinished(_) => "finished",
                StreamEvent::Done => "done",
                StreamEvent::Error(_) => "error",
            })
            .collect();
        assert_eq!(kinds, vec!["token", "tools", "done"]);
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

    /// ...including when that non-streaming answer is a tool call.
    #[tokio::test]
    async fn a_non_streaming_response_can_also_carry_tool_calls() {
        let body = r#"{"choices":[{"message":{"role":"assistant","content":null,"tool_calls":[{"id":"call_9","type":"function","function":{"name":"list_dir","arguments":"{\"path\":\"src\"}"}}]}}]}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let addr = serve(response.into_bytes(), 4096).await;

        let calls = tool_calls_of(&collect(&addr).await);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_9");
        assert_eq!(calls[0].function.name, "list_dir");
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

    /// A 400 on a request that carried tool schemas is most likely an endpoint
    /// that cannot do tool calling, so the error has to say how to turn them off.
    #[tokio::test]
    async fn a_400_while_sending_tools_explains_how_to_disable_them() {
        let body = r#"{"error":"unknown field: tools"}"#;
        let response = format!(
            "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let addr = serve(response.into_bytes(), 4096).await;

        let (tx, mut rx) = mpsc::channel(64);
        stream_chat(
            &addr,
            "m",
            "k",
            vec![ChatMessage::text("user", "hi")],
            vec![serde_json::json!({"type": "function"})],
            1,
            tx,
        )
        .await;

        let mut last = None;
        while let Ok((_, e)) = rx.try_recv() {
            last = Some(e);
        }
        match last {
            Some(StreamEvent::Error(e)) => assert!(e.contains("enabled = false"), "{e}"),
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
