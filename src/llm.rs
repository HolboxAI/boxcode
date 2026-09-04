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
    /// Asks a streaming endpoint to append token counts. Omitted unless
    /// requested, for the same reason as `tools`: plenty of OpenAI-compatible
    /// servers reject fields they do not recognise, and exact usage is not
    /// worth breaking a working endpoint over.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<StreamOptions>,
    /// Sampling temperature. Omitted entirely when `None` rather than sent as
    /// `null`, so a provider nobody has an opinion about sees exactly the
    /// request it saw before this field existed and falls back to its own
    /// default. See `config::LlmConfig::effective_temperature`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
}

#[derive(Serialize)]
pub struct StreamOptions {
    pub include_usage: bool,
}

/// OpenAI reports the cached share of the prompt nested one level down.
#[derive(Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PromptTokensDetails {
    #[serde(default)]
    pub cached_tokens: usize,
}

/// Token counts as the endpoint reports them, when it does.
#[derive(Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ApiUsage {
    #[serde(default)]
    pub prompt_tokens: usize,
    #[serde(default)]
    pub completion_tokens: usize,
    /// DeepSeek's name for the part of the prompt that hit its context cache.
    #[serde(default)]
    pub prompt_cache_hit_tokens: usize,
    /// OpenAI's name for the same figure. Both are read because the two
    /// providers in `providers.rs` disagree about where to put it, and a
    /// custom endpoint may be either shape.
    #[serde(default)]
    pub prompt_tokens_details: PromptTokensDetails,
}

impl ApiUsage {
    pub fn total(&self) -> usize {
        self.prompt_tokens.saturating_add(self.completion_tokens)
    }

    /// How much of the prompt the endpoint served from cache.
    ///
    /// Not added to `total`: a cached token is a *discounted* prompt token,
    /// already counted in `prompt_tokens`, not an extra one. Reported by at
    /// most one of the two field names, so the larger wins rather than the
    /// sum -- adding them would double-count an endpoint that sends both.
    pub fn cached_prompt_tokens(&self) -> usize {
        self.prompt_cache_hit_tokens
            .max(self.prompt_tokens_details.cached_tokens)
    }
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
    /// `default` because the usage chunk can carry `"choices": []`, and some
    /// endpoints omit the key entirely on that final chunk.
    #[serde(default)]
    pub choices: Vec<Choice>,
    #[serde(default)]
    pub usage: Option<ApiUsage>,
}

#[derive(Deserialize)]
pub struct Choice {
    #[serde(default)]
    pub delta: Delta,
    /// Why the endpoint stopped. `"length"` means it hit the output cap and the
    /// answer is cut off mid-thought -- the difference between a short reply and
    /// a truncated one, which is invisible without this.
    #[serde(default)]
    pub finish_reason: Option<String>,
    /// Present when the endpoint ignores `stream: true` and answers with a plain
    /// (non-SSE) completion body.
    #[serde(default)]
    pub message: Option<ChatMessage>,
}

#[derive(Deserialize, Default)]
pub struct Delta {
    #[serde(default)]
    pub content: Option<String>,
    /// A reasoning model's chain of thought, which arrives *before* any
    /// `content` and on its own field.
    ///
    /// Absent from every OpenAI-reference response and present on DeepSeek's
    /// reasoning models, Alibaba's, and several gateways that proxy them. It
    /// was previously not declared here at all, so serde dropped it: on a long
    /// think the endpoint was streaming continuously, every byte was
    /// discarded, and the UI showed a spinner over a blank screen with a
    /// frozen token counter. That is indistinguishable from a hang, and it was
    /// reported as one.
    ///
    /// `reasoning` is the same field under the name some gateways use, aliased
    /// rather than given a second field so everything downstream sees one
    /// stream regardless of which spelling arrived.
    #[serde(default, alias = "reasoning")]
    pub reasoning_content: Option<String>,
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
    /// A fragment of the model's reasoning, on its way to the live area but
    /// never to the transcript and never back onto the wire.
    ///
    /// Kept separate from `Token` rather than folded into it because the two
    /// have different lifetimes: `Token` is the answer and is persisted,
    /// replayed and resent, while this is scaffolding that exists to prove the
    /// model is working and is dropped the moment it stops.
    Reasoning(String),
    /// `bool` is `finish_reason == "length"` for the response these calls came
    /// from -- the model was cut off by the token budget rather than stopping
    /// on its own. Carried alongside the calls, not as a separate event,
    /// because it describes *this* response and every call in it: a
    /// `write_file`/`edit_file` whose arguments were still streaming when the
    /// cap hit is a truncated write wearing valid JSON, and the tool runner
    /// needs to know that before it trusts the content.
    ToolCalls(Vec<ToolCall>, bool),
    /// Not from the endpoint: the local command runner reports back on the same
    /// channel, so the event loop has one place to drain and one stale-id guard
    /// covering both sources.
    ToolsFinished(Vec<crate::tools::ToolOutcome>),
    /// Not from the endpoint either: a running subagent saying what it just
    /// did -- one event per tool call the child makes, carrying the parent
    /// call's id, the child call's one-line label, and which round the child
    /// is on. The transcript's live area shows the latest one under the
    /// agent's entry, and `/subagents` replays the whole trail afterwards.
    AgentActivity { call_id: String, label: String, rounds: usize },
    /// Exact token counts for the request that just finished, when the endpoint
    /// reports them. Absent rather than guessed: the caller keeps its own
    /// character estimate as the fallback, and knowing which one it has is the
    /// difference between a spend figure and a hunch.
    Usage(ApiUsage),
    Done,
    /// Something the user should know that is not the model talking and not a
    /// failure -- currently only "your answer was truncated".
    Notice(String),
    Error(String),
}

