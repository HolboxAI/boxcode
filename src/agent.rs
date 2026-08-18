//! The agent loop's mechanics -- fire a request, ingest what streams back,
//! execute what the user allowed -- extracted from `main.rs`'s event loop.
//! Step two of Phase 3 in `upgrade-plan.md`.
//!
//! Nothing here decides anything. `App` still owns the state machine and every
//! approval still flows through `approval.rs`'s seam; these functions are the
//! *plumbing* between `App`, the LLM transport, and the tool runner, gathered
//! in one module so they stop being woven through the UI loop. They are called
//! from exactly the same three points in `run_app` they always ran at, and
//! behavior is deliberately identical.
//!
//! Phase 4 builds on that seam: [`run_subagent`] is the same
//! fire-ingest-execute cycle folded into one function, run as a *child* loop
//! whose "user" is the parent model and whose tools are the read-only slice
//! that needs no approval. It lives here because this module is where the
//! loop's mechanics live; when step three of Phase 3 makes `AgentLoop` a
//! spawned task, parent and child become the same code with different
//! callers.
//!
//! Why this shape: step three of the phase turns this trio into the body of a
//! spawned `AgentLoop` task -- fire, ingest, execute *is* the loop. Once they
//! live here, that move is a change of caller, not of code. The deployment
//! flow is deliberately absent: it takes over the terminal and converses with
//! the user directly, which makes it part of the UI's world, not the agent's.

use crate::app::{App, AppState};
use crate::config::Config;
use crate::llm::{self, ChatMessage, StreamEvent, ToolCall};
use crate::tools::{self, ToolOutcome};
use crate::workspace::Workspace;
use tokio::sync::mpsc;

/// Fire the request `App` is waiting to send, spawning the streaming task.
/// A no-op unless `app.state` is `Sending` -- the caller does not need to
/// check first.
pub fn fire_request(
    app: &mut App,
    workspace: Option<&Workspace>,
    tx: &mpsc::Sender<(u64, StreamEvent)>,
) {
    if app.state != AppState::Sending {
        return;
    }
    app.request_id += 1;
    let id = app.request_id;
    let endpoint = app.config.llm.endpoint.clone();
    let model = app.config.llm.model.clone();
    let api_key = app.config.llm.api_key.clone();
    let max_tokens = app.config.llm.max_tokens;

    // Withholding the schemas once the budget is spent is what actually
    // stops a runaway loop: the model has nothing left to call, so it
    // answers. Saying "stop" in the prompt alone would only be a request.
    let budget_left = app.tool_steps < app.config.tools.max_steps;
    // Exact counts make the quota real; without them it falls back to the
    // same character estimate `usage.rs` uses.
    let include_usage = app.config.quota.enabled && app.config.quota.include_usage;
    // A `/compact` request reads the conversation and writes a summary
    // of it; it has nothing to run. Withholding the schemas is what
    // makes that true rather than merely asked for -- and a tool call
    // here would be worse than useless, since the history that replaces
    // this one has nowhere to put the result it would be owed.
    let (schemas, system) = match workspace {
        _ if app.compacting => (Vec::new(), None),
        Some(ws) => (
            if budget_left {
                // Whether anything in this workspace has ever been
                // published, not just this session -- reads the same
                // on-disk registry `/pull`'s picker does, so a resumed or
                // relaunched session picks the four gated tools back up
                // immediately rather than waiting for a fresh
                // publish_artifact call. `any_published_under`, not
                // `remembered_id`: a project published as a single file
                // registers under that file's own path, never equal to
                // (only nested under) the directory `Workspace::new`
                // resolves it to.
                let published = crate::artifacts::any_published_under(ws.root());
                tools::schemas(app.mode, app.active_plan.is_some(), app.config.deploy.enabled, published)
            } else {
                Vec::new()
            },
            Some(tools::system_prompt(
                ws,
                &app.config.tools,
                app.tool_steps,
                app.mode,
                app.active_plan.as_ref(),
            )),
        ),
        None => (Vec::new(), None),
    };
    let history = if app.compacting {
        app.compaction_history()
    } else {
        app.history(system.as_deref())
    };
    let tx = tx.clone();

    let handle = tokio::spawn(async move {
        llm::stream_chat(
            llm::Target {
                endpoint: &endpoint,
                model: &model,
                api_key: &api_key,
                max_tokens,
                include_usage,
            },
            history,
            schemas,
            id,
            tx,
        )
        .await;
    });

    app.abort = Some(handle.abort_handle());
    app.state = AppState::Streaming;
}

