use futures::StreamExt;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tokio::sync::mpsc;

/// Where to send a turn, and how big an answer to allow.
#[derive(Clone, Debug)]
pub struct Target {
    pub endpoint: String,
    pub model: String,
    pub api_key: String,
    pub max_tokens: u32,
}

#[derive(Serialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    pub stream: bool,
    pub max_tokens: u32,
    /// Omitted entirely when the agent has no tools, so a plain chat turn is
    /// byte-for-byte the request this crate sent before tool calling existed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolDef>>,
}

/// One message in the conversation. `content` is optional because an assistant
/// turn that only calls tools legitimately carries `content: null`.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct ChatMessage {
    pub role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    /// Set only on `role: "tool"` messages, tying a result back to its call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".to_string(),
            content: Some(content.into()),
            ..Default::default()
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".to_string(),
            content: Some(content.into()),
            ..Default::default()
        }
    }

    pub fn assistant(text: impl Into<String>, tool_calls: Vec<ToolCall>) -> Self {
        let text = text.into();
        Self {
            role: "assistant".to_string(),
            content: (!text.is_empty()).then_some(text),
            tool_calls,
            tool_call_id: None,
        }
    }

    pub fn tool_result(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: "tool".to_string(),
            content: Some(content.into()),
            tool_calls: Vec::new(),
            tool_call_id: Some(tool_call_id.into()),
        }
    }

    pub fn text(&self) -> &str {
        self.content.as_deref().unwrap_or_default()
    }
}

/// A tool as advertised to the model in the request's `tools` array.
#[derive(Serialize, Clone, Debug)]
pub struct ToolDef {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub function: FunctionDef,
}

#[derive(Serialize, Clone, Debug)]
pub struct FunctionDef {
    pub name: String,
    pub description: String,
    /// JSON Schema for the arguments object.
    pub parameters: serde_json::Value,
}

impl ToolDef {
    pub fn function(name: &str, description: &str, parameters: serde_json::Value) -> Self {
        Self {
            kind: "function",
            function: FunctionDef {
                name: name.to_string(),
                description: description.to_string(),
                parameters,
            },
        }
    }
}

/// A tool call the model wants executed.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type", default = "function_kind")]
    pub kind: String,
    pub function: FunctionCall,
}

fn function_kind() -> String {
    "function".to_string()
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct FunctionCall {
    pub name: String,
    /// A JSON *string*, not an object -- that is the wire format. It may be
    /// malformed if the model was cut off mid-call; callers must handle that.
    pub arguments: String,
}

impl ToolCall {
    /// Parse `arguments` into a JSON object. An absent/blank argument string is
    /// treated as `{}` -- models routinely omit it for zero-argument tools.
    pub fn parsed_arguments(&self) -> Result<serde_json::Value, String> {
        let raw = self.function.arguments.trim();
        if raw.is_empty() {
            return Ok(serde_json::json!({}));
        }
        serde_json::from_str(raw)
            .map_err(|e| format!("arguments were not valid JSON: {e}\nreceived: {raw}"))
    }
}

/// The result of one assistant turn: any prose it produced, plus any tool calls
/// it wants run before it can continue.
#[derive(Clone, Debug, Default)]
pub struct AssistantTurn {
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
}

impl AssistantTurn {
    pub fn into_message(self) -> ChatMessage {
        ChatMessage::assistant(self.text, self.tool_calls)
    }
}

#[derive(Deserialize)]
pub struct StreamDelta {
    #[serde(default)]
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
    #[serde(default)]
    pub finish_reason: Option<String>,
}

#[derive(Deserialize, Default)]
pub struct Delta {
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub tool_calls: Vec<ToolCallDelta>,
}

/// One fragment of a streamed tool call. `id` and `function.name` arrive on the
/// first fragment only; `function.arguments` dribbles in across many. `index` is
/// the only field that reliably says which call a fragment belongs to.
#[derive(Deserialize, Clone, Debug, Default)]
pub struct ToolCallDelta {
    #[serde(default)]
    pub index: usize,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub function: Option<FunctionCallDelta>,
}

#[derive(Deserialize, Clone, Debug, Default)]
pub struct FunctionCallDelta {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub arguments: Option<String>,
}

/// Reassembles streamed tool-call fragments into whole `ToolCall`s.
#[derive(Default)]
struct ToolCallAccumulator {
    calls: Vec<ToolCall>,
}

impl ToolCallAccumulator {
    fn apply(&mut self, delta: &ToolCallDelta) {
        // Grow to cover `index`: providers may stream call 1's fragments before
        // call 0 is complete, so this cannot assume calls arrive in order.
        while self.calls.len() <= delta.index {
            self.calls.push(ToolCall::default());
        }
        let call = &mut self.calls[delta.index];

        if let Some(id) = delta.id.as_deref().filter(|s| !s.is_empty()) {
            call.id.push_str(id);
        }
        if let Some(function) = &delta.function {
            if let Some(name) = function.name.as_deref().filter(|s| !s.is_empty()) {
                call.function.name.push_str(name);
            }
            if let Some(arguments) = &function.arguments {
                call.function.arguments.push_str(arguments);
            }
        }
    }

    /// Drop slots that never received a name (a provider quirk: an empty
    /// trailing delta), and synthesize ids for endpoints that omit them --
    /// results are matched back by id, so a blank one would be unroutable.
    fn finish(self) -> Vec<ToolCall> {
        self.calls
            .into_iter()
            .enumerate()
            .filter(|(_, c)| !c.function.name.is_empty())
            .map(|(i, mut c)| {
                if c.id.is_empty() {
                    c.id = format!("call_{i}");
                }
                if c.kind.is_empty() {
                    c.kind = function_kind();
                }
                c
            })
            .collect()
    }
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

pub fn build_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| format!("Could not create HTTP client: {e}"))
}

