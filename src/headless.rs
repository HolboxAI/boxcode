//! A headless, RPC-driven counterpart to `App`'s agent loop -- what
//! `protocol.rs`'s ACP types actually get driven by. `App` itself is
//! untouched by this module; see `approval.rs`'s `Verdict`/`verdict_for`
//! docs for why the two don't share state, only the one piece of logic
//! (`verdict_for`) where drift would be dangerous.
//!
//! Deliberately narrower than `App` for a first working version, matching
//! an independent review's explicit recommendation (see `approval.rs`):
//! - No plan mode. `mode` is fixed at `Mode::Normal`.
//! - No deploy tool. `Action::Deploy` needs `deploy_takes_over`'s real
//!   terminal-based OAuth flow (`app.rs`'s own doc comment: deployment "may
//!   need... the terminal itself for a browser login"), which a headless
//!   session has no answer for yet. The deploy schema is never offered
//!   (`deploy: false` in `tools::schemas_for`), so the model never sees the
//!   tool at all -- refusing a call that was never offered, rather than
//!   offering one and always refusing it.
//! - No subagents. `Action::Agent` calls are refused with an explanation,
//!   same reasoning: `ACP` has no nested-session concept (`protocol.rs`'s
//!   own docs), so there's nowhere on the wire for a subagent's progress to
//!   go yet.
//! - No compaction. `App::finish_compaction`'s logic isn't replicated here;
//!   a long headless session just keeps growing its own history for now.
//!
//! One round of `stream_chat` is consumed the same way `agent::run_subagent`
//! already does (`tokio::select!` over the pinned stream future and its
//! event channel) -- a proven pattern in this codebase, not a new one
//! invented for this module.

use crate::approval::{verdict_for, Decision, Verdict};
use crate::config::Config;
use crate::llm::{self, ApiUsage, ChatMessage, StreamEvent, Target, ToolCall};
use crate::protocol::{
    AcpToolCall, ContentBlock, PermissionOption, PermissionOptionId, PermissionOptionKind,
    RequestPermissionOutcome, RequestPermissionRequest, SessionId, SessionUpdate, StopReason,
    ToolCallContent, ToolCallId, ToolCallStatus, ToolCallUpdate, ToolKind,
};
use crate::tools::{self, Action, Mode};
use crate::workspace::Workspace;
use std::path::Path;
use tokio::sync::{mpsc, oneshot};

/// What a caller (the real stdio transport, or a test) gets sent for each
/// `RequestPermissionRequest` a turn needs answered -- paired with a
/// `oneshot` so `prompt()` can `await` the one reply it's asking for,
/// without needing a request-id-keyed table the way a general JSON-RPC
/// client would (a `HeadlessSession` only ever has one permission request
/// outstanding at a time, since it blocks on each one before continuing).
pub struct PermissionAsk {
    pub request: RequestPermissionRequest,
    pub respond: oneshot::Sender<RequestPermissionOutcome>,
}

/// What a caller gets sent for each `check_in_browser` call -- same shape
/// and same reasoning as `PermissionAsk`: paired with a `oneshot` rather
/// than a request-id-keyed table, since a `HeadlessSession` only ever has
/// one of these outstanding at a time (it blocks on the reply before
/// continuing). A separate type and a separate channel from `PermissionAsk`
/// rather than folding this into one generic "client request" concept --
/// this codebase's own convention (see `transport.rs`'s docs) is a small,
/// explicit type per real use, not a shared abstraction built for two data
/// points.
pub struct BrowserCheckAsk {
    pub session_id: SessionId,
    pub url: String,
    pub respond: oneshot::Sender<BrowserCheckResult>,
}

/// What the client reports back for one `BrowserCheckAsk`. `Failed` covers
/// everything that can go wrong on the client's side (no browser tab, CDP
/// failed, timed out, `url` never loaded) -- boxcode has no way to tell
/// those apart from here, only to say the check didn't happen and hand the
/// model text it can react to, the same posture `ask_permission` already
/// takes toward "the client disconnected."
pub enum BrowserCheckResult {
    Screenshot { mime_type: String, data: String },
    Failed(String),
}

/// One ACP session's worth of state -- deliberately independent of `App`,
/// not extracted from it. See this module's own docs for the narrower
/// scope (no plan mode, no deploy, no subagents, no compaction) that keeps
/// this a small, honestly-scoped first version rather than a shadow `App`.
pub struct HeadlessSession {
    session_id: SessionId,
    workspace: Workspace,
    config: Config,
    messages: Vec<ChatMessage>,
    request_id: u64,
    tool_steps: usize,
    /// The same journal `App::rollback`/`/rollback` reads and writes,
    /// populated the identical way `App::push_tool_outcome` does (see
    /// `decide_and_run`/`ask_permission`'s own calls to `.record(...)`) --
    /// not a parallel reimplementation, the same mechanism reused.
    rollback: crate::rollback::Journal,
}