/// Feed one event from the stream (or the tool runner, which reports on the
/// same channel) into `App`. Events for a request that is no longer current
/// -- cancelled, superseded -- are dropped here, so `App` only ever hears
/// about the turn it is actually in.
pub fn handle_event(app: &mut App, id: u64, event: StreamEvent) {
    if id != app.request_id {
        return; // stale: belongs to a cancelled request
    }
    match event {
        StreamEvent::Token(token) => app.append_token(&token),
        StreamEvent::ToolCalls(calls) => app.request_tools(calls),
        StreamEvent::ToolsFinished(outcomes) => app.finish_tools(outcomes),
        StreamEvent::AgentActivity { call_id, label, rounds } => {
            app.record_subagent_activity(&call_id, label, rounds)
        }
        StreamEvent::Usage(u) => app.record_exact_usage(u),
        StreamEvent::Done => app.finish_stream(),
        StreamEvent::Notice(note) => app.note(note),
        StreamEvent::Error(err) => app.fail_stream(err),
    }
}

/// Run the calls the user allowed, spawned off the event loop. A no-op
/// unless `App` has finished its approvals and has something to run.
///
/// Spawned rather than run inline: a command may take a minute, and doing
/// it on the event loop would freeze the whole UI -- no redraw, no Esc, no
/// way to tell a slow build from a hang. Results come back on the same
/// channel as tokens, so the stale-request-id guard in `handle_event`
/// covers them too.
pub fn execute_approved(
    app: &mut App,
    workspace: Option<&Workspace>,
    tx: &mpsc::Sender<(u64, StreamEvent)>,
) {
    if app.state != AppState::ExecutingTools || app.approved_tools.is_empty() {
        return;
    }
    let calls = std::mem::take(&mut app.approved_tools);
    // The whole config, not just `[tools]`: an `agent` call spawns a child
    // loop, and a loop needs the endpoint. Everything else still sees only
    // the `tools` half it always saw.
    let config = app.config.clone();
    match workspace {
        Some(ws) => {
            let ws = ws.clone();
            let id = app.request_id;
            let tx = tx.clone();
            let handle = tokio::spawn(async move {
                let mut outcomes = Vec::with_capacity(calls.len());
                for call in &calls {
                    // Resolved here rather than in `tools::execute` because a
                    // subagent is made of the pieces this module owns -- a
                    // conversation, a stream, a tool runner -- and because the
                    // runner deliberately never sees the LLM config.
                    outcomes.push(if call.function.name == tools::AGENT {
                        run_subagent(call, &ws, &config, Some((id, &tx))).await
                    } else {
                        tools::execute(call, &ws, &config.tools).await
                    });
                }
                let _ = tx.send((id, StreamEvent::ToolsFinished(outcomes))).await;
            });
            app.abort = Some(handle.abort_handle());
        }
        // Only reachable if a model invents tool calls for a schema it
        // was never sent. Answer them anyway, or the history is left
        // invalid and the next prompt fails instead of this one.
        None => app.fail_stream(
            "The model asked to run a command, but the command tool is not enabled.".to_string(),
        ),
    }
}

