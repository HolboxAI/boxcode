//! The real stdio wiring for [`crate::headless::HeadlessSession`] -- reads
//! ACP JSON-RPC off stdin, dispatches to sessions, writes responses and
//! `session/update`/`session/request_permission` messages to stdout. This
//! is the last piece between "the turn loop works" (`headless.rs`, tested
//! end to end already) and "you can spawn `boxcode --acp` as a subprocess
//! and talk to it."
//!
//! **Why this is the hard part, precisely:** ACP is bidirectional over one
//! connection. Requests flow both ways -- the client sends `session/prompt`
//! and expects a response; the agent (this process), mid-`session/prompt`,
//! sends its own `session/request_permission` request and needs *its*
//! response, correlated by an id this process assigns, while the same
//! stdin-reading loop keeps running so that response can actually arrive.
//!
//! That means `session/prompt`'s own JSON-RPC response can't be produced
//! synchronously inside the read loop's `handle()` call, even though
//! `HeadlessSession::prompt` (deliberately, per ACP v1) blocks until the
//! whole turn -- including any permission round trip -- is done. If
//! `handle()` awaited that inline, the read loop would be stuck inside one
//! `await` for the entire turn and could never reach the branch that writes
//! the outgoing `session/request_permission` line or the branch that reads
//! the client's answer to it: a real deadlock, not a hypothetical one.
//! `dispatch_prompt_deferred` avoids it by handing the wait for the turn's
//! result to its own spawned task, which reports back through the same
//! `Outgoing` channel [`SessionActor`]'s update-draining task already
//! writes into, instead of blocking `handle`'s own caller.
//!
//! Routing/classification logic is kept separate from real I/O
//! ([`classify`], [`Router`]'s methods) so it's unit-testable without a
//! real stdin/stdout pipe -- only [`run`] itself touches real I/O, and it
//! isn't unit tested for the same reason `main`'s own top-level loop isn't.

use crate::config::Config;
use crate::headless::{HeadlessSession, PermissionAsk};
use crate::protocol::{
    Implementation, InitializeRequest, InitializeResponse, JsonRpcVersion, NewSessionRequest,
    NewSessionResponse, PromptRequest, PromptResponse, RequestId, RequestPermissionOutcome,
    RequestPermissionRequest, RpcError, RpcErrorObject, RpcRequest, RpcResponse, SessionId,
    SessionNotification, PROTOCOL_VERSION,
};
use crate::workspace::Workspace;
use serde_json::json;
use std::collections::HashMap;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

/// One incoming line, classified. ACP's own JSON-RPC envelope doesn't carry
/// an explicit discriminant for this -- a request has `id` *and* `method`,
/// a notification has `method` alone, and a response to one of *our own*
/// outgoing requests has `id` alone (`result` or `error`, never `method`).
/// Classifying by which fields are present, rather than guessing from
/// shape, is what the JSON-RPC 2.0 spec itself says a receiver does.
#[derive(Debug, Clone, PartialEq)]
enum Incoming {
    Request { id: RequestId, method: String, params: serde_json::Value },
    Notification { method: String, params: serde_json::Value },
    Response { id: RequestId, result: serde_json::Value },
    Error { id: RequestId, error: serde_json::Value },
    /// Present but not a well-formed JSON-RPC message of any of the above
    /// shapes -- kept as its own case rather than silently dropped, so a
    /// caller can decide whether to log it.
    Malformed,
}