impl HeadlessSession {
    pub fn new(session_id: SessionId, workspace: Workspace, config: Config) -> Self {
        Self {
            session_id,
            workspace,
            config,
            messages: Vec::new(),
            request_id: 0,
            tool_steps: 0,
            rollback: crate::rollback::Journal::default(),
        }
    }

    /// Handles one `session/prompt`: appends the user's message, then runs
    /// rounds of fire-stream-decide-execute until the model answers with no
    /// further tool calls or a hard stop condition is hit. Blocks until
    /// done, per ACP v1's `PromptResponse` -- progress streams out via
    /// `updates` while this runs.
    pub async fn prompt(
        &mut self,
        text: String,
        updates: &mpsc::Sender<SessionUpdate>,
        permissions: &mpsc::Sender<PermissionAsk>,
        browser: &mpsc::Sender<BrowserCheckAsk>,
    ) -> StopReason {
        self.messages.push(ChatMessage::text("user", text));

        // `App`'s own welcome screen already surfaces a missing key via
        // `Config::warnings()` before the user ever types a prompt -- an ACP
        // client has no equivalent surface, so without this check the exact
        // same misconfiguration would only show up as a raw connection/HTTP
        // failure after a real round trip to `self.config.llm.endpoint`,
        // which is a strictly worse version of the same answer arriving
        // slower and less clearly.
        if self.config.llm.api_key.is_empty() {
            let _ = updates
                .send(SessionUpdate::AgentMessageChunk {
                    content: ContentBlock::Text {
                        text: "No API key set. Export BOXCODE_API_KEY or add api_key to \
                               ~/.boxcode/config.toml."
                            .to_string(),
                    },
                    message_id: None,
                })
                .await;
            return StopReason::Refusal;
        }

        loop {
            let budget_left = self.tool_steps < self.config.tools.max_steps;
            if !budget_left {
                // Same reasoning as agent::fire_request: withholding the
                // schemas is what actually stops a runaway loop, not asking
                // nicely in the prompt.
            }

            let schemas = if budget_left {
                tools::schemas_for(
                    Mode::Normal,
                    false, // no plan mode in v1 -- see module docs
                    false, // no deploy tool in v1 -- see module docs
                    false, // published-artifact detection not wired yet
                    true, // check_in_browser -- the one action only an ACP client can fulfill
                    tools::SchemaDiet::for_workspace(self.workspace.root()),
                )
            } else {
                Vec::new()
            };
            let system = tools::system_prompt(&self.workspace, &self.config.tools, self.tool_steps, Mode::Normal);
            let mut history = self.messages.clone();
            history.insert(0, ChatMessage::text("system", system));
            if let Some(status) = tools::turn_status(&self.config.tools, self.tool_steps, None) {
                history.push(ChatMessage::text("user", status));
            }

            self.request_id += 1;
            let round = self.run_one_round(history, schemas, updates).await;

            match round {
                RoundOutcome::Answered => return StopReason::EndTurn,
                RoundOutcome::Error(message) => {
                    let _ = updates
                        .send(SessionUpdate::AgentMessageChunk {
                            content: ContentBlock::Text { text: format!("Error: {message}") },
                            message_id: None,
                        })
                        .await;
                    return StopReason::Refusal;
                }
                RoundOutcome::ToolCalls(calls) => {
                    self.tool_steps += 1;
                    let outcomes = self.decide_and_run(calls, updates, permissions, browser).await;
                    for (call_id, content) in outcomes {
                        self.messages.push(ChatMessage {
                            role: "tool".to_string(),
                            content: Some(content),
                            tool_calls: Vec::new(),
                            tool_call_id: Some(call_id),
                        });
                    }
                    if !budget_left {
                        return StopReason::MaxTurnRequests;
                    }
                    // Loop again: the model gets to react to what just ran.
                }
            }
        }
    }