/// Run one assistant turn. Prose is pushed through `tx` as it streams (so the UI
/// stays live); the assembled turn -- including tool calls -- is returned to the
/// caller, which is what the agent loop needs to decide what happens next.
pub async fn stream_turn<E, F>(
    client: &reqwest::Client,
    target: &Target,
    messages: &[ChatMessage],
    tools: &[ToolDef],
    tx: &mpsc::Sender<E>,
    on_token: F,
) -> Result<AssistantTurn, String>
where
    F: Fn(String) -> E,
{
    if messages.is_empty() {
        return Err("Nothing to send.".to_string());
    }

    let request = ChatRequest {
        model: target.model.clone(),
        messages: messages.to_vec(),
        stream: true,
        max_tokens: target.max_tokens,
        tools: (!tools.is_empty()).then(|| tools.to_vec()),
    };

    let url = chat_completions_url(&target.endpoint);

    let mut req = client.post(&url).json(&request);
    if !target.api_key.is_empty() {
        req = req.bearer_auth(&target.api_key);
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
        return non_streaming_turn(response, &url, tx, &on_token).await;
    }

    let mut turn = AssistantTurn::default();
    let mut calls = ToolCallAccumulator::default();
    let mut finish_reason: Option<String> = None;

    let mut stream = response.bytes_stream();
    // Chunks split arbitrarily: mid-UTF-8-sequence and mid-SSE-line. Buffer bytes
    // and only consume whole lines, otherwise tokens get dropped or decoding fails.
    let mut buf: Vec<u8> = Vec::new();
    let mut done = false;

    'outer: while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("Stream interrupted: {e}"))?;
        buf.extend_from_slice(&chunk);

        while let Some(nl) = buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = buf.drain(..=nl).collect();
            let line = String::from_utf8_lossy(&line[..line.len() - 1]);
            match parse_sse_line(line.trim_end_matches('\r')) {
                SseLine::Done => {
                    done = true;
                    break 'outer;
                }
                SseLine::Delta(delta, reason) => {
                    if reason.is_some() {
                        finish_reason = reason;
                    }
                    if let Some(text) = delta.content.filter(|s| !s.is_empty()) {
                        turn.text.push_str(&text);
                        if tx.send(on_token(text)).await.is_err() {
                            // Receiver gone; the app is shutting down.
                            done = true;
                            break 'outer;
                        }
                    }
                    for d in &delta.tool_calls {
                        calls.apply(d);
                    }
                }
                SseLine::Ignore => {}
            }
        }
    }

    // Trailing line without a final newline.
    if !done && !buf.is_empty() {
        let line = String::from_utf8_lossy(&buf).to_string();
        if let SseLine::Delta(delta, reason) = parse_sse_line(line.trim()) {
            if reason.is_some() {
                finish_reason = reason;
            }
            if let Some(text) = delta.content.filter(|s| !s.is_empty()) {
                turn.text.push_str(&text);
                let _ = tx.send(on_token(text)).await;
            }
            for d in &delta.tool_calls {
                calls.apply(d);
            }
        }
    }

    turn.tool_calls = calls.finish();

    // A turn cut off at max_tokens leaves truncated prose and, worse, tool-call
    // arguments that are invalid JSON. Say so rather than letting it look like
    // the model simply stopped.
    if finish_reason.as_deref() == Some("length") {
        turn.text
            .push_str("\n\n[truncated: hit max_tokens; raise it in config.toml]");
    }

    Ok(turn)
}