/// Token counts carried by an SSE line, wherever they appear.
///
/// Read separately from `parse_sse_line` because the two are not mutually
/// exclusive on the wire. The OpenAI reference puts usage in a final chunk with
/// `"choices": []`, but other endpoints (DeepSeek among them) attach it to the
/// same chunk that carries `finish_reason` -- which therefore has a choices
/// entry too. Matching on choices first, as `parse_sse_line` must, would
/// silently discard those counts and fall back to estimating.
pub fn usage_of(line: &str) -> Option<ApiUsage> {
    let payload = line.trim_end().strip_prefix("data:")?.trim_start();
    if payload == "[DONE]" {
        return None;
    }
    serde_json::from_str::<StreamDelta>(payload)
        .ok()?
        .usage
        // Some endpoints advertise support and then send zeroes, which is
        // indistinguishable from not reporting at all.
        .filter(|u| u.total() > 0)
}

/// Pull the human-readable part out of an OpenAI-shaped error body.
///
/// The raw body is JSON with the sentence buried three levels down, so printing
/// it verbatim buries the one line the reader needs in punctuation.
fn summarise_error(body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| {
            v.get("error")
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| {
            let trimmed = body.trim();
            if trimmed.is_empty() {
                String::new()
            } else {
                trimmed.chars().take(200).collect()
            }
        })
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

/// Where a request goes and how it is bounded. Grouped so the two entry points
/// below take a handful of arguments rather than a dozen positional strings
/// that are trivial to transpose.
pub struct Target<'a> {
    pub endpoint: &'a str,
    pub model: &'a str,
    pub api_key: &'a str,
    pub max_tokens: u32,
    /// Ask the endpoint for exact token counts. Off leaves the request byte for
    /// byte as it was before this existed.
    pub include_usage: bool,
    /// Sampling temperature to send, or `None` to omit the field and let the
    /// endpoint use its own default. Resolved by the caller from
    /// `config::LlmConfig::effective_temperature`, which is where the
    /// per-provider default (currently DeepSeek only) lives.
    pub temperature: Option<f32>,
}

pub async fn stream_chat(
    target: Target<'_>,
    messages: Vec<ChatMessage>,
    tools: Vec<Value>,
    request_id: u64,
    tx: mpsc::Sender<(u64, StreamEvent)>,
) {
    let result = run(target, messages, tools, request_id, &tx).await;
    let event = match result {
        Ok(()) => StreamEvent::Done,
        Err(e) => StreamEvent::Error(e),
    };
    let _ = tx.send((request_id, event)).await;
}