fn classify(value: &serde_json::Value) -> Incoming {
    let id = value.get("id").cloned();
    let method = value.get("method").and_then(|m| m.as_str()).map(str::to_string);
    match (id, method) {
        (Some(id), Some(method)) => match serde_json::from_value::<RequestId>(id) {
            Ok(id) => Incoming::Request {
                id,
                method,
                params: value.get("params").cloned().unwrap_or(json!({})),
            },
            Err(_) => Incoming::Malformed,
        },
        (None, Some(method)) => {
            Incoming::Notification { method, params: value.get("params").cloned().unwrap_or(json!({})) }
        }
        (Some(id), None) => match serde_json::from_value::<RequestId>(id) {
            Ok(id) => {
                if let Some(result) = value.get("result") {
                    Incoming::Response { id, result: result.clone() }
                } else if let Some(error) = value.get("error") {
                    Incoming::Error { id, error: error.clone() }
                } else {
                    Incoming::Malformed
                }
            }
            Err(_) => Incoming::Malformed,
        },
        (None, None) => Incoming::Malformed,
    }
}

/// A message sent to one session's own task.
enum SessionMsg {
    Prompt { text: String, respond: oneshot::Sender<PromptResponse> },
}

/// Everything a spawned task can produce for the outgoing stdout stream,
/// asynchronously, outside of `Router::handle`'s own synchronous return
/// value -- see this module's own docs for why `session/prompt`'s response
/// has to travel this way rather than being returned directly.
enum Outgoing {
    /// A `session/update` notification, still needing its envelope built.
    Update(SessionNotification),
    /// An already-serialized JSON-RPC line (a deferred request's response),
    /// ready to write as-is.
    Line(String),
}

/// Owns one [`HeadlessSession`] and processes its `session/prompt` calls
/// one at a time on its own spawned task -- see this module's own docs for
/// why that has to be a separate task from the main read loop.
struct SessionActor;

impl SessionActor {
    fn spawn(
        mut session: HeadlessSession,
        outgoing_tx: mpsc::Sender<Outgoing>,
        permission_relay: mpsc::Sender<PermissionAsk>,
        session_id: SessionId,
    ) -> mpsc::Sender<SessionMsg> {
        let (tx, mut rx) = mpsc::channel::<SessionMsg>(8);
        tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                match msg {
                    SessionMsg::Prompt { text, respond } => {
                        // A fresh channel per prompt, not one shared for
                        // the session's whole lifetime: a session answers
                        // many `session/prompt` calls over time, and each
                        // one's own drain task below has to see its own
                        // channel actually close (via the `drop` below) to
                        // know that specific turn is over, rather than the
                        // one from three turns ago.
                        let (updates_tx, mut updates_rx) = mpsc::channel(64);
                        // Drain this prompt's own update channel into the
                        // shared outgoing one, tagged with this session's
                        // id -- HeadlessSession itself doesn't know about
                        // the wire, only about SessionUpdate values (see
                        // headless.rs's own module docs).
                        let session_id = session_id.clone();
                        let outgoing_tx = outgoing_tx.clone();
                        let drain = tokio::spawn(async move {
                            while let Some(update) = updates_rx.recv().await {
                                let _ = outgoing_tx
                                    .send(Outgoing::Update(SessionNotification {
                                        session_id: session_id.clone(),
                                        update,
                                    }))
                                    .await;
                            }
                        });
                        let stop_reason =
                            session.prompt(text, &updates_tx, &permission_relay).await;
                        drop(updates_tx); // let the drain task see the channel close
                        let _ = drain.await;
                        let _ = respond.send(PromptResponse { stop_reason });
                    }
                }
            }
        });
        tx
    }
}

/// Owns everything the read loop needs across the life of the connection:
/// active sessions, and the table correlating a `session/request_permission`
/// response back to the specific pending ask that's waiting for it.
struct Router {
    sessions: HashMap<SessionId, mpsc::Sender<SessionMsg>>,
    pending_permission_responses: HashMap<RequestId, oneshot::Sender<RequestPermissionOutcome>>,
    next_outgoing_id: i64,
    outgoing_tx: mpsc::Sender<Outgoing>,
    permission_relay_in: mpsc::Receiver<PermissionAsk>,
    permission_relay_tx: mpsc::Sender<PermissionAsk>,
    /// The one field genuinely needed at runtime that a unit test doesn't
    /// have to fill in with anything meaningful -- see `run`'s own
    /// construction of the real `Config::load()` value.
    config: Config,
}