    /// One `stream_chat` round, consumed the same way `agent::run_subagent`
    /// already does -- see that function's own comments for why
    /// `tokio::select!` over the pinned future is the right shape here
    /// (this was not reinvented for this module).
    async fn run_one_round(
        &mut self,
        history: Vec<ChatMessage>,
        schemas: Vec<serde_json::Value>,
        updates: &mpsc::Sender<SessionUpdate>,
    ) -> RoundOutcome {
        let (tx, mut rx) = mpsc::channel(64);
        let target = Target {
            endpoint: &self.config.llm.endpoint,
            model: &self.config.llm.model,
            api_key: &self.config.llm.api_key,
            max_tokens: self.config.llm.max_tokens,
            include_usage: self.config.quota.enabled && self.config.quota.include_usage,
            temperature: self.config.llm.effective_temperature(),
        };
        let stream = llm::stream_chat(target, history.clone(), schemas, self.request_id, tx);
        tokio::pin!(stream);

        let mut stream_finished = false;
        let mut text = String::new();
        let mut calls: Vec<ToolCall> = Vec::new();
        let mut usage: Option<ApiUsage> = None;
        let outcome = loop {
            tokio::select! {
                _ = &mut stream, if !stream_finished => stream_finished = true,
                received = rx.recv() => match received {
                    Some((_, StreamEvent::Token(t))) => {
                        text.push_str(&t);
                        let _ = updates
                            .send(SessionUpdate::AgentMessageChunk {
                                content: ContentBlock::Text { text: t },
                                message_id: None,
                            })
                            .await;
                    }
                    // Reasoning is deliberately not forwarded -- see
                    // protocol.rs's module docs for why: no ACP-compatible
                    // way to signal "thinking" exists yet that doesn't
                    // either require content (chunks can't be content-free)
                    // or misuse a mode-change notification.
                    Some((_, StreamEvent::Reasoning(_))) => {}
                    Some((_, StreamEvent::ToolCalls(c, _))) => calls = c,
                    Some((_, StreamEvent::Usage(u))) => usage = Some(u),
                    Some((_, StreamEvent::Notice(_) | StreamEvent::AgentActivity { .. })) => {}
                    Some((_, StreamEvent::ToolsFinished(_))) => {}
                    Some((_, StreamEvent::Done)) | None => break None,
                    Some((_, StreamEvent::Error(e))) => break Some(e),
                },
            }
        };

        if let Some(error) = outcome {
            return RoundOutcome::Error(error);
        }

        if let Some(u) = usage {
            let _ = updates.send(u.into()).await;
        }

        if !calls.is_empty() {
            // Mirrors App::request_tools: whatever prose streamed alongside
            // the tool calls still belongs in history, even though it's
            // not the final answer.
            self.messages.push(ChatMessage {
                role: "assistant".to_string(),
                content: (!text.trim().is_empty()).then_some(text),
                tool_calls: calls.clone(),
                tool_call_id: None,
            });
            return RoundOutcome::ToolCalls(calls);
        }

        if !text.trim().is_empty() {
            self.messages.push(ChatMessage::text("assistant", text));
        }
        RoundOutcome::Answered
    }

    /// Applies `verdict_for` to each queued call -- the one piece of logic
    /// this module shares with `App` rather than reimplementing, per
    /// `approval::Verdict`'s own docs. Returns `(call_id, content)` pairs
    /// ready to become `tool` messages.
    async fn decide_and_run(
        &mut self,
        calls: Vec<ToolCall>,
        updates: &mpsc::Sender<SessionUpdate>,
        permissions: &mpsc::Sender<PermissionAsk>,
        browser: &mpsc::Sender<BrowserCheckAsk>,
    ) -> Vec<(String, String)> {
        let mut results = Vec::with_capacity(calls.len());
        for call in calls {
            let verdict = verdict_for(
                &call,
                Path::new(self.workspace.root()),
                Mode::Normal,
                self.config.tools.approval,
            );

            let content = match verdict {
                Verdict::Blocked(reason) => {
                    let _ = updates
                        .send(SessionUpdate::ToolCallUpdate(ToolCallUpdate {
                            tool_call_id: ToolCallId(call.id.clone()),
                            title: None,
                            kind: None,
                            status: Some(ToolCallStatus::Failed),
                            content: Some(ToolCallContent::Text { text: reason.clone() }),
                        }))
                        .await;
                    reason
                }
                Verdict::PlanRefused(reason) => reason,
                Verdict::Progress { .. } | Verdict::Todos(_) => {
                    // No plan/todo state kept in a headless session yet
                    // (see module docs: no plan mode). Acknowledged, not
                    // silently dropped -- the model still gets a real
                    // answer so the transcript stays valid.
                    "Noted (not tracked in this session).".to_string()
                }
                Verdict::AutoApprove => {
                    let action = tools::describe_action(&call);
                    // `check_in_browser` is the one auto-approved action
                    // `tools::execute` cannot fulfill at all -- boxcode has
                    // no browser tab, only an ACP client does. Intercepted
                    // here, before ever reaching the local dispatcher, the
                    // same way `Action::Agent` is intercepted below rather
                    // than offered to `tools::execute`.
                    if let Some(Action::CheckInBrowser { url }) = &action {
                        self.check_browser(&call, url, updates, browser).await
                    } else {
                        let _ = updates
                            .send(SessionUpdate::ToolCall(AcpToolCall {
                                tool_call_id: ToolCallId(call.id.clone()),
                                title: action
                                    .map(|a| a.label())
                                    .unwrap_or_else(|| call.function.name.clone()),
                                kind: ToolKind::Other,
                                status: ToolCallStatus::InProgress,
                            }))
                            .await;
                        let mut outcome = tools::execute(&call, &self.workspace, &self.config.tools).await;
                        // Same call, same place in the flow as
                        // `App::push_tool_outcome`'s own -- before the
                        // outcome is taken apart, since `.content` moves out
                        // of it just below.
                        if let Some(record) = outcome.rollback.take() {
                            self.rollback.record(record);
                        }
                        let _ = updates.send(SessionUpdate::ToolCallUpdate((&outcome).into())).await;
                        outcome.content
                    }
                }
                Verdict::Ask(action) => {
                    if action.label() == "agent" || matches!(action, crate::tools::Action::Agent { .. }) {
                        // No subagents in v1 -- see module docs.
                        "Subagents aren't supported in this session yet.".to_string()
                    } else {
                        self.ask_permission(&call, &action, updates, permissions).await
                    }
                }
            };

            results.push((call.id.clone(), content));
        }
        results
    }