/// Run one read-only subagent to completion and return its report as the
/// tool outcome. Phase 4 of `upgrade-plan.md`: a subagent is an agent loop
/// whose "user" is the parent model.
///
/// This is fire-ingest-execute again, but self-contained: the child has no
/// UI, no approval popup, and no need for one -- it is offered nothing but
/// `tools::subagent_schemas()` and every call is re-checked against
/// `tools::subagent_call_allowed` before it runs, so the read-only guarantee
/// is enforced twice and the user is interrupted zero times. The parent's
/// conversation receives only what this returns; the child's transcript
/// lives and dies inside this function, which is the entire point -- research
/// at the cost of one tool result.
///
/// The stream is driven with `select!` in this task rather than spawned, so
/// when the user cancels the turn (aborting the runner task in
/// `execute_approved`), the child's in-flight request is dropped with it --
/// and the same abort kills any command the child had running, via the
/// `kill_on_drop` the command runner already sets. No orphaned stream, no
/// orphaned process, no tokens spent after Esc.
///
/// `progress` is how the child stays visible while it works: one
/// [`StreamEvent::AgentActivity`] per tool call it makes, sent on the same
/// channel as everything else so the stale-id guard covers it too. `None`
/// runs silently -- the loop is not entitled to a UI.
pub async fn run_subagent(
    call: &ToolCall,
    workspace: &Workspace,
    config: &Config,
    progress: Option<(u64, &mpsc::Sender<(u64, StreamEvent)>)>,
) -> ToolOutcome {
    let Some(tools::Action::Agent { task, agent_type }) = tools::describe_action(call) else {
        return ToolOutcome {
            call_id: call.id.clone(),
            display: "⛭ agent — bad arguments".to_string(),
            content: "Error: could not parse the arguments. Pass {\"task\": \"...\"} with a \
                      non-empty task describing what to research."
                .to_string(),
            diff: None,
        };
    };
    if agent_type != "explore" {
        return ToolOutcome {
            call_id: call.id.clone(),
            display: format!("⛭ agent — no such type '{agent_type}'"),
            content: format!(
                "Error: there is no '{agent_type}' subagent. Only 'explore' (read-only \
                 research) exists; omit agent_type to get it, and do any writing yourself."
            ),
            diff: None,
        };
    }

    let max_steps = config.tools.subagent_max_steps;
    let token_budget = config.tools.subagent_token_budget;
    let mut history = vec![
        ChatMessage::text("system", String::new()), // rewritten every round
        ChatMessage::text("user", task.clone()),
    ];
    let mut steps = 0usize;
    let mut spent = 0usize;
    // How many rounds the child gets *after* its budget ran out. One is owed
    // -- the forced "answer now" round -- and a model that answers even that
    // with tool calls gets those refused once, then cut off. Without the hard
    // floor this loop's exit would depend on the model's cooperation.
    let mut overtime = 0usize;

    loop {
        let budget_left = steps < max_steps && spent < token_budget;
        // Telling the prompt the budget is gone (whichever budget it was) is
        // what makes the last round an answer instead of another search.
        history[0] = ChatMessage::text(
            "system",
            tools::subagent_system_prompt(
                workspace,
                if budget_left { steps } else { max_steps },
                max_steps,
            ),
        );
        let schemas = if budget_left { tools::subagent_schemas() } else { Vec::new() };
        let request_chars: usize = history
            .iter()
            .map(|m| m.content.as_deref().map_or(0, str::len))
            .sum();

        let (tx, mut rx) = mpsc::channel(64);
        let stream = llm::stream_chat(
            llm::Target {
                endpoint: &config.llm.endpoint,
                model: &config.llm.model,
                api_key: &config.llm.api_key,
                max_tokens: config.llm.max_tokens,
                include_usage: config.quota.enabled && config.quota.include_usage,
            },
            history.clone(),
            schemas,
            steps as u64,
            tx,
        );
        tokio::pin!(stream);
        let mut stream_finished = false;
        let mut text = String::new();
        let mut calls: Vec<ToolCall> = Vec::new();
        let mut usage: Option<llm::ApiUsage> = None;
        let error = loop {
            tokio::select! {
                _ = &mut stream, if !stream_finished => stream_finished = true,
                received = rx.recv() => match received {
                    Some((_, StreamEvent::Token(t))) => text.push_str(&t),
                    Some((_, StreamEvent::ToolCalls(c))) => calls = c,
                    Some((_, StreamEvent::Usage(u))) => usage = Some(u),
                    // Notices ("answer was truncated") are addressed to a
                    // user, and this loop has none; ToolsFinished and
                    // AgentActivity never arrive here -- the child runs its
                    // tools inline below, and only *sends* activity, upward.
                    Some((
                        _,
                        StreamEvent::Notice(_)
                        | StreamEvent::ToolsFinished(_)
                        | StreamEvent::AgentActivity { .. },
                    )) => {}
                    Some((_, StreamEvent::Done)) | None => break None,
                    Some((_, StreamEvent::Error(e))) => break Some(e),
                },
            }
        };
        if let Some(e) = error {
            record_subagent_usage(spent, config);
            return ToolOutcome {
                call_id: call.id.clone(),
                display: format!("⛭ agent \"{}\" — failed", brief(&task, 40)),
                content: format!(
                    "The subagent failed before finishing: {e}\nDo the research yourself \
                     with read_file/grep_search/glob, or try the agent again."
                ),
                diff: None,
            };
        }

        // Exact counts when the endpoint reports them, the same
        // character-count-over-four estimate `usage.rs` describes when it
        // does not. Counted against the ceiling either way, so "no usage
        // reporting" never means "no budget".
        spent += usage
            .map(|u| u.total())
            .unwrap_or_else(|| (request_chars + text.len()) / 4);

        if calls.is_empty() {
            record_subagent_usage(spent, config);
            let report = text.trim().to_string();
            return ToolOutcome {
                call_id: call.id.clone(),
                display: format!(
                    "⛭ agent \"{}\" — done ({} tool round{}, ~{}k tokens)",
                    brief(&task, 40),
                    steps,
                    if steps == 1 { "" } else { "s" },
                    (spent + 500) / 1000,
                ),
                content: if report.is_empty() {
                    "The subagent finished without producing a report.".to_string()
                } else {
                    report
                },
                diff: None,
            };
        }

        history.push(ChatMessage {
            role: "assistant".to_string(),
            content: (!text.trim().is_empty()).then_some(text),
            tool_calls: calls.clone(),
            tool_call_id: None,
        });
        if !budget_left {
            overtime += 1;
            if overtime > 1 {
                // Refused once already and it asked again: stop paying.
                record_subagent_usage(spent, config);
                return ToolOutcome {
                    call_id: call.id.clone(),
                    display: format!("⛭ agent \"{}\" — budget spent", brief(&task, 40)),
                    content: "The subagent spent its whole budget without writing a report."
                        .to_string(),
                    diff: None,
                };
            }
        }
        steps += 1;
        for c in &calls {
            // Judged before the activity event is sent, so a refused call is
            // *reported* as refused -- a trail that said "📝 write notes.md"
            // about a write that never happened would be a lie in the UI.
            let gate = if !budget_left {
                Err("Your budget is spent. Write your report now, from what you have seen."
                    .to_string())
            } else {
                tools::subagent_call_allowed(c)
            };
            if let Some((id, tx)) = progress {
                let mut label = tools::describe_action(c)
                    .map(|a| a.label())
                    .unwrap_or_else(|| c.function.name.clone());
                if gate.is_err() {
                    label.push_str(" — refused");
                }
                // Best-effort: a full channel or a gone receiver must never
                // stall or fail the child, so the error is dropped.
                let _ = tx
                    .send((
                        id,
                        StreamEvent::AgentActivity {
                            call_id: call.id.clone(),
                            label,
                            rounds: steps,
                        },
                    ))
                    .await;
            }
            // Answered in order, one message per call, even the refused ones
            // -- an unanswered tool call leaves the transcript invalid and
            // fails the *next* request instead of this one.
            let content = match gate {
                Ok(()) => tools::execute(c, workspace, &config.tools).await.content,
                Err(reason) => reason,
            };
            history.push(ChatMessage {
                role: "tool".to_string(),
                content: Some(content),
                tool_calls: Vec::new(),
                tool_call_id: Some(c.id.clone()),
            });
        }
    }
}