/// Endpoint ignored `stream: true` and returned one JSON object.
async fn non_streaming_turn<E, F>(
    response: reqwest::Response,
    url: &str,
    tx: &mpsc::Sender<E>,
    on_token: &F,
) -> Result<AssistantTurn, String>
where
    F: Fn(String) -> E,
{
    let body = response
        .text()
        .await
        .map_err(|e| format!("Could not read response body: {e}"))?;
    let parsed: StreamDelta = serde_json::from_str(&body).map_err(|e| {
        format!("Unexpected response from {url}: {e}\n{}", truncate(&body, 800))
    })?;

    let mut turn = AssistantTurn::default();
    if let Some(message) = parsed.choices.first().and_then(|c| c.message.as_ref()) {
        turn.text = message.text().to_string();
        turn.tool_calls = message.tool_calls.clone();
        if !turn.text.is_empty() {
            let _ = tx.send(on_token(turn.text.clone())).await;
        }
    }
    Ok(turn)
}

pub enum SseLine {
    /// A content and/or tool-call fragment, plus this chunk's `finish_reason`.
    Delta(Delta, Option<String>),
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
        Ok(mut parsed) => match parsed.choices.first_mut() {
            Some(choice) => SseLine::Delta(
                std::mem::take(&mut choice.delta),
                choice.finish_reason.take(),
            ),
            None => SseLine::Ignore,
        },
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
            SseLine::Delta(d, _) => d.content.filter(|s| !s.is_empty()),
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
        assert_eq!(
            token(r#"data: {"choices":[{"delta":{"role":"assistant"}}]}"#),
            None
        );
    }

    /// A tool-only turn carries `content: null`, which must not be mistaken for
    /// a parse failure.
    #[test]
    fn a_tool_call_delta_parses_without_content() {
        let line = r#"data: {"choices":[{"delta":{"content":null,"tool_calls":[{"index":0,"id":"call_a","function":{"name":"read_file","arguments":""}}]}}]}"#;
        match parse_sse_line(line) {
            SseLine::Delta(d, _) => {
                assert!(d.content.is_none());
                assert_eq!(d.tool_calls.len(), 1);
                assert_eq!(d.tool_calls[0].id.as_deref(), Some("call_a"));
            }
            _ => panic!("expected a Delta"),
        }
    }