    async fn ask_permission(
        &mut self,
        call: &ToolCall,
        action: &crate::tools::Action,
        updates: &mpsc::Sender<SessionUpdate>,
        permissions: &mpsc::Sender<PermissionAsk>,
    ) -> String {
        // Same call, same reasoning as `App::advance_approvals`'s own
        // `tools::preview_change(&action, ...)`: computed once, here, where
        // the question is being asked -- not re-derived by whatever renders
        // it. The text shape (not `preview_change`'s hunked `FileDiff`,
        // which stays TUI-only) is what an ACP client with its own diff
        // renderer wants; see `preview_change_text`'s own doc comment.
        let diff_content = tools::preview_change_text(action, Path::new(self.workspace.root())).map(
            |(path, before, after)| ToolCallContent::Diff {
                path,
                old_text: (!before.is_empty()).then_some(before),
                new_text: after,
            },
        );
        let tool_call_update = ToolCallUpdate {
            tool_call_id: ToolCallId(call.id.clone()),
            title: Some(action.label()),
            kind: None,
            status: Some(ToolCallStatus::Pending),
            content: diff_content,
        };
        let request = RequestPermissionRequest {
            session_id: self.session_id.clone(),
            tool_call: tool_call_update,
            options: vec![
                PermissionOption {
                    option_id: PermissionOptionId("allow".to_string()),
                    name: "Allow".to_string(),
                    kind: PermissionOptionKind::AllowOnce,
                },
                PermissionOption {
                    option_id: PermissionOptionId("reject".to_string()),
                    name: "Reject".to_string(),
                    kind: PermissionOptionKind::RejectOnce,
                },
            ],
        };
        let (respond, receive) = oneshot::channel();
        if permissions.send(PermissionAsk { request, respond }).await.is_err() {
            return "The client disconnected before answering.".to_string();
        }
        let decision: Decision = match receive.await {
            Ok(outcome) => outcome.into(),
            Err(_) => Decision::Refused,
        };

        if decision.is_allowed() {
            let mut outcome = tools::execute(call, &self.workspace, &self.config.tools).await;
            if let Some(record) = outcome.rollback.take() {
                self.rollback.record(record);
            }
            let _ = updates.send(SessionUpdate::ToolCallUpdate((&outcome).into())).await;
            outcome.content
        } else {
            let _ = updates
                .send(SessionUpdate::ToolCallUpdate(ToolCallUpdate {
                    tool_call_id: ToolCallId(call.id.clone()),
                    title: None,
                    kind: None,
                    status: Some(ToolCallStatus::Failed),
                    content: Some(ToolCallContent::Text { text: "Refused by user.".to_string() }),
                }))
                .await;
            "The user declined this action.".to_string()
        }
    }