async fn run(
    target: Target<'_>,
    messages: Vec<ChatMessage>,
    tools: Vec<Value>,
    request_id: u64,
    tx: &mpsc::Sender<(u64, StreamEvent)>,
) -> Result<(), String> {
    let Target { endpoint, model, api_key, max_tokens, include_usage, temperature } = target;
    // A lone system prompt is not a conversation.
    if messages.iter().all(|m| m.role == "system") {
        return Err("Nothing to send.".to_string());
    }

    let sent_tools = !tools.is_empty();
    let request = ChatRequest {
        model: model.to_string(),
        messages,
        stream: true,
        max_tokens,
        tools,
        stream_options: include_usage.then_some(StreamOptions { include_usage: true }),
        temperature,
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

    // Retry the connection/status-check phase only -- never once a token has
    // reached the caller, since replaying a request that already streamed part
    // of an answer would risk duplicate or garbled output. `try_clone` always
    // succeeds here because the body is buffered JSON, not a stream.
    let mut attempt: u32 = 0;
    let response = loop {
        let this_req = req
            .try_clone()
            .expect("request body is buffered JSON, so it is always cloneable");
        let response = this_req.send().await.map_err(|e| network_failure(&url, &e))?;
        let status = response.status();
        if attempt >= MAX_RETRY_ATTEMPTS || !is_retryable_status(status) {
            break response;
        }
        let wait = retry_after_duration(&response).unwrap_or_else(|| backoff_delay(attempt));
        attempt += 1;
        // A plain `.await`: this task is spawned with an `AbortHandle` the app
        // already holds (see `agent.rs`), and aborting it drops this future --
        // including mid-sleep -- so a user cancelling during backoff works the
        // same way cancelling mid-stream already does.
        tokio::time::sleep(wait).await;
    };

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
        // A raw `HTTP 429` status line says nothing a user can act on, and the
        // remedy is the one thing worth saying: it is not a fault.
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(format!(
                "{}\n\nThis is a rate limit, not a fault -- wait a moment and try again.",
                summarise_error(&body)
            ));
        }

        // A conversation that no longer fits is the other common 400, and it
        // is the one with an obvious remedy the user will never guess from the
        // provider's raw wording ("maximum context length is 65536 tokens").
        if crate::notice::classify(&body) == crate::notice::Kind::ContextFull {
            return Err(format!(
                "This conversation is too long for {model}'s context window.\n\n\
                 Every turn resends the whole history, so a long session eventually \
                 stops fitting. /new starts fresh and keeps your provider and model.\n\
                 {}",
                truncate(body.trim(), 300)
            ));
        }

        // The most likely cause of a 400 the moment file tools ship is an
        // endpoint that does not implement tool calling, so name the fix.
        let hint = if status == reqwest::StatusCode::BAD_REQUEST && sent_tools {
            "\n\nIf this endpoint does not support tool calling, disable file tools:\nset `enabled = false` under [tools] in ~/.boxcode/config.toml."
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
        if let Some(usage) = parsed.usage.filter(|u| u.total() > 0) {
            let _ = tx.send((request_id, StreamEvent::Usage(usage))).await;
        }
        if let Some(choice) = parsed.choices.into_iter().next() {
            let truncated = choice.finish_reason.as_deref() == Some("length");
            if let Some(message) = choice.message {
                if let Some(text) = message.content.filter(|t| !t.is_empty()) {
                    let _ = tx.send((request_id, StreamEvent::Token(text))).await;
                }
                let calls = finalize_tool_calls(message.tool_calls);
                if !calls.is_empty() {
                    let _ = tx.send((request_id, StreamEvent::ToolCalls(calls, truncated))).await;
                }
            }
        }
        return Ok(());
    }

    let mut stream = response.bytes_stream();
    // Chunks split arbitrarily: mid-UTF-8-sequence and mid-SSE-line. Buffer bytes
    // and only consume whole lines, otherwise tokens get dropped or decoding fails.
    let mut buf: Vec<u8> = Vec::new();
    let mut pending: Vec<ToolCall> = Vec::new();
    let mut finish_reason: Option<String> = None;
    let mut reported_usage: Option<ApiUsage> = None;

    'read: loop {
        // A stall, not a total time limit. A long generation legitimately runs
        // for minutes, so capping the whole request would kill the answers
        // people most want; what never happens on a healthy stream is a long
        // *gap between chunks*, since bytes arrive continuously once the model
        // starts -- reasoning included. Without this the read simply waits
        // forever: a dropped Wi-Fi link or a server that accepted the request
        // and then went quiet left the spinner turning with nothing behind it
        // and no way to tell that from a slow answer.
        let next = match tokio::time::timeout(STREAM_STALL_TIMEOUT, stream.next()).await {
            Ok(next) => next,
            Err(_) => {
                return Err(format!(
                    "Lost the connection to {}: nothing received for {}s.",
                    host_of(&url),
                    STREAM_STALL_TIMEOUT.as_secs()
                ))
            }
        };
        let Some(chunk) = next else { break 'read };
        // Mid-stream, so the connection was fine a moment ago -- said in those
        // terms rather than as reqwest's chain, for the same reason
        // `network_failure` exists.
        let chunk = chunk.map_err(|_| {
            format!("Lost the connection to {} mid-reply.", host_of(&url))
        })?;
        buf.extend_from_slice(&chunk);

        while let Some(nl) = buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = buf.drain(..=nl).collect();
            let line = String::from_utf8_lossy(&line[..line.len() - 1]);
            let line = line.trim_end_matches('\r');
            if let Some(u) = usage_of(line) {
                reported_usage = Some(u);
            }
            match parse_sse_line(line) {
                SseLine::Done => break 'read,
                SseLine::Delta(delta, reason) => {
                    if reason.is_some() {
                        finish_reason = reason;
                    }
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
        let line = line.trim();
        if let Some(u) = usage_of(line) {
            reported_usage = Some(u);
        }
        if let SseLine::Delta(delta, reason) = parse_sse_line(line) {
            if reason.is_some() {
                finish_reason = reason;
            }
            apply_delta(delta, &mut pending, request_id, tx).await;
        }
    }

    // Before the tool calls, so the ordering the app relies on (tools last
    // before the terminating event) is undisturbed.
    if let Some(usage) = reported_usage {
        let _ = tx.send((request_id, StreamEvent::Usage(usage))).await;
    }

    // Tool calls are only complete once the stream is, since `arguments` is
    // assembled from fragments and is not valid JSON before then. `finish_reason`
    // is already known by now (set as soon as the chunk carrying it arrived), so
    // a call built from a response the endpoint cut off for length is marked as
    // such before anything downstream sees it.
    let calls = finalize_tool_calls(pending);
    if !calls.is_empty() {
        let truncated = finish_reason.as_deref() == Some("length");
        let _ = tx.send((request_id, StreamEvent::ToolCalls(calls, truncated))).await;
    }

    // Truncation is otherwise silent: a cut-off answer looks like a finished
    // one, and a cut that lands before any content at all looks like the
    // endpoint returned nothing. Both send you looking at the wrong thing.
    if finish_reason.as_deref() == Some("length") {
        let _ = tx
            .send((
                request_id,
                StreamEvent::Notice(format!(
                    "The reply hit the {max_tokens}-token output cap and was cut off. \
                     Raise `max_tokens` under [llm] in ~/.boxcode/config.toml, \
                     or ask for the work in smaller pieces."
                )),
            ))
            .await;
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
    if let Some(text) = delta.reasoning_content.filter(|s| !s.is_empty()) {
        if tx
            .send((request_id, StreamEvent::Reasoning(text)))
            .await
            .is_err()
        {
            return false;
        }
    }
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
        Ok(parsed) => parsed
            .choices
            .into_iter()
            .next()
            .map(|c| SseLine::Delta(c.delta, c.finish_reason))
            .unwrap_or(SseLine::Ignore),
        Err(_) => SseLine::Ignore,
    }
}

/// How long the stream may go silent before it is treated as dead.
///
/// Deliberately a gap between chunks rather than a limit on the whole request:
/// a long generation is supposed to take minutes, and a total timeout would
/// abandon exactly the answers that took the most work. Two minutes is far
/// longer than any real pause between chunks -- including the wait for the
/// first one on a large prompt -- and short enough that a connection which
/// died silently does not leave the spinner turning indefinitely.
const STREAM_STALL_TIMEOUT: Duration = Duration::from_secs(120);

/// How many extra attempts a request gets after a retryable failure (429 or
/// 5xx) before giving up and surfacing the error, on top of the first try.
/// Small on purpose: a request stuck retrying is a request the user cannot
/// see progress on, and three chances at exponential backoff already covers
/// the ordinary case of a rate limit or a server hiccup clearing itself.
const MAX_RETRY_ATTEMPTS: u32 = 3;

/// The backoff cap. However high `attempt` climbs, never wait longer than this
/// between retries.
const MAX_BACKOFF: Duration = Duration::from_secs(16);

/// A 429 (rate limited) or 5xx (server-side fault) is worth retrying
/// automatically -- both are usually transient. A 4xx other than 429 (bad
/// key, bad model, malformed request) will fail again identically, so
/// retrying it would only delay the error the user needs to see.
fn is_retryable_status(status: reqwest::StatusCode) -> bool {
    status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}

/// Exponential backoff for the `attempt`-th retry (0-indexed): 1s, 2s, 4s, ...,
/// capped at `MAX_BACKOFF`.
fn backoff_delay(attempt: u32) -> Duration {
    let secs = 1u64.checked_shl(attempt).unwrap_or(u64::MAX);
    Duration::from_secs(secs).min(MAX_BACKOFF)
}

/// The `Retry-After` header, when the endpoint sends one, as the delay-seconds
/// form (`Retry-After: 20`) rather than an HTTP-date. Preferred over the
/// exponential backoff guess whenever it is present, since the endpoint is
/// telling us exactly how long it wants; clamped so a misbehaving endpoint
/// cannot stall a retry indefinitely.
fn retry_after_duration(response: &reqwest::Response) -> Option<Duration> {
    let raw = response.headers().get(reqwest::header::RETRY_AFTER)?.to_str().ok()?;
    let secs: u64 = raw.trim().parse().ok()?;
    Some(Duration::from_secs(secs).min(Duration::from_secs(60)))
}

/// The host part of an endpoint URL, for a message a person has to read.
///
/// `https://api.deepseek.com/v1/chat/completions` is the URL the request went
/// to, and repeating all of it -- three times, as the old message did -- says
/// nothing the host does not.
fn host_of(url: &str) -> &str {
    url.split("://")
        .nth(1)
        .unwrap_or(url)
        .split('/')
        .next()
        .unwrap_or(url)
}

/// Whether any layer of `e`'s source chain mentions `needle`.
///
/// reqwest does not expose "this was a DNS failure" as a predicate, and the
/// only place that fact exists is the wrapped `hyper`/`std::io` error several
/// levels down.
fn chain_mentions(e: &reqwest::Error, needle: &str) -> bool {
    use std::error::Error;
    let mut source: Option<&(dyn Error + 'static)> = Some(e);
    while let Some(err) = source {
        if err.to_string().to_ascii_lowercase().contains(needle) {
            return true;
        }
        source = err.source();
    }
    false
}

/// One short sentence for a network failure.
///
/// Printing `{e}` gave the whole of reqwest's nested chain, which is written
/// for whoever is debugging reqwest and repeats the URL at every layer:
///
/// ```text
/// Could not reach https://api.deepseek.com/v1/chat/completions: error sending
/// request for url (https://api.deepseek.com/v1/chat/completions): error trying
/// to connect: dns error: failed to lookup address information: nodename nor
/// servname provided, or not known
/// Check BOXCODE_ENDPOINT / config.toml.
/// ```
///
/// Four lines, one fact: the name did not resolve. Every layer above that is
/// the library narrating its own call stack, and the trailing advice is said
/// again by `notice.rs`'s hint for this kind of error.
///
/// The `Could not reach` prefix is load-bearing -- `notice::markers::UNREACHABLE`
/// keys on it to pick the "Endpoint unreachable" headline and its hint, so a
/// rewording here silently downgrades the whole class to a plain "Error".
fn network_failure(url: &str, e: &reqwest::Error) -> String {
    let host = host_of(url);
    // Ordered by specificity: a DNS failure is also a connect failure, and
    // "no such host" is the more useful of the two things to be told.
    if chain_mentions(e, "dns error") || chain_mentions(e, "failed to lookup") {
        format!("Could not reach {host}: no such host.")
    } else if e.is_timeout() {
        format!("Could not reach {host}: timed out.")
    } else if chain_mentions(e, "connection refused") {
        format!("Could not reach {host}: connection refused.")
    } else if chain_mentions(e, "certificate") || chain_mentions(e, "tls") {
        format!("Could not reach {host}: TLS handshake failed.")
    } else if e.is_connect() {
        format!("Could not reach {host}.")
    } else {
        // Not a connection problem at all -- a malformed request, a body that
        // would not encode. Still one line, but it must not claim the network
        // is at fault.
        format!("Request to {host} failed.")
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
            SseLine::Delta(delta, _) => delta.content.filter(|s| !s.is_empty()),
            _ => None,
        }
    }

    /// The field that was being dropped. A reasoning model streams its chain
    /// of thought here, ahead of any `content`, and serde discarded it because
    /// `Delta` never declared it -- so a minute of continuous streaming looked
    /// exactly like a hung connection.
    #[test]
    fn reasoning_content_is_read_rather_than_discarded() {
        let reasoning = |line: &str| match parse_sse_line(line) {
            SseLine::Delta(delta, _) => delta.reasoning_content,
            _ => None,
        };
        assert_eq!(
            reasoning(r#"data: {"choices":[{"delta":{"reasoning_content":"Let me check"}}]}"#)
                .as_deref(),
            Some("Let me check")
        );
        // The spelling some gateways use for the same field.
        assert_eq!(
            reasoning(r#"data: {"choices":[{"delta":{"reasoning":"Let me check"}}]}"#).as_deref(),
            Some("Let me check")
        );
        // And it must not be confused with the answer.
        assert!(token(r#"data: {"choices":[{"delta":{"reasoning_content":"Let me check"}}]}"#).is_none());
    }

    /// Reasoning and content on the same chunk: both come through, and the
    /// thinking precedes the answer it produced.
    #[tokio::test]
    async fn reasoning_arrives_before_the_answer_it_produced() {
        let (tx, mut rx) = mpsc::channel(16);
        let mut pending = Vec::new();
        apply_delta(
            Delta {
                content: Some("Hello".to_string()),
                reasoning_content: Some("thinking".to_string()),
                tool_calls: Vec::new(),
            },
            &mut pending,
            1,
            &tx,
        )
        .await;
        drop(tx);

        let mut seen = Vec::new();
        while let Some((_, event)) = rx.recv().await {
            seen.push(match event {
                StreamEvent::Reasoning(t) => format!("reasoning:{t}"),
                StreamEvent::Token(t) => format!("token:{t}"),
                _ => "other".to_string(),
            });
        }
        assert_eq!(seen, vec!["reasoning:thinking", "token:Hello"]);
    }

    /// A network failure is one short line, not reqwest's call stack.
    ///
    /// The message this replaced ran to four lines and repeated the URL three
    /// times, to say one thing: the name did not resolve. Everything above
    /// that was the library narrating its own layers.
    #[tokio::test]
    async fn a_network_failure_is_one_short_line() {
        let (tx, mut rx) = mpsc::channel(8);
        // Port 1 refuses immediately, so this is hermetic and fast -- no DNS,
        // no waiting on a timeout.
        stream_chat(
            Target {
                endpoint: "http://127.0.0.1:1",
                model: "m",
                api_key: "",
                max_tokens: 100,
                include_usage: false,
                temperature: None,
            },
            vec![ChatMessage {
                role: "user".into(),
                content: Some("hi".into()),
                tool_calls: Vec::new(),
                tool_call_id: None,
            }],
            Vec::new(),
            1,
            tx,
        )
        .await;

        let mut error = None;
        while let Ok((_, ev)) = rx.try_recv() {
            if let StreamEvent::Error(e) = ev {
                error = Some(e);
            }
        }
        let error = error.expect("an unreachable endpoint must report something");

        assert_eq!(error.lines().count(), 1, "should be one line: {error:?}");
        assert!(error.contains("127.0.0.1:1"), "should name the host: {error:?}");
        // The layers that made the old message unreadable.
        for noise in ["error sending request", "error trying to connect", "hyper", "os error"] {
            assert!(!error.contains(noise), "{noise:?} leaked through: {error:?}");
        }
        // And the URL is named once, not at every layer.
        assert_eq!(error.matches("127.0.0.1").count(), 1, "{error:?}");
    }

    /// Every one of these has to keep reaching `notice`'s "Endpoint
    /// unreachable" headline and its hint. That classification keys on the
    /// wording, so a rewording silently demotes the whole class to a bare
    /// "Error" with no hint -- which is exactly the sort of thing that is
    /// noticed months later, by a user, offline.
    #[test]
    fn every_network_message_still_classifies_as_offline() {
        let e = |m: &str| crate::notice::classify(m);
        for message in [
            "Could not reach api.deepseek.com: no such host.",
            "Could not reach api.deepseek.com: timed out.",
            "Could not reach api.deepseek.com: connection refused.",
            "Could not reach api.deepseek.com: TLS handshake failed.",
            "Could not reach api.deepseek.com.",
            "Lost the connection to api.deepseek.com mid-reply.",
            "Lost the connection to api.deepseek.com: nothing received for 120s.",
        ] {
            assert_eq!(
                e(message),
                crate::notice::Kind::Offline,
                "{message:?} lost its headline"
            );
        }
        // ...but a failure that is not the network's fault must not claim to
        // be one, or "check your connection" is advice about the wrong thing.
        assert_ne!(
            e("Request to api.deepseek.com failed."),
            crate::notice::Kind::Offline
        );
    }

    #[test]
    fn the_host_is_taken_from_whatever_shape_of_url_arrives() {
        assert_eq!(host_of("https://api.deepseek.com/v1/chat/completions"), "api.deepseek.com");
        assert_eq!(host_of("http://127.0.0.1:1/v1/chat/completions"), "127.0.0.1:1");
        assert_eq!(host_of("https://llm.internal:8443"), "llm.internal:8443");
        // Not a URL at all: better to echo it than to produce an empty message.
        assert_eq!(host_of("nonsense"), "nonsense");
    }

    /// The stall guard has to be long enough that it never fires on a real
    /// answer. It is a gap between chunks, not a limit on the request, because
    /// a total timeout would abandon exactly the long generations people most
    /// want -- if this is ever "simplified" into `Client::timeout`, that is
    /// the bug it introduces.
    #[test]
    fn the_stall_guard_is_a_generous_gap_not_a_request_limit() {
        assert!(
            STREAM_STALL_TIMEOUT >= Duration::from_secs(60),
            "too tight to survive a slow first chunk on a large prompt"
        );
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
            if let SseLine::Delta(delta, _) = parse_sse_line(line) {
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
            if let SseLine::Delta(delta, _) = parse_sse_line(line) {
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

    /// Serve a sequence of canned HTTP responses, one per accepted connection,
    /// in order. Used to exercise retries: an early connection can fail while
    /// a later one succeeds, the way a rate limit clearing itself would.
    async fn serve_sequence(responses: Vec<Vec<u8>>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            for response in responses {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut buf = [0u8; 4096];
                let _ = socket.read(&mut buf).await; // consume the request
                if socket.write_all(&response).await.is_err() {
                    return;
                }
                let _ = socket.flush().await;
                let _ = socket.shutdown().await;
            }
        });
        format!("http://{addr}")
    }

    async fn collect(endpoint: &str) -> Vec<StreamEvent> {
        let (tx, mut rx) = mpsc::channel(64);
        stream_chat(
            Target {
                endpoint,
                model: "test-model",
                api_key: "sk-test",
                max_tokens: 4096,
                include_usage: true,
                temperature: None,
            },
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
                StreamEvent::ToolCalls(calls, _) => Some(calls.clone()),
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

    /// Regression: a reply cut off by the output cap arrived looking exactly
    /// like a finished one, and when the cut landed before any content the app
    /// said "the endpoint returned an empty response" -- blaming the endpoint
    /// for our own 4096-token ceiling. `finish_reason` has to be read and said
    /// out loud.
    #[tokio::test]
    async fn a_reply_truncated_by_the_output_cap_is_reported() {
        let body = "data: {\"choices\":[{\"delta\":{\"content\":\"half a sen\"}}]}\n\n\
                    data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"length\"}]}\n\n\
                    data: [DONE]\n\n";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        let addr = serve(response.into_bytes(), 4096).await;
        let events = collect(&addr).await;

        let notice = events.iter().find_map(|e| match e {
            StreamEvent::Notice(n) => Some(n.clone()),
            _ => None,
        });
        let notice = notice.expect("a truncated reply must say so");
        assert!(notice.contains("cut off"), "{notice}");
        assert!(notice.contains("max_tokens"), "it must name the setting: {notice}");

        // The partial text still arrives -- half an answer beats none.
        assert_eq!(text_of(&events), "half a sen");
    }

    /// The ordinary case must stay quiet: a reply that simply ended has nothing
    /// to warn about.
    #[tokio::test]
    async fn a_reply_that_ends_normally_produces_no_notice() {
        let body = "data: {\"choices\":[{\"delta\":{\"content\":\"done\"},\"finish_reason\":\"stop\"}]}\n\n\
                    data: [DONE]\n\n";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        let addr = serve(response.into_bytes(), 4096).await;
        let events = collect(&addr).await;

        assert!(
            !events.iter().any(|e| matches!(e, StreamEvent::Notice(_))),
            "a normal reply must not warn"
        );
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
                StreamEvent::Reasoning(_) => "reasoning",
                StreamEvent::ToolCalls(..) => "tools",
                StreamEvent::ToolsFinished(_) => "finished",
                StreamEvent::AgentActivity { .. } => "activity",
                StreamEvent::Usage(_) => "usage",
                StreamEvent::Done => "done",
                StreamEvent::Notice(_) => "notice",
                StreamEvent::Error(_) => "error",
            })
            .collect();
        assert_eq!(kinds, vec!["token", "tools", "done"]);
    }

    fn truncated_flag_of(events: &[StreamEvent]) -> Option<bool> {
        events.iter().find_map(|e| match e {
            StreamEvent::ToolCalls(_, truncated) => Some(*truncated),
            _ => None,
        })
    }

    /// A tool call whose response was cut off by the token cap must carry
    /// that fact, so `write_file`/`edit_file` can refuse content that never
    /// finished generating instead of writing it to disk as if it had.
    #[tokio::test]
    async fn a_tool_call_cut_off_by_length_is_flagged_truncated() {
        let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"write_file\",\"arguments\":\"{\\\"path\\\":\\\"a.rs\\\",\\\"content\\\":\\\"fn a\\\"}\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"length\"}]}\n\n",
            "data: [DONE]\n\n",
        );
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        let addr = serve(response.into_bytes(), 4096).await;
        let events = collect(&addr).await;

        assert_eq!(truncated_flag_of(&events), Some(true));
    }

    /// The ordinary case: a tool call that arrives because the model finished
    /// on its own must not be flagged.
    #[tokio::test]
    async fn a_tool_call_that_finishes_normally_is_not_flagged_truncated() {
        let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"read_file\",\"arguments\":\"{\\\"path\\\":\\\"a.rs\\\"}\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
            "data: [DONE]\n\n",
        );
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        let addr = serve(response.into_bytes(), 4096).await;
        let events = collect(&addr).await;

        assert_eq!(truncated_flag_of(&events), Some(false));
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

    /// The non-streaming fallback path reads `finish_reason` from the same
    /// choice as the tool calls, not just the streaming path.
    #[tokio::test]
    async fn a_non_streaming_tool_call_carries_its_finish_reason() {
        let body = r#"{"choices":[{"finish_reason":"length","message":{"role":"assistant","content":null,"tool_calls":[{"id":"call_9","type":"function","function":{"name":"write_file","arguments":"{\"path\":\"a.rs\",\"content\":\"fn a\"}"}}]}}]}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let addr = serve(response.into_bytes(), 4096).await;

        assert_eq!(truncated_flag_of(&collect(&addr).await), Some(true));
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
            Target {
                endpoint: &addr,
                model: "m",
                api_key: "k",
                max_tokens: 4096,
                include_usage: true,
                temperature: None,
            },
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

    /// The reference implementation puts usage in a trailing chunk with
    /// `"choices": []`.
    #[test]
    fn usage_is_read_from_a_trailing_chunk_with_no_choices() {
        let u = usage_of(r#"data: {"choices":[],"usage":{"prompt_tokens":7,"completion_tokens":8}}"#)
            .expect("usage");
        assert_eq!(u.prompt_tokens, 7);
        assert_eq!(u.completion_tokens, 8);
    }

    /// ...but other endpoints attach it to the chunk carrying `finish_reason`,
    /// which therefore has a choices entry. Matching on choices first would
    /// discard the counts and silently fall back to estimating.
    #[test]
    fn usage_is_read_even_when_it_rides_along_with_a_choices_entry() {
        let line = r#"data: {"choices":[{"index":0,"delta":{"content":""},"finish_reason":"stop"}],"usage":{"prompt_tokens":118,"completion_tokens":33406}}"#;
        let u = usage_of(line).expect("usage alongside a choice");
        assert_eq!(u.prompt_tokens, 118);
        assert_eq!(u.completion_tokens, 33_406);
    }

    /// An endpoint that accepts `include_usage` and then reports zeroes is
    /// indistinguishable from one that does not report at all.
    #[test]
    fn an_all_zero_report_is_treated_as_no_report() {
        assert!(usage_of(r#"data: {"choices":[],"usage":{"prompt_tokens":0,"completion_tokens":0}}"#).is_none());
        assert!(usage_of(r#"data: {"choices":[{"delta":{"content":"x"}}]}"#).is_none());
        assert!(usage_of("data: [DONE]").is_none());
    }

    /// `stream_options` is the field most likely to be rejected by a minimal
    /// OpenAI-compatible server, so it must be absent unless asked for.
    #[test]
    fn stream_options_is_omitted_entirely_when_usage_is_not_requested() {
        let base = ChatRequest {
            model: "m".to_string(),
            messages: vec![ChatMessage::text("user", "hi")],
            stream: true,
            max_tokens: 4096,
            tools: Vec::new(),
            stream_options: None,
            temperature: None,
        };
        assert!(!serde_json::to_string(&base).unwrap().contains("stream_options"));

        let with = ChatRequest { stream_options: Some(StreamOptions { include_usage: true }), ..base };
        assert!(serde_json::to_string(&with)
            .unwrap()
            .contains(r#""stream_options":{"include_usage":true}"#));
    }

    /// The field this whole change adds: present and exactly `0.0` when the
    /// DeepSeek default kicks in, absent entirely -- not sent as `null` --
    /// for a provider nobody has configured an opinion for. An endpoint that
    /// has never heard of the field should see the same request it saw
    /// before this existed, the same reasoning `tools` and `stream_options`
    /// are already held to above.
    #[test]
    fn temperature_is_omitted_unless_configured_and_present_when_it_is() {
        let base = ChatRequest {
            model: "deepseek-v4-pro".to_string(),
            messages: vec![ChatMessage::text("user", "hi")],
            stream: true,
            max_tokens: 4096,
            tools: Vec::new(),
            stream_options: None,
            temperature: None,
        };
        assert!(!serde_json::to_string(&base).unwrap().contains("temperature"));

        // What `effective_temperature` resolves to for the DeepSeek provider
        // when `config.toml` sets nothing: an explicit, deterministic 0.0.
        let deepseek_default = ChatRequest { temperature: Some(0.0), ..base };
        assert!(serde_json::to_string(&deepseek_default)
            .unwrap()
            .contains(r#""temperature":0.0"#));
    }

    #[tokio::test]
    async fn a_stream_that_reports_usage_emits_it_before_done() {
        let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Hi\"}}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":123,\"completion_tokens\":45}}\n\n",
            "data: [DONE]\n\n",
        );
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{body}"
        );
        let addr = serve(response.into_bytes(), 7).await;

        let events = collect(&addr).await;
        let usage = events.iter().find_map(|e| match e {
            StreamEvent::Usage(u) => Some(*u),
            _ => None,
        });
        assert_eq!(
            usage,
            Some(ApiUsage { prompt_tokens: 123, completion_tokens: 45, ..Default::default() })
        );
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

    #[test]
    fn only_429_and_5xx_are_retried() {
        for code in [429, 500, 502, 503, 599] {
            assert!(
                is_retryable_status(reqwest::StatusCode::from_u16(code).unwrap()),
                "{code} should be retried"
            );
        }
        // Client errors other than 429 will fail again identically -- retrying
        // them only delays the error the user actually needs to see.
        for code in [400, 401, 403, 404] {
            assert!(
                !is_retryable_status(reqwest::StatusCode::from_u16(code).unwrap()),
                "{code} should not be retried"
            );
        }
    }

    #[test]
    fn backoff_doubles_and_is_capped() {
        assert_eq!(backoff_delay(0), Duration::from_secs(1));
        assert_eq!(backoff_delay(1), Duration::from_secs(2));
        assert_eq!(backoff_delay(2), Duration::from_secs(4));
        // However high the attempt count climbs, it never exceeds the cap.
        assert_eq!(backoff_delay(10), MAX_BACKOFF);
    }

    /// A 429 with no `Retry-After` is retried automatically -- a rate limit is
    /// exactly the transient failure this exists for -- and the retry can
    /// succeed, with the answer reaching the caller as if the first attempt
    /// had never happened.
    ///
    /// `Retry-After: 0` keeps this test fast without weakening what it checks:
    /// the code path taken is the same one a real rate limit's backoff would
    /// use, just with a delay of zero.
    #[tokio::test]
    async fn a_429_is_retried_and_the_retry_can_succeed() {
        let body = r#"{"error":{"message":"slow down"}}"#;
        let rate_limited = format!(
            "HTTP/1.1 429 Too Many Requests\r\nContent-Type: application/json\r\n\
             Retry-After: 0\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes();
        let ok_body = "data: {\"choices\":[{\"delta\":{\"content\":\"Hi\"}}]}\n\ndata: [DONE]\n\n";
        let ok = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{ok_body}"
        )
        .into_bytes();
        let addr = serve_sequence(vec![rate_limited, ok]).await;

        let events = collect(&addr).await;
        assert_eq!(text_of(&events), "Hi");
        assert!(matches!(events.last(), Some(StreamEvent::Done)));
    }

    /// Retries are bounded: a rate limit that never clears still ends in the
    /// same user-facing error as before, not an infinite or unbounded wait.
    #[tokio::test]
    async fn a_429_that_never_clears_still_surfaces_the_original_error() {
        let body = r#"{"error":{"message":"slow down"}}"#;
        let responses = (0..=MAX_RETRY_ATTEMPTS)
            .map(|_| {
                format!(
                    "HTTP/1.1 429 Too Many Requests\r\nContent-Type: application/json\r\n\
                     Retry-After: 0\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .into_bytes()
            })
            .collect();
        let addr = serve_sequence(responses).await;

        let events = collect(&addr).await;
        match events.last() {
            Some(StreamEvent::Error(e)) => assert!(e.contains("rate limit"), "{e}"),
            other => panic!("expected an Error event, got {other:?}"),
        }
    }
}