    #[test]
    fn accumulator_reassembles_arguments_split_across_fragments() {
        let mut acc = ToolCallAccumulator::default();
        for (index, id, name, args) in [
            (0usize, Some("call_a"), Some("read_file"), Some(r#"{"pa"#)),
            (0, None, None, Some(r#"th": "src/"#)),
            (0, None, None, Some(r#"app.rs"}"#)),
        ] {
            acc.apply(&ToolCallDelta {
                index,
                id: id.map(str::to_string),
                function: Some(FunctionCallDelta {
                    name: name.map(str::to_string),
                    arguments: args.map(str::to_string),
                }),
            });
        }

        let calls = acc.finish();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_a");
        assert_eq!(calls[0].function.name, "read_file");
        assert_eq!(
            calls[0].parsed_arguments().unwrap(),
            serde_json::json!({"path": "src/app.rs"})
        );
    }

    /// Two calls in one turn, interleaved -- `index`, not arrival order, decides
    /// which fragment belongs to which call.
    #[test]
    fn accumulator_keeps_interleaved_parallel_calls_apart() {
        let mut acc = ToolCallAccumulator::default();
        let frag = |index, id: Option<&str>, name: Option<&str>, args: &str| ToolCallDelta {
            index,
            id: id.map(str::to_string),
            function: Some(FunctionCallDelta {
                name: name.map(str::to_string),
                arguments: Some(args.to_string()),
            }),
        };
        acc.apply(&frag(0, Some("a"), Some("read_file"), r#"{"path":"#));
        acc.apply(&frag(1, Some("b"), Some("list_dir"), r#"{"path":"#));
        acc.apply(&frag(0, None, None, r#""one.rs"}"#));
        acc.apply(&frag(1, None, None, r#""src"}"#));

        let calls = acc.finish();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].function.name, "read_file");
        assert_eq!(
            calls[0].parsed_arguments().unwrap(),
            serde_json::json!({"path": "one.rs"})
        );
        assert_eq!(calls[1].function.name, "list_dir");
        assert_eq!(
            calls[1].parsed_arguments().unwrap(),
            serde_json::json!({"path": "src"})
        );
    }

    #[test]
    fn accumulator_synthesizes_ids_and_drops_nameless_slots() {
        let mut acc = ToolCallAccumulator::default();
        acc.apply(&ToolCallDelta {
            index: 0,
            id: None,
            function: Some(FunctionCallDelta {
                name: Some("list_dir".to_string()),
                arguments: Some("{}".to_string()),
            }),
        });
        // A trailing fragment that never names a tool is not a call.
        acc.apply(&ToolCallDelta {
            index: 1,
            id: Some("orphan".to_string()),
            function: None,
        });

        let calls = acc.finish();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_0");
        assert_eq!(calls[0].kind, "function");
    }

    #[test]
    fn blank_arguments_parse_as_an_empty_object() {
        let call = ToolCall {
            id: "x".to_string(),
            kind: "function".to_string(),
            function: FunctionCall {
                name: "list_dir".to_string(),
                arguments: "  ".to_string(),
            },
        };
        assert_eq!(call.parsed_arguments().unwrap(), serde_json::json!({}));
    }

    #[test]
    fn malformed_arguments_report_an_error_rather_than_panicking() {
        let call = ToolCall {
            id: "x".to_string(),
            kind: "function".to_string(),
            function: FunctionCall {
                name: "read_file".to_string(),
                arguments: r#"{"path": "unterminated"#.to_string(),
            },
        };
        assert!(call.parsed_arguments().is_err());
    }

    /// Tool-free turns must serialize exactly as they did before tool calling
    /// existed -- some endpoints reject unknown/null fields.
    #[test]
    fn a_plain_message_serializes_without_tool_fields() {
        let json = serde_json::to_string(&ChatMessage::user("hi")).unwrap();
        assert_eq!(json, r#"{"role":"user","content":"hi"}"#);
    }

    #[test]
    fn a_tool_only_assistant_turn_serializes_with_null_content_omitted() {
        let message = ChatMessage::assistant(
            "",
            vec![ToolCall {
                id: "call_a".to_string(),
                kind: "function".to_string(),
                function: FunctionCall {
                    name: "list_dir".to_string(),
                    arguments: "{}".to_string(),
                },
            }],
        );
        let json = serde_json::to_value(&message).unwrap();
        assert!(json.get("content").is_none());
        assert_eq!(json["tool_calls"][0]["type"], "function");
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

    fn sse(body: &str) -> Vec<u8> {
        format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{body}")
            .into_bytes()
    }

    pub(crate) fn target(endpoint: &str) -> Target {
        Target {
            endpoint: endpoint.to_string(),
            model: "test-model".to_string(),
            api_key: "sk-test".to_string(),
            max_tokens: 4096,
        }
    }

    async fn run(endpoint: &str) -> (Result<AssistantTurn, String>, Vec<String>) {
        let (tx, mut rx) = mpsc::channel(64);
        let client = build_client().unwrap();
        let result = stream_turn(
            &client,
            &target(endpoint),
            &[ChatMessage::user("hi")],
            &[],
            &tx,
            |t| t,
        )
        .await;
        let mut tokens = Vec::new();
        while let Ok(t) = rx.try_recv() {
            tokens.push(t);
        }
        (result, tokens)
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
        let addr = serve(sse(body), 7).await;

        let (turn, tokens) = run(&addr).await;
        let turn = turn.expect("the turn should succeed");
        assert_eq!(turn.text, "Hello wörld→");
        assert!(turn.tool_calls.is_empty());
        assert_eq!(tokens.concat(), "Hello wörld→");
    }

    /// The same 7-byte split, but over a tool call -- the JSON arguments are torn
    /// apart mid-string and must still reassemble.
    #[tokio::test]
    async fn reassembles_a_tool_call_split_across_chunks() {
        let body = concat!(
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_x","type":"function","function":{"name":"read_file","arguments":""}}]}}]}"#,
            "\n\n",
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"pa"}}]}}]}"#,
            "\n\n",
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"th\": \"src/app.rs\"}"}}]}}]}"#,
            "\n\n",
            r#"data: {"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
            "\n\n",
            "data: [DONE]\n\n",
        );
        let addr = serve(sse(body), 7).await;

        let (turn, _) = run(&addr).await;
        let turn = turn.expect("the turn should succeed");
        assert_eq!(turn.tool_calls.len(), 1);
        assert_eq!(turn.tool_calls[0].id, "call_x");
        assert_eq!(turn.tool_calls[0].function.name, "read_file");
        assert_eq!(
            turn.tool_calls[0].parsed_arguments().unwrap(),
            serde_json::json!({"path": "src/app.rs"})
        );
    }