    /// Fulfills a `check_in_browser` call by asking the client to take the
    /// screenshot -- boxcode itself has no browser tab, only the client
    /// does. Mirrors `ask_permission`'s own shape (send an ask, await one
    /// reply on a fresh `oneshot`) since a `HeadlessSession` only ever has
    /// one of either outstanding at a time.
    ///
    /// The tool result text fed back to the *model* stays plain
    /// confirmation, never the image bytes: this is evidence for the human
    /// (rendered by the client, e.g. inline in a chat panel via the
    /// `SessionUpdate::ToolCallUpdate` sent below), not a claim that the
    /// model can see it too. Actually feeding an image into the model's own
    /// context would need real multimodal message support in `llm.rs`,
    /// which does not exist yet -- a separate, bigger scope than showing
    /// the human what was captured.
    async fn check_browser(
        &self,
        call: &ToolCall,
        url: &str,
        updates: &mpsc::Sender<SessionUpdate>,
        browser: &mpsc::Sender<BrowserCheckAsk>,
    ) -> String {
        let _ = updates
            .send(SessionUpdate::ToolCall(AcpToolCall {
                tool_call_id: ToolCallId(call.id.clone()),
                title: format!("check in browser — {url}"),
                kind: ToolKind::Fetch,
                status: ToolCallStatus::InProgress,
            }))
            .await;

        let (respond, receive) = oneshot::channel();
        let ask = BrowserCheckAsk { session_id: self.session_id.clone(), url: url.to_string(), respond };
        if browser.send(ask).await.is_err() {
            return "The client disconnected before taking the screenshot.".to_string();
        }
        let result = match receive.await {
            Ok(result) => result,
            Err(_) => {
                BrowserCheckResult::Failed("The client disconnected before responding.".to_string())
            }
        };

        match result {
            BrowserCheckResult::Screenshot { mime_type, data } => {
                let _ = updates
                    .send(SessionUpdate::ToolCallUpdate(ToolCallUpdate {
                        tool_call_id: ToolCallId(call.id.clone()),
                        title: None,
                        kind: None,
                        status: Some(ToolCallStatus::Completed),
                        content: Some(ToolCallContent::Image { mime_type, data }),
                    }))
                    .await;
                format!("Screenshot of {url} captured and shown to the user.")
            }
            BrowserCheckResult::Failed(reason) => {
                let _ = updates
                    .send(SessionUpdate::ToolCallUpdate(ToolCallUpdate {
                        tool_call_id: ToolCallId(call.id.clone()),
                        title: None,
                        kind: None,
                        status: Some(ToolCallStatus::Failed),
                        content: Some(ToolCallContent::Text { text: reason.clone() }),
                    }))
                    .await;
                format!("Could not check {url} in the browser: {reason}")
            }
        }
    }

    /// Fulfills `session/rollback` -- a plain client-initiated request, not
    /// deferred like `session/prompt` (this is local disk I/O, not an LLM
    /// round trip, so there's no risk of the deadlock class that made
    /// `session/prompt`'s own response need deferring; see transport.rs's
    /// docs on that).
    ///
    /// Same three steps as `App::finish_rollback`, the same order, for the
    /// same reasons: run the plan, clear the journal (every entry has now
    /// been acted on), and put `report.notice()` in the model's own history
    /// so its next edit isn't reasoning about a disk that no longer
    /// matches what it was told -- `Report::notice`'s own doc comment says
    /// why that has to reach the wire, not just the human.
    pub fn rollback(&mut self) -> crate::protocol::RollbackResponse {
        let steps = self.rollback.plan();
        let report = crate::rollback::apply(&steps);
        self.rollback.clear();
        self.messages.push(ChatMessage::text("user", report.notice()));
        crate::protocol::RollbackResponse { summary: report.summary() }
    }
}

enum RoundOutcome {
    Answered,
    ToolCalls(Vec<ToolCall>),
    Error(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn sse(body: &str) -> String {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{body}"
        )
    }

    fn text_round(text: &str) -> String {
        sse(&format!(
            "data: {{\"choices\":[{{\"delta\":{{\"content\":\"{text}\"}}}}]}}\n\ndata: [DONE]\n\n"
        ))
    }

    fn tool_call_round(name: &str, args: &str) -> String {
        let escaped = args.replace('\\', "\\\\").replace('"', "\\\"");
        sse(&format!(
            "data: {{\"choices\":[{{\"delta\":{{\"tool_calls\":[{{\"index\":0,\"id\":\"call_1\",\
             \"type\":\"function\",\"function\":{{\"name\":\"{name}\",\"arguments\":\
             \"{escaped}\"}}}}]}}}}]}}\n\ndata: [DONE]\n\n"
        ))
    }