impl Router {
    fn new(config: Config, outgoing_tx: mpsc::Sender<Outgoing>) -> Self {
        let (permission_relay_tx, permission_relay_in) = mpsc::channel(64);
        Self {
            sessions: HashMap::new(),
            pending_permission_responses: HashMap::new(),
            next_outgoing_id: 1,
            outgoing_tx,
            permission_relay_in,
            permission_relay_tx,
            config,
        }
    }

    /// Handles one classified incoming line, returning zero or more lines
    /// to write back immediately (a response to a request, or nothing for
    /// a notification/response). `session/prompt` is the one request whose
    /// response never comes back through this return value -- see this
    /// module's own docs -- so it always yields `None` here even though
    /// it's a request.
    async fn handle(&mut self, incoming: Incoming) -> Option<String> {
        match incoming {
            Incoming::Request { id, method, params } if method == "session/prompt" => {
                self.dispatch_prompt_deferred(id, params).await;
                None
            }
            Incoming::Request { id, method, params } => {
                let result = self.dispatch_request(&method, params).await;
                let line = match result {
                    Ok(value) => {
                        serde_json::to_string(&RpcResponse { jsonrpc: JsonRpcVersion, id, result: value })
                    }
                    Err((code, message)) => serde_json::to_string(&RpcError {
                        jsonrpc: JsonRpcVersion,
                        id,
                        error: RpcErrorObject { code, message, data: None },
                    }),
                };
                Some(format!("{}\n", line.expect("response always serializes")))
            }
            Incoming::Notification { method, params } => {
                if method == "session/cancel" {
                    // v1 has no cancellation plumbing into HeadlessSession
                    // yet (see headless.rs's own module docs: no
                    // compaction, no plan mode -- cancellation is the same
                    // class of not-yet-built as those). Accepted and
                    // acknowledged as received, not silently unparsed.
                    let _ = params;
                }
                None
            }
            Incoming::Response { id, result } => {
                if let Some(respond) = self.pending_permission_responses.remove(&id) {
                    if let Ok(outcome) = serde_json::from_value::<RequestPermissionOutcome>(result) {
                        let _ = respond.send(outcome);
                    }
                }
                None
            }
            Incoming::Error { id, .. } => {
                if let Some(respond) = self.pending_permission_responses.remove(&id) {
                    let _ = respond.send(RequestPermissionOutcome::Cancelled);
                }
                None
            }
            Incoming::Malformed => None,
        }
    }