    #[tokio::test]
    async fn a_turn_cut_off_at_max_tokens_says_so() {
        let body = concat!(
            r#"data: {"choices":[{"delta":{"content":"half an ans"}}]}"#,
            "\n\n",
            r#"data: {"choices":[{"delta":{},"finish_reason":"length"}]}"#,
            "\n\n",
            "data: [DONE]\n\n",
        );
        let addr = serve(sse(body), 4096).await;

        let (turn, _) = run(&addr).await;
        let turn = turn.expect("the turn should succeed");
        assert!(turn.text.starts_with("half an ans"));
        assert!(turn.text.contains("max_tokens"), "{}", turn.text);
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

        let (turn, tokens) = run(&addr).await;
        assert_eq!(turn.expect("the turn should succeed").text, "Plain reply");
        assert_eq!(tokens.concat(), "Plain reply");
    }

    /// Tool calls also have to survive the non-streaming path.
    #[tokio::test]
    async fn a_non_streaming_response_carries_tool_calls() {
        let body = r#"{"choices":[{"message":{"role":"assistant","content":null,"tool_calls":[{"id":"c1","type":"function","function":{"name":"list_dir","arguments":"{\"path\":\".\"}"}}]}}]}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let addr = serve(response.into_bytes(), 4096).await;

        let (turn, _) = run(&addr).await;
        let turn = turn.expect("the turn should succeed");
        assert!(turn.text.is_empty());
        assert_eq!(turn.tool_calls.len(), 1);
        assert_eq!(turn.tool_calls[0].function.name, "list_dir");
    }

    #[tokio::test]
    async fn http_errors_are_reported_with_the_body() {
        let body = r#"{"error":{"message":"Invalid API key"}}"#;
        let response = format!(
            "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let addr = serve(response.into_bytes(), 4096).await;

        match run(&addr).await.0 {
            Err(e) => {
                assert!(e.contains("401"), "{e}");
                assert!(e.contains("Invalid API key"), "{e}");
            }
            Ok(_) => panic!("expected an error"),
        }
    }

    #[tokio::test]
    async fn unreachable_endpoints_report_an_error_rather_than_hanging() {
        // Port 1 on loopback refuses connections immediately.
        match run("http://127.0.0.1:1").await.0 {
            Err(e) => assert!(e.contains("Could not reach"), "{e}"),
            Ok(_) => panic!("expected an error"),
        }
    }
}