    async fn read_request(socket: &mut tokio::net::TcpStream) -> String {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let n = socket.read(&mut chunk).await.unwrap_or(0);
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            if let Some(header_end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&buf[..header_end]).to_lowercase();
                let length: usize = headers
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .and_then(|v| v.trim().parse().ok())
                    .unwrap_or(0);
                if buf.len() >= header_end + 4 + length {
                    break;
                }
            }
        }
        String::from_utf8_lossy(&buf).to_string()
    }

    async fn serve_rounds(responses: Vec<String>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            for response in responses {
                let (mut socket, _) = listener.accept().await.expect("accept");
                let _ = read_request(&mut socket).await;
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            }
        });
        format!("http://{addr}")
    }

    fn config_for(endpoint: &str) -> Config {
        let mut config = Config::default();
        config.llm.endpoint = endpoint.to_string();
        config.llm.model = "test-model".to_string();
        config.llm.api_key = "sk-test".to_string();
        config
    }

    fn workspace() -> (tempfile::TempDir, Workspace) {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(dir.path().join("hello.txt"), "hi\n").unwrap();
        let ws = Workspace::new(dir.path()).expect("workspace");
        (dir, ws)
    }

    /// A missing API key must fail fast and clearly -- before ever touching
    /// the network -- both because that's a strictly better answer for a
    /// real misconfiguration, and because an ACP client (unlike the TUI,
    /// which shows this on its own welcome screen via `Config::warnings()`)
    /// has no other way to learn about it. `config.llm.endpoint`
    /// deliberately points nowhere real: if this check were ever skipped,
    /// the assertion on the exact message text below would fail against
    /// whatever generic connection error came back instead, rather than
    /// this test simply hanging -- proving the check runs before any
    /// network attempt, not just that *some* error eventually surfaces.
    #[tokio::test]
    async fn a_missing_api_key_fails_before_any_network_call() {
        let (_dir, ws) = workspace();
        let mut config = config_for("http://127.0.0.1:1");
        config.llm.api_key = String::new();
        let mut session = HeadlessSession::new(SessionId("s1".to_string()), ws, config);
        let (updates_tx, mut updates_rx) = mpsc::channel(16);
        let (permissions_tx, _permissions_rx) = mpsc::channel(16);
        let (browser_tx, _browser_rx) = mpsc::channel(16);

        let stop = session.prompt("hi".to_string(), &updates_tx, &permissions_tx, &browser_tx).await;

        assert_eq!(stop, StopReason::Refusal);
        let update = updates_rx.recv().await.expect("one chunk explaining why");
        match update {
            SessionUpdate::AgentMessageChunk { content: ContentBlock::Text { text }, .. } => {
                assert!(text.contains("No API key set"), "unexpected message: {text}");
            }
            other => panic!("expected an AgentMessageChunk, got {other:?}"),
        }
    }

    /// The whole point of this module in one test: a plain answer, with no
    /// tool calls, becomes exactly one `agent_message_chunk` and an
    /// `EndTurn` stop reason -- no approval machinery involved.
    #[tokio::test]
    async fn a_plain_answer_ends_the_turn_with_no_approvals_needed() {
        let (_dir, ws) = workspace();
        let endpoint = serve_rounds(vec![text_round("Hello!")]).await;
        let config = config_for(&endpoint);
        let mut session =
            HeadlessSession::new(SessionId("s1".to_string()), ws, config);
        let (updates_tx, mut updates_rx) = mpsc::channel(16);
        let (permissions_tx, _permissions_rx) = mpsc::channel(16);
        let (browser_tx, _browser_rx) = mpsc::channel(16);

        let stop = session.prompt("hi".to_string(), &updates_tx, &permissions_tx, &browser_tx).await;

        assert_eq!(stop, StopReason::EndTurn);
        let update = updates_rx.recv().await.expect("one chunk");
        assert!(matches!(update, SessionUpdate::AgentMessageChunk { .. }));
    }

    /// A read (auto-approved, per `verdict_for`) never reaches the
    /// permission channel, and its result is fed back to the model, which
    /// then answers.
    #[tokio::test]
    async fn an_auto_approved_read_runs_without_asking_permission() {
        let (_dir, ws) = workspace();
        let endpoint = serve_rounds(vec![
            tool_call_round(tools::READ_FILE, r#"{"path":"hello.txt"}"#),
            text_round("It says hi."),
        ])
        .await;
        let config = config_for(&endpoint);
        let mut session = HeadlessSession::new(SessionId("s1".to_string()), ws, config);
        let (updates_tx, mut updates_rx) = mpsc::channel(16);
        let (permissions_tx, mut permissions_rx) = mpsc::channel(16);
        let (browser_tx, _browser_rx) = mpsc::channel(16);

        let stop = session
            .prompt("what does hello.txt say?".to_string(), &updates_tx, &permissions_tx, &browser_tx)
            .await;

        assert_eq!(stop, StopReason::EndTurn);
        assert!(permissions_rx.try_recv().is_err(), "a read must never ask permission");
        let mut saw_tool_call = false;
        while let Ok(update) = updates_rx.try_recv() {
            if matches!(update, SessionUpdate::ToolCall(_)) {
                saw_tool_call = true;
            }
        }
        assert!(saw_tool_call, "the auto-approved call should still be reported");
    }

    /// A dangerous command (needs approval under every policy, per
    /// `verdict_for` -- a plain write does not, since writes are
    /// "ordinary" under the default `Destructive` policy) blocks on the
    /// permission channel; refusing it feeds a refusal back to the model
    /// instead of running anything.
    #[tokio::test]
    async fn a_dangerous_command_asks_permission_and_a_refusal_is_honored() {
        let (dir, ws) = workspace();
        let endpoint = serve_rounds(vec![
            tool_call_round(tools::RUN_COMMAND, r#"{"command":"rm -rf build"}"#),
            text_round("Understood, not running that."),
        ])
        .await;
        let config = config_for(&endpoint);
        let mut session = HeadlessSession::new(SessionId("s1".to_string()), ws, config);
        let (updates_tx, _updates_rx) = mpsc::channel(16);
        let (permissions_tx, mut permissions_rx) = mpsc::channel(16);
        let (browser_tx, _browser_rx) = mpsc::channel(16);

        let prompt = tokio::spawn(async move {
            session.prompt("clean the build dir".to_string(), &updates_tx, &permissions_tx, &browser_tx).await
        });

        let ask = permissions_rx.recv().await.expect("a permission ask");
        assert_eq!(ask.request.session_id, SessionId("s1".to_string()));
        let _ = ask.respond.send(RequestPermissionOutcome::Selected {
            option_id: PermissionOptionId("reject".to_string()),
        });

        let stop = prompt.await.expect("task joins");
        assert_eq!(stop, StopReason::EndTurn);
        // rm -rf on a directory that never existed is a no-op either way,
        // so the real proof this was refused (not just answered oddly) is
        // that the model's second round ran at all: refusing must feed a
        // `tool` message back so the transcript stays valid and the model
        // gets to respond, exactly like `App::advance_approvals`'s own
        // refusal path -- if that hadn't happened, `serve_rounds`' second
        // queued response would never have been consumed and this task
        // would hang instead of returning `EndTurn`.
        let _ = dir; // kept for the tempdir's own lifetime, not asserted on
    }

    /// The actual point of wiring `tools::preview_change_text` into
    /// `ask_permission`: a write that needs asking about carries the real
    /// before/after text on the wire, not just a title string -- an ACP
    /// client can show the developer what would change before they approve
    /// it, instead of approving blind.
    #[tokio::test]
    async fn a_pending_write_carries_its_diff_in_the_permission_request() {
        let (dir, ws) = workspace(); // hello.txt already contains "hi\n"
        let endpoint = serve_rounds(vec![
            tool_call_round(tools::WRITE_FILE, r#"{"path":"hello.txt","content":"bye\n"}"#),
            text_round("Updated it."),
        ])
        .await;
        let mut config = config_for(&endpoint);
        // The default policy auto-approves an ordinary write (see
        // `an_auto_approved_read_runs_without_asking_permission`'s own
        // sibling reasoning in app.rs's characterization tests) -- `Always`
        // is what actually reaches the permission-asking path for a plain
        // write, which is the path under test here.
        config.tools.approval = crate::config::ApprovalMode::Always;
        let mut session = HeadlessSession::new(SessionId("s1".to_string()), ws, config);
        let (updates_tx, _updates_rx) = mpsc::channel(16);
        let (permissions_tx, mut permissions_rx) = mpsc::channel(16);
        let (browser_tx, _browser_rx) = mpsc::channel(16);

        let prompt = tokio::spawn(async move {
            session.prompt("say hi in a different way".to_string(), &updates_tx, &permissions_tx, &browser_tx).await
        });

        let ask = permissions_rx.recv().await.expect("a permission ask");
        match ask.request.tool_call.content {
            Some(ToolCallContent::Diff { path, old_text, new_text }) => {
                assert_eq!(path, "hello.txt");
                assert_eq!(old_text.as_deref(), Some("hi\n"));
                assert_eq!(new_text, "bye\n");
            }
            other => panic!("expected a Diff, got {other:?}"),
        }
        let _ = ask.respond.send(RequestPermissionOutcome::Selected {
            option_id: PermissionOptionId("allow".to_string()),
        });

        let stop = prompt.await.expect("task joins");
        assert_eq!(stop, StopReason::EndTurn);
        assert_eq!(std::fs::read_to_string(dir.path().join("hello.txt")).unwrap(), "bye\n");
    }

    /// The actual point of `check_in_browser`: it goes out over the
    /// `browser` channel, not `permissions` -- auto-approved like a read
    /// (see `is_read_only_action`), and fulfilled entirely by whatever the
    /// client sends back, since `tools::execute` has no way to take a
    /// screenshot at all.
    #[tokio::test]
    async fn check_in_browser_asks_the_client_and_reports_what_it_sees() {
        let (_dir, ws) = workspace();
        let endpoint = serve_rounds(vec![
            tool_call_round(tools::CHECK_IN_BROWSER, r#"{"url":"http://localhost:3000"}"#),
            text_round("It renders correctly."),
        ])
        .await;
        let config = config_for(&endpoint);
        let mut session = HeadlessSession::new(SessionId("s1".to_string()), ws, config);
        let (updates_tx, mut updates_rx) = mpsc::channel(16);
        let (permissions_tx, mut permissions_rx) = mpsc::channel(16);
        let (browser_tx, mut browser_rx) = mpsc::channel(16);

        let prompt = tokio::spawn(async move {
            session
                .prompt("does the homepage render?".to_string(), &updates_tx, &permissions_tx, &browser_tx)
                .await
        });

        let ask = browser_rx.recv().await.expect("a browser check ask");
        assert_eq!(ask.session_id, SessionId("s1".to_string()));
        assert_eq!(ask.url, "http://localhost:3000");
        assert!(
            permissions_rx.try_recv().is_err(),
            "check_in_browser must never ask permission -- it's read-only, like a read"
        );
        let _ = ask.respond.send(BrowserCheckResult::Screenshot {
            mime_type: "image/png".to_string(),
            data: "aGVsbG8=".to_string(),
        });

        let stop = prompt.await.expect("task joins");
        assert_eq!(stop, StopReason::EndTurn);

        let mut saw_image = false;
        while let Ok(update) = updates_rx.try_recv() {
            if let SessionUpdate::ToolCallUpdate(ToolCallUpdate {
                content: Some(ToolCallContent::Image { mime_type, data }),
                ..
            }) = update
            {
                assert_eq!(mime_type, "image/png");
                assert_eq!(data, "aGVsbG8=");
                saw_image = true;
            }
        }
        assert!(saw_image, "the screenshot must reach the client as an Image, for the human to see");
    }

    /// The actual point of wiring `Journal` into `HeadlessSession`: a write
    /// the model made this session can be undone through the exact same
    /// mechanism `App`'s own `/rollback` uses, not a parallel
    /// reimplementation that could drift from it.
    #[tokio::test]
    async fn rollback_restores_a_file_the_session_wrote() {
        let (dir, ws) = workspace(); // hello.txt already contains "hi\n"
        let endpoint = serve_rounds(vec![
            tool_call_round(tools::WRITE_FILE, r#"{"path":"hello.txt","content":"bye\n"}"#),
            text_round("Updated it."),
        ])
        .await;
        let config = config_for(&endpoint);
        let mut session = HeadlessSession::new(SessionId("s1".to_string()), ws, config);
        let (updates_tx, _updates_rx) = mpsc::channel(16);
        let (permissions_tx, _permissions_rx) = mpsc::channel(16);
        let (browser_tx, _browser_rx) = mpsc::channel(16);

        // Ordinary write, default policy -- auto-approved, same as
        // `an_auto_approved_read_runs_without_asking_permission`'s own
        // sibling reasoning, just for a write instead of a read.
        let stop = session
            .prompt("say hi in a different way".to_string(), &updates_tx, &permissions_tx, &browser_tx)
            .await;
        assert_eq!(stop, StopReason::EndTurn);
        assert_eq!(std::fs::read_to_string(dir.path().join("hello.txt")).unwrap(), "bye\n");

        let response = session.rollback();
        assert!(response.summary.contains("Rolled back 1 file"), "{}", response.summary);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("hello.txt")).unwrap(),
            "hi\n",
            "rollback must put the file back to what the session found, not leave the edit"
        );

        // The journal clears once acted on -- a second rollback right after
        // must be a genuine no-op, not undo the same write again. Both
        // outcomes' summaries start with the literal words "Rolled back"
        // ("Rolled back nothing -- ..." is the no-op case's own wording),
        // so the real distinguishing check is the file count, not just
        // whether that substring appears at all.
        let second = session.rollback();
        assert_eq!(second.summary, "Rolled back nothing — every file was already as the session found it.");
    }
}