/// A child's spend goes in the same ledger as everyone else's, attributed to
/// the same model, so `/usage` stays honest about what a session cost.
/// Per-agent attribution is Phase 5.4; this is the part that cannot wait.
fn record_subagent_usage(spent: usize, config: &Config) {
    crate::usage::record_turn(spent, &config.llm.model);
}

/// First `max` characters with an ellipsis -- for one-line transcript labels,
/// where `tools`' own `clip` would append a multi-line truncation notice.
fn brief(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let kept: String = s.chars().take(max).collect();
        format!("{kept}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn agent_call(args: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "call_agent".to_string(),
            kind: "function".to_string(),
            function: llm::FunctionCall {
                name: tools::AGENT.to_string(),
                arguments: args.to_string(),
            },
        }
    }

    fn workspace_with_hello() -> (tempfile::TempDir, Workspace) {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(dir.path().join("hello.txt"), "one\ntwo\nthree\n").unwrap();
        let ws = Workspace::new(dir.path()).expect("workspace");
        (dir, ws)
    }

    fn sse(body: &str) -> String {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{body}"
        )
    }

    /// One SSE round that asks for a single tool call.
    fn tool_call_round(name: &str, args: &str) -> String {
        let escaped = args.replace('\\', "\\\\").replace('"', "\\\"");
        sse(&format!(
            "data: {{\"choices\":[{{\"delta\":{{\"tool_calls\":[{{\"index\":0,\"id\":\"c1\",\
             \"type\":\"function\",\"function\":{{\"name\":\"{name}\",\"arguments\":\
             \"{escaped}\"}}}}]}}}}]}}\n\ndata: [DONE]\n\n"
        ))
    }

    /// One SSE round that answers in plain text.
    fn text_round(text: &str) -> String {
        sse(&format!(
            "data: {{\"choices\":[{{\"delta\":{{\"content\":\"{text}\"}}}}]}}\n\ndata: [DONE]\n\n"
        ))
    }

    /// Read one full HTTP request (headers + Content-Length body) off a socket.
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

    /// Serve the given responses to successive connections and hand back the
    /// requests that were made, once all responses are spent.
    async fn serve_rounds(responses: Vec<String>) -> (String, tokio::sync::oneshot::Receiver<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().unwrap();
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let mut requests = Vec::new();
            for response in responses {
                let (mut socket, _) = listener.accept().await.expect("accept");
                requests.push(read_request(&mut socket).await);
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            }
            let _ = done_tx.send(requests);
        });
        (format!("http://{addr}"), done_rx)
    }

    fn config_for(endpoint: &str) -> Config {
        let mut config = Config::default();
        config.llm.endpoint = endpoint.to_string();
        config.llm.model = "test-model".to_string();
        config.llm.api_key = "sk-test".to_string();
        config
    }

    /// The whole Phase 4.1 exit criterion in one test: the child reads a file
    /// and only its final report reaches the parent -- as the outcome of one
    /// tool call, with the file's contents nowhere in it.
    #[tokio::test]
    async fn a_subagent_researches_then_reports_only_its_final_message() {
        let (_dir, ws) = workspace_with_hello();
        let (endpoint, requests) = serve_rounds(vec![
            tool_call_round(tools::READ_FILE, r#"{"path":"hello.txt"}"#),
            text_round("hello.txt has three lines."),
        ])
        .await;
        let config = config_for(&endpoint);

        let outcome =
            run_subagent(&agent_call(json!({ "task": "count lines in hello.txt" })), &ws, &config, None)
                .await;

        assert_eq!(outcome.content, "hello.txt has three lines.");
        assert_eq!(outcome.call_id, "call_agent");
        assert!(outcome.display.contains("1 tool round"), "{}", outcome.display);
        let requests = requests.await.expect("both rounds served");
        assert!(
            requests[1].contains("three"),
            "the file's contents went back to the child, not the parent"
        );
        assert!(
            requests[0].contains("read-only research subagent"),
            "the child gets the child's prompt, not the parent's"
        );
    }

    /// A child that asks to write gets the refusal as that call's result and
    /// can still finish -- the read-only guarantee holds without anything
    /// reaching disk or a prompt.
    #[tokio::test]
    async fn a_subagent_write_is_refused_and_the_child_carries_on() {
        let (dir, ws) = workspace_with_hello();
        let (endpoint, requests) = serve_rounds(vec![
            tool_call_round(tools::WRITE_FILE, r#"{"path":"notes.md","content":"hi"}"#),
            text_round("Report without writing."),
        ])
        .await;
        let config = config_for(&endpoint);

        let outcome =
            run_subagent(&agent_call(json!({ "task": "write up findings" })), &ws, &config, None).await;

        assert_eq!(outcome.content, "Report without writing.");
        assert!(!dir.path().join("notes.md").exists(), "nothing may reach disk");
        let requests = requests.await.expect("both rounds served");
        assert!(
            requests[1].contains("read-only research subagent"),
            "the refusal is answered to the model so the transcript stays valid"
        );
    }

    /// Unknown types are refused before any request is made -- with the fix
    /// spelled out, because a bare "no" gets retried.
    #[tokio::test]
    async fn an_unknown_agent_type_is_refused_with_directions() {
        let (_dir, ws) = workspace_with_hello();
        let config = config_for("http://127.0.0.1:9"); // never contacted
        let outcome = run_subagent(
            &agent_call(json!({ "task": "fix the bug", "agent_type": "worker" })),
            &ws,
            &config,
            None,
        )
        .await;
        assert!(outcome.content.contains("'explore'"), "{}", outcome.content);
        assert!(outcome.content.contains("Error"), "{}", outcome.content);
    }

    #[tokio::test]
    async fn unparseable_arguments_are_reported_not_run() {
        let (_dir, ws) = workspace_with_hello();
        let config = config_for("http://127.0.0.1:9"); // never contacted
        let mut call = agent_call(json!({}));
        call.function.arguments = "not json".to_string();
        let outcome = run_subagent(&call, &ws, &config, None).await;
        assert!(outcome.content.contains("task"), "{}", outcome.content);
    }

    /// While it works, the child announces each tool call on the parent's
    /// event channel -- executed ones plainly, refused ones saying so -- and
    /// every event carries the parent call's id and request id, so the
    /// stale-id guard and the trail bookkeeping both work unchanged.
    #[tokio::test]
    async fn a_subagent_reports_each_step_while_it_works() {
        let (_dir, ws) = workspace_with_hello();
        let (endpoint, _requests) = serve_rounds(vec![
            tool_call_round(tools::READ_FILE, r#"{"path":"hello.txt"}"#),
            tool_call_round(tools::WRITE_FILE, r#"{"path":"n.md","content":"x"}"#),
            text_round("Report."),
        ])
        .await;
        let config = config_for(&endpoint);
        let (tx, mut rx) = mpsc::channel(64);

        let outcome = run_subagent(
            &agent_call(json!({ "task": "look around" })),
            &ws,
            &config,
            Some((7, &tx)),
        )
        .await;
        drop(tx); // so the drain below ends instead of waiting forever

        assert_eq!(outcome.content, "Report.");
        let mut activities = Vec::new();
        while let Some((id, event)) = rx.recv().await {
            let StreamEvent::AgentActivity { call_id, label, rounds } = event else {
                panic!("the child must only send activity, got {event:?}");
            };
            assert_eq!(id, 7, "tagged with the parent's request id");
            assert_eq!(call_id, "call_agent", "tagged with the parent call");
            activities.push((label, rounds));
        }
        assert_eq!(activities.len(), 2, "one event per tool call the child made");
        assert!(activities[0].0.contains("read"), "{}", activities[0].0);
        assert_eq!(activities[0].1, 1);
        assert!(activities[1].0.ends_with("— refused"), "{}", activities[1].0);
        assert_eq!(activities[1].1, 2);
    }

    /// With the step budget already spent, the one request made carries the
    /// answer-now prompt and no schemas: the report is forced, not asked for.
    #[tokio::test]
    async fn a_spent_budget_forces_the_report() {
        let (_dir, ws) = workspace_with_hello();
        let (endpoint, requests) = serve_rounds(vec![text_round("Forced report.")]).await;
        let mut config = config_for(&endpoint);
        config.tools.subagent_max_steps = 0;

        let outcome =
            run_subagent(&agent_call(json!({ "task": "anything" })), &ws, &config, None).await;

        assert_eq!(outcome.content, "Forced report.");
        let requests = requests.await.expect("round served");
        assert!(requests[0].contains("Write your report now"), "the prompt demands an answer");
        assert!(!requests[0].contains("\"tools\""), "no schemas: stopping is enforced, not requested");
    }
}