    async fn dispatch_request(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, (i64, String)> {
        match method {
            "initialize" => {
                let _req: InitializeRequest = serde_json::from_value(params)
                    .map_err(|e| (-32602, format!("invalid params: {e}")))?;
                let resp = InitializeResponse {
                    protocol_version: PROTOCOL_VERSION,
                    agent_capabilities: json!({ "session": {} }),
                    auth_methods: Vec::new(),
                    agent_info: Some(Implementation {
                        name: "boxcode".to_string(),
                        title: Some("boxcode".to_string()),
                        version: crate::VERSION.to_string(),
                    }),
                };
                Ok(serde_json::to_value(resp).expect("serializes"))
            }
            "session/new" => {
                let req: NewSessionRequest = serde_json::from_value(params)
                    .map_err(|e| (-32602, format!("invalid params: {e}")))?;
                let workspace = Workspace::new(&req.cwd).map_err(|e| (-32000, e))?;
                let session_id = SessionId(format!("sess_{}", self.sessions.len() + 1));
                let session = HeadlessSession::new(session_id.clone(), workspace, self.config.clone());
                let handle = SessionActor::spawn(
                    session,
                    self.outgoing_tx.clone(),
                    self.permission_relay_tx.clone(),
                    session_id.clone(),
                );
                self.sessions.insert(session_id.clone(), handle);
                Ok(serde_json::to_value(NewSessionResponse { session_id }).expect("serializes"))
            }
            other => Err((-32601, format!("method not found: {other}"))),
        }
    }

    /// `session/prompt`'s response can only be known once the whole turn
    /// finishes -- including, potentially, a full
    /// `session/request_permission` round trip back through this same
    /// connection. Awaiting that inline here (the way every other request
    /// is handled above) would block the read loop from ever reaching the
    /// code that sends that permission request or reads its answer. So
    /// this spawns its own task to wait for the result and report it
    /// through `outgoing_tx` -- the same channel [`SessionActor`]'s update
    /// drain already uses -- instead of returning it from `handle`.
    async fn dispatch_prompt_deferred(&mut self, id: RequestId, params: serde_json::Value) {
        let outgoing_tx = self.outgoing_tx.clone();
        let error_line = |id: RequestId, code: i64, message: String| {
            format!(
                "{}\n",
                serde_json::to_string(&RpcError {
                    jsonrpc: JsonRpcVersion,
                    id,
                    error: RpcErrorObject { code, message, data: None },
                })
                .expect("error response always serializes")
            )
        };

        let req: PromptRequest = match serde_json::from_value(params) {
            Ok(req) => req,
            Err(e) => {
                let line = error_line(id, -32602, format!("invalid params: {e}"));
                let _ = outgoing_tx.send(Outgoing::Line(line)).await;
                return;
            }
        };
        let Some(handle) = self.sessions.get(&req.session_id).cloned() else {
            let line = error_line(id, -32001, "no such session".to_string());
            let _ = outgoing_tx.send(Outgoing::Line(line)).await;
            return;
        };
        let text = req
            .prompt
            .into_iter()
            .map(|block| match block {
                crate::protocol::ContentBlock::Text { text } => text,
            })
            .collect::<Vec<_>>()
            .join("\n");

        let (respond, receive) = oneshot::channel();
        if handle.send(SessionMsg::Prompt { text, respond }).await.is_err() {
            let line = error_line(id, -32002, "session actor gone".to_string());
            let _ = outgoing_tx.send(Outgoing::Line(line)).await;
            return;
        }

        tokio::spawn(async move {
            let line = match receive.await {
                Ok(response) => format!(
                    "{}\n",
                    serde_json::to_string(&RpcResponse {
                        jsonrpc: JsonRpcVersion,
                        id,
                        result: serde_json::to_value(response).expect("serializes"),
                    })
                    .expect("response always serializes")
                ),
                Err(_) => error_line(id, -32002, "session actor gone".to_string()),
            };
            let _ = outgoing_tx.send(Outgoing::Line(line)).await;
        });
    }

    /// Pulls one queued [`PermissionAsk`] (from any session's spawned
    /// task), assigns it a fresh outgoing request id, registers where its
    /// eventual response should go, and returns the line to write to
    /// stdout. `None` means the relay channel closed (every session
    /// finished).
    async fn next_outgoing_permission_request(&mut self) -> Option<String> {
        let ask = self.permission_relay_in.recv().await?;
        let id = RequestId::Number(self.next_outgoing_id);
        self.next_outgoing_id += 1;
        self.pending_permission_responses.insert(id.clone(), ask.respond);
        let request = RpcRequest {
            jsonrpc: JsonRpcVersion,
            id,
            method: "session/request_permission".to_string(),
            params: Some(
                serde_json::to_value(RequestPermissionRequest {
                    session_id: ask.request.session_id,
                    tool_call: ask.request.tool_call,
                    options: ask.request.options,
                })
                .expect("serializes"),
            ),
        };
        Some(request.to_ndjson_line())
    }
}

/// The real entry point -- reads stdin, writes stdout, runs until stdin
/// closes. Selected by `main.rs` via `boxcode --acp`, mutually exclusive
/// with the normal TUI (this never touches a terminal, only pipes).
pub async fn run(config: Config) -> Result<(), Box<dyn std::error::Error>> {
    let stdin = tokio::io::stdin();
    let mut lines = BufReader::new(stdin).lines();
    let mut stdout = tokio::io::stdout();

    let (outgoing_tx, mut outgoing_in) = mpsc::channel::<Outgoing>(256);
    let mut router = Router::new(config, outgoing_tx);

    loop {
        tokio::select! {
            line = lines.next_line() => {
                let Some(line) = line? else { break }; // stdin closed
                if line.trim().is_empty() {
                    continue;
                }
                let value: serde_json::Value = match serde_json::from_str(&line) {
                    Ok(v) => v,
                    Err(_) => continue, // malformed line, not this process's job to crash over
                };
                if let Some(out) = router.handle(classify(&value)).await {
                    stdout.write_all(out.as_bytes()).await?;
                    stdout.flush().await?;
                }
            }
            Some(outgoing) = outgoing_in.recv() => {
                let line = match outgoing {
                    Outgoing::Update(notification) => crate::protocol::RpcNotification {
                        jsonrpc: JsonRpcVersion,
                        method: "session/update".to_string(),
                        params: Some(serde_json::to_value(&notification).expect("serializes")),
                    }
                    .to_ndjson_line(),
                    Outgoing::Line(line) => line,
                };
                stdout.write_all(line.as_bytes()).await?;
                stdout.flush().await?;
            }
            Some(out) = router.next_outgoing_permission_request() => {
                stdout.write_all(out.as_bytes()).await?;
                stdout.flush().await?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_request_needs_both_id_and_method() {
        let value = json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {} });
        assert_eq!(
            classify(&value),
            Incoming::Request {
                id: RequestId::Number(1),
                method: "initialize".to_string(),
                params: json!({})
            }
        );
    }

    #[test]
    fn a_notification_has_method_but_no_id() {
        let value = json!({ "jsonrpc": "2.0", "method": "session/cancel", "params": { "sessionId": "s1" } });
        assert_eq!(
            classify(&value),
            Incoming::Notification {
                method: "session/cancel".to_string(),
                params: json!({ "sessionId": "s1" })
            }
        );
    }

    #[test]
    fn a_response_to_our_own_request_has_id_and_result_but_no_method() {
        let value = json!({ "jsonrpc": "2.0", "id": 7, "result": { "outcome": "cancelled" } });
        assert_eq!(
            classify(&value),
            Incoming::Response { id: RequestId::Number(7), result: json!({ "outcome": "cancelled" }) }
        );
    }

    #[test]
    fn an_error_response_is_distinguished_from_a_result() {
        let value = json!({ "jsonrpc": "2.0", "id": 7, "error": { "code": -32000, "message": "boom" } });
        assert_eq!(
            classify(&value),
            Incoming::Error { id: RequestId::Number(7), error: json!({ "code": -32000, "message": "boom" }) }
        );
    }

    #[test]
    fn a_message_with_neither_id_nor_method_is_malformed_not_guessed_at() {
        assert_eq!(classify(&json!({ "jsonrpc": "2.0" })), Incoming::Malformed);
    }

    /// The actual end-to-end proof: initialize, open a session, send a
    /// prompt that triggers a permission request, answer it, and see the
    /// turn actually complete -- through `Router`, not just through
    /// `HeadlessSession` directly (that's already covered in
    /// `headless.rs`'s own tests; this test is specifically about the
    /// routing/id-correlation logic this module adds on top, and about
    /// `session/prompt` genuinely not blocking the router the way this
    /// module's own docs say it must not).
    #[tokio::test]
    async fn a_full_round_trip_through_the_router_completes_a_turn() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt as _};
        use tokio::net::TcpListener;

        let dir = tempfile::tempdir().expect("temp dir");
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            for response in [
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n\
                 data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\
                 \"type\":\"function\",\"function\":{\"name\":\"run_command\",\"arguments\":\
                 \"{\\\"command\\\":\\\"rm -rf build\\\"}\"}}]}}]}\n\ndata: [DONE]\n\n",
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n\
                 data: {\"choices\":[{\"delta\":{\"content\":\"Understood.\"}}]}\n\ndata: [DONE]\n\n",
            ] {
                let (mut socket, _) = listener.accept().await.expect("accept");
                let mut buf = [0u8; 4096];
                let _ = socket.read(&mut buf).await;
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            }
        });

        let mut config = Config::default();
        config.llm.endpoint = format!("http://{addr}");
        config.llm.model = "test-model".to_string();
        config.llm.api_key = "sk-test".to_string();

        let (outgoing_tx, mut outgoing_in) = mpsc::channel(64);
        let mut router = Router::new(config, outgoing_tx);

        let init = router
            .handle(classify(&json!({
                "jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": { "protocolVersion": 1 }
            })))
            .await;
        assert!(init.unwrap().contains("\"protocolVersion\":1"));

        let new_session = router
            .handle(classify(&json!({
                "jsonrpc": "2.0", "id": 2, "method": "session/new",
                "params": { "cwd": dir.path().display().to_string(), "mcpServers": [] }
            })))
            .await
            .unwrap();
        assert!(new_session.contains("sess_1"));

        // session/prompt must return None immediately -- its own response
        // is deferred, not synchronous -- exactly the property that keeps
        // the router free to handle the permission round trip below on the
        // very same task, with no extra `tokio::spawn` needed on the test's
        // side either. That itself is proof the deadlock this module's own
        // docs describe doesn't happen: if `handle` blocked here instead,
        // this call would never return and the test would time out.
        let prompt_ack = router
            .handle(classify(&json!({
                "jsonrpc": "2.0", "id": 3, "method": "session/prompt",
                "params": { "sessionId": "sess_1", "prompt": [{ "type": "text", "text": "clean the build dir" }] }
            })))
            .await;
        assert_eq!(prompt_ack, None);

        // Drain the permission request the dangerous `rm -rf build` call
        // produces, and answer it as a rejection.
        let permission_line = router
            .next_outgoing_permission_request()
            .await
            .expect("the dangerous command asks permission");
        let permission_value: serde_json::Value =
            serde_json::from_str(permission_line.trim()).expect("valid JSON line");
        assert_eq!(permission_value["method"], "session/request_permission");
        let request_id = permission_value["id"].as_i64().expect("numeric id");
        let reject_option = permission_value["params"]["options"]
            .as_array()
            .expect("options array")
            .iter()
            .find(|opt| opt["optionId"] == "reject")
            .expect("a reject option is offered")["optionId"]
            .as_str()
            .expect("optionId is a string")
            .to_string();

        let response_ack = router
            .handle(classify(&json!({
                "jsonrpc": "2.0", "id": request_id,
                "result": { "outcome": "selected", "optionId": reject_option }
            })))
            .await;
        assert_eq!(response_ack, None, "a response to our own request never produces a line");

        // Now the deferred session/prompt response arrives on the outgoing
        // channel, proving the whole turn -- including the permission round
        // trip that just ran through this same Router -- actually finished.
        // It's not necessarily the very next thing on the channel: ordinary
        // `session/update` notifications (the tool call's pending/failed
        // status, the model's follow-up message chunk) legitimately
        // interleave ahead of it, same as a real client would see.
        let final_line = loop {
            match outgoing_in.recv().await.expect("the deferred response arrives") {
                Outgoing::Line(line) => break line,
                Outgoing::Update(_) => continue,
            }
        };
        let final_value: serde_json::Value =
            serde_json::from_str(final_line.trim()).expect("valid JSON line");
        assert_eq!(final_value["id"], 3);
        assert_eq!(final_value["result"]["stopReason"], "end_turn");
    }
}
