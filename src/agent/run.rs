//! The agentic loop: ask the model, run what it asks for, hand back the results,
//! repeat until it stops asking for tools.

use super::{workspace_preamble, AgentEvent, AgentSpec, PermissionRequest, RunCtx};
use crate::llm::{self, ChatMessage, ToolCall};
use crate::permission::{self, Decision};
use crate::tools::{self, ToolOutcome};
use tokio::sync::oneshot;

/// Run one prompt to completion.
///
/// `messages` is the conversation so far -- empty on the first prompt, and the
/// list returned by the previous run after that. It is returned alongside the
/// result whether the run succeeded, failed or was cancelled, so the session can
/// always continue from a valid state.
pub async fn run(
    spec: &'static AgentSpec,
    task: String,
    mut messages: Vec<ChatMessage>,
    ctx: RunCtx,
) -> (Result<String, String>, Vec<ChatMessage>) {
    if messages.is_empty() {
        messages.push(ChatMessage::system(format!(
            "{}\n\n{}",
            spec.system_prompt,
            workspace_preamble(&ctx.tools)
        )));
    }
    messages.push(ChatMessage::user(task));

    let defs = tools::defs(spec.tools);

    for _ in 0..ctx.max_iterations {
        if ctx.cancelled() {
            return (Err("Cancelled.".to_string()), messages);
        }

        let turn = llm::stream_turn(
            &ctx.client,
            &ctx.target,
            &messages,
            &defs,
            &ctx.tx,
            |text| {
                (
                    ctx.run_id,
                    AgentEvent::Token {
                        agent: spec.id,
                        text,
                    },
                )
            },
        )
        .await;

        let turn = match turn {
            Ok(turn) => turn,
            // A transport failure leaves `messages` ending on a user or tool
            // message, which is a valid state to retry from.
            Err(e) => return (Err(e), messages),
        };

        let text = turn.text.clone();
        let calls = turn.tool_calls.clone();
        messages.push(turn.into_message());

        if calls.is_empty() {
            return (Ok(text), messages);
        }

        for (i, call) in calls.iter().enumerate() {
            // Every tool_call in the assistant message above must get a reply,
            // or the next request is rejected outright. So a cancellation part
            // way through still answers the calls it is abandoning.
            if ctx.cancelled() {
                for remaining in &calls[i..] {
                    messages.push(ChatMessage::tool_result(
                        &remaining.id,
                        "Cancelled by the user.",
                    ));
                }
                return (Err("Cancelled.".to_string()), messages);
            }

            let outcome = execute(spec, call, &ctx).await;
            ctx.emit(AgentEvent::ToolFinished {
                call_id: call.id.clone(),
                ok: outcome.is_ok(),
                detail: outcome.text().to_string(),
            })
            .await;
            messages.push(ChatMessage::tool_result(&call.id, outcome.text()));
        }
    }

    (
        Err(format!(
            "Stopped after {} tool rounds without finishing. Raise [agent] max_iterations \
             in config.toml, or give a narrower task.",
            ctx.max_iterations
        )),
        messages,
    )
}

/// Resolve one tool call: check it is allowed, get approval if needed, run it.
///
/// Every failure path returns `ToolOutcome::Err`, never a panic and never an
/// abort -- the model reads the message and adjusts.
async fn execute(spec: &AgentSpec, call: &ToolCall, ctx: &RunCtx) -> ToolOutcome {
    let name = call.function.name.as_str();

    let Some(tool_spec) = tools::find(name) else {
        return ToolOutcome::Err(format!(
            "Unknown tool '{name}'. Available: {}",
            spec.tools.join(", ")
        ));
    };
    if !spec.tools.contains(&name) {
        return ToolOutcome::Err(format!(
            "'{name}' is not available to the {} agent. Available: {}",
            spec.label,
            spec.tools.join(", ")
        ));
    }

    let args = match call.parsed_arguments() {
        Ok(args) => args,
        Err(e) => return ToolOutcome::Err(format!("Could not read the arguments to {name}: {e}")),
    };

    let summary = tools::summarize(name, &args);
    ctx.emit(AgentEvent::ToolStarted {
        agent: spec.id,
        call_id: call.id.clone(),
        summary: summary.clone(),
    })
    .await;

    if permission::requires_approval(tool_spec) {
        let grant = permission::grant_key(name, &args);
        let granted = grant
            .as_deref()
            .map(|key| ctx.allowlist.allows(key))
            .unwrap_or(false);

        if !granted {
            match ask(ctx, summary, grant.clone()).await {
                Decision::Deny => {
                    return ToolOutcome::Err(
                        "The user denied this action. Do not retry it -- find another approach, \
                         or ask them what they would prefer."
                            .to_string(),
                    )
                }
                Decision::AllowSession => {
                    if let Some(key) = grant {
                        ctx.allowlist.allow(key);
                    }
                }
                Decision::AllowOnce => {}
            }
        }
    }

    tools::dispatch(name, &args, &ctx.tools).await
}

/// Put the decision to the user and wait. A closed channel or a dropped responder
/// means the app is shutting down, which is a denial.
async fn ask(ctx: &RunCtx, summary: String, grant: Option<String>) -> Decision {
    let (respond, receive) = oneshot::channel();
    let request = PermissionRequest {
        summary,
        grant,
        respond,
    };
    if ctx
        .tx
        .send((ctx.run_id, AgentEvent::NeedsPermission(request)))
        .await
        .is_err()
    {
        return Decision::Deny;
    }
    receive.await.unwrap_or(Decision::Deny)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentSpec;
    use crate::permission::Allowlist;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::mpsc;

    static TEST_AGENT: AgentSpec = AgentSpec {
        id: "coder",
        label: "Coder",
        description: "test",
        system_prompt: "test prompt",
        tools: crate::agent::ALL_TOOLS,
    };

    static READ_ONLY_AGENT: AgentSpec = AgentSpec {
        id: "reader",
        label: "Reader",
        description: "test",
        system_prompt: "test prompt",
        tools: &["read_file"],
    };

    /// An SSE turn that asks for one tool call.
    fn tool_turn(id: &str, name: &str, args: &str) -> String {
        let args = serde_json::to_string(args).unwrap(); // JSON-escape into a string literal
        format!(
            "data: {{\"choices\":[{{\"delta\":{{\"tool_calls\":[{{\"index\":0,\"id\":\"{id}\",\
             \"type\":\"function\",\"function\":{{\"name\":\"{name}\",\"arguments\":{args}}}}}]}}}}]}}\n\n\
             data: [DONE]\n\n"
        )
    }

    /// An SSE turn that is just prose, ending the run.
    fn text_turn(text: &str) -> String {
        let text = serde_json::to_string(text).unwrap();
        format!(
            "data: {{\"choices\":[{{\"delta\":{{\"content\":{text}}}}}]}}\n\ndata: [DONE]\n\n"
        )
    }

    /// Serve `turns` in order, one per request, then refuse further connections.
    /// This is the whole fake endpoint: it lets the loop be tested end to end
    /// without a live model.
    async fn serve(turns: Vec<String>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            for body in turns {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = [0u8; 8192];
                let _ = socket.read(&mut buf).await;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{body}"
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            }
        });
        format!("http://{addr}")
    }

    struct Harness {
        _dir: tempfile::TempDir,
        ctx: RunCtx,
        rx: mpsc::Receiver<(u64, AgentEvent)>,
        cancel: Arc<AtomicBool>,
        workspace: std::path::PathBuf,
    }

    async fn harness(turns: Vec<String>) -> Harness {
        let endpoint = serve(turns).await;
        let (dir, tool_ctx) = tools::test_support::ctx();
        let workspace = tool_ctx.workspace.clone();
        let (tx, rx) = mpsc::channel(256);
        let cancel = Arc::new(AtomicBool::new(false));
        let ctx = RunCtx {
            run_id: 1,
            client: llm::build_client().unwrap(),
            target: llm::Target {
                endpoint,
                model: "test".to_string(),
                api_key: String::new(),
                max_tokens: 1024,
            },
            tools: tool_ctx,
            allowlist: Allowlist::new(),
            max_iterations: 10,
            cancel: cancel.clone(),
            tx,
        };
        Harness {
            _dir: dir,
            ctx,
            rx,
            cancel,
            workspace,
        }
    }

    /// Stand in for the UI: answer every permission request with `decision` and
    /// record everything seen, so a test can assert on what the user would have
    /// been shown.
    fn auto_respond(
        mut rx: mpsc::Receiver<(u64, AgentEvent)>,
        decision: Decision,
    ) -> tokio::task::JoinHandle<Vec<AgentEvent>> {
        tokio::spawn(async move {
            let mut seen = Vec::new();
            while let Some((_, event)) = rx.recv().await {
                if let AgentEvent::NeedsPermission(request) = event {
                    let summary = request.summary.clone();
                    let grant = request.grant.clone();
                    let _ = request.respond.send(decision);
                    seen.push(AgentEvent::NeedsPermission(PermissionRequest {
                        summary,
                        grant,
                        respond: oneshot::channel().0,
                    }));
                } else {
                    seen.push(event);
                }
            }
            seen
        })
    }

    fn summaries(events: &[AgentEvent]) -> Vec<String> {
        events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::ToolStarted { summary, .. } => Some(summary.clone()),
                _ => None,
            })
            .collect()
    }

    /// The whole point of the change: the model asks for a file, the loop reads
    /// it, feeds the contents back, and the model answers from them.
    #[tokio::test]
    async fn a_read_only_call_runs_unattended_and_feeds_its_result_back() {
        let h = harness(vec![
            tool_turn("c1", "read_file", r#"{"path": "hello.txt"}"#),
            text_turn("The file says hi."),
        ])
        .await;
        tools::test_support::write(&h.ctx.tools, "hello.txt", "hi");
        let events = auto_respond(h.rx, Decision::Deny); // must never be consulted

        let (result, messages) = run(&TEST_AGENT, "read it".to_string(), Vec::new(), h.ctx).await;

        assert_eq!(result.unwrap(), "The file says hi.");
        let events = events.await.unwrap();
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, AgentEvent::NeedsPermission(_))),
            "reads must not prompt"
        );
        assert_eq!(summaries(&events), vec!["read_file(hello.txt)"]);

        // system, user, assistant(tool_call), tool result, assistant(text)
        assert_eq!(messages.len(), 5);
        assert_eq!(messages[3].role, "tool");
        assert!(messages[3].text().contains("hi"));
    }

    #[tokio::test]
    async fn a_write_is_gated_and_actually_lands_when_allowed() {
        let h = harness(vec![
            tool_turn(
                "c1",
                "write_file",
                r#"{"path": "new.txt", "content": "written"}"#,
            ),
            text_turn("Done."),
        ])
        .await;
        let workspace = h.workspace.clone();
        let events = auto_respond(h.rx, Decision::AllowOnce);

        let (result, _) = run(&TEST_AGENT, "write it".to_string(), Vec::new(), h.ctx).await;

        assert_eq!(result.unwrap(), "Done.");
        assert_eq!(
            std::fs::read_to_string(workspace.join("new.txt")).unwrap(),
            "written"
        );
        let events = events.await.unwrap();
        assert!(events
            .iter()
            .any(|e| matches!(e, AgentEvent::NeedsPermission(_))));
    }

    /// A denial is information for the model, not a crash: the run continues and
    /// the file is left alone.
    #[tokio::test]
    async fn a_denied_write_is_reported_to_the_model_and_the_run_continues() {
        let h = harness(vec![
            tool_turn(
                "c1",
                "write_file",
                r#"{"path": "new.txt", "content": "written"}"#,
            ),
            text_turn("Understood, I left it alone."),
        ])
        .await;
        let workspace = h.workspace.clone();
        let events = auto_respond(h.rx, Decision::Deny);

        let (result, messages) = run(&TEST_AGENT, "write it".to_string(), Vec::new(), h.ctx).await;

        assert_eq!(result.unwrap(), "Understood, I left it alone.");
        assert!(!workspace.join("new.txt").exists(), "the write must not happen");

        let tool_reply = messages.iter().find(|m| m.role == "tool").unwrap();
        assert!(tool_reply.text().contains("denied"), "{}", tool_reply.text());

        let events = events.await.unwrap();
        assert!(events
            .iter()
            .any(|e| matches!(e, AgentEvent::ToolFinished { ok: false, .. })));
    }

    /// "Allow for session" has to actually stop the second prompt.
    #[tokio::test]
    async fn a_session_grant_stops_the_next_prompt_for_the_same_tool() {
        let h = harness(vec![
            tool_turn("c1", "write_file", r#"{"path": "a.txt", "content": "1"}"#),
            tool_turn("c2", "write_file", r#"{"path": "b.txt", "content": "2"}"#),
            text_turn("Both written."),
        ])
        .await;
        let workspace = h.workspace.clone();
        let events = auto_respond(h.rx, Decision::AllowSession);

        let (result, _) = run(&TEST_AGENT, "write both".to_string(), Vec::new(), h.ctx).await;

        assert_eq!(result.unwrap(), "Both written.");
        assert!(workspace.join("a.txt").exists());
        assert!(workspace.join("b.txt").exists());

        let prompts = events
            .await
            .unwrap()
            .iter()
            .filter(|e| matches!(e, AgentEvent::NeedsPermission(_)))
            .count();
        assert_eq!(prompts, 1, "the second write should not have prompted");
    }

    #[tokio::test]
    async fn a_tool_outside_the_agents_list_is_refused_without_running() {
        let h = harness(vec![
            tool_turn("c1", "write_file", r#"{"path": "a.txt", "content": "x"}"#),
            text_turn("I cannot write."),
        ])
        .await;
        let workspace = h.workspace.clone();
        let events = auto_respond(h.rx, Decision::AllowOnce);

        let (result, messages) =
            run(&READ_ONLY_AGENT, "write it".to_string(), Vec::new(), h.ctx).await;

        assert!(result.is_ok());
        assert!(!workspace.join("a.txt").exists());
        let tool_reply = messages.iter().find(|m| m.role == "tool").unwrap();
        assert!(
            tool_reply.text().contains("not available"),
            "{}",
            tool_reply.text()
        );
        // Refused before the gate: an agent that may not write is never a
        // question to put to the user.
        assert!(!events
            .await
            .unwrap()
            .iter()
            .any(|e| matches!(e, AgentEvent::NeedsPermission(_))));
    }

    #[tokio::test]
    async fn malformed_tool_arguments_come_back_as_a_readable_error() {
        let h = harness(vec![
            // Truncated JSON, the shape a cut-off turn produces.
            format!(
                "data: {{\"choices\":[{{\"delta\":{{\"tool_calls\":[{{\"index\":0,\"id\":\"c1\",\
                 \"function\":{{\"name\":\"read_file\",\"arguments\":\"{{\\\"path\\\":\"}}}}]}}}}]}}\n\n\
                 data: [DONE]\n\n"
            ),
            text_turn("Let me try again."),
        ])
        .await;
        let events = auto_respond(h.rx, Decision::AllowOnce);

        let (result, messages) = run(&TEST_AGENT, "read it".to_string(), Vec::new(), h.ctx).await;

        assert!(result.is_ok());
        let tool_reply = messages.iter().find(|m| m.role == "tool").unwrap();
        assert!(
            tool_reply.text().contains("not valid JSON"),
            "{}",
            tool_reply.text()
        );
        drop(events);
    }

    /// A model that never stops calling tools must not loop forever.
    #[tokio::test]
    async fn the_iteration_cap_ends_a_runaway_loop() {
        let turns = vec![tool_turn("c1", "list_dir", r#"{"path": "."}"#); 12];
        let mut h = harness(turns).await;
        h.ctx.max_iterations = 3;
        let events = auto_respond(h.rx, Decision::AllowOnce);

        let (result, messages) = run(&TEST_AGENT, "loop".to_string(), Vec::new(), h.ctx).await;

        let error = result.unwrap_err();
        assert!(error.contains("max_iterations"), "{error}");
        // Three rounds: system + user + 3 x (assistant + tool result).
        assert_eq!(messages.len(), 8);
        drop(events);
    }

    /// Cancelling mid-run must still answer every outstanding tool call, or the
    /// next request to the endpoint is rejected as malformed.
    #[tokio::test]
    async fn cancelling_leaves_the_conversation_valid_to_continue_from() {
        let h = harness(vec![tool_turn("c1", "read_file", r#"{"path": "a.txt"}"#)]).await;
        tools::test_support::write(&h.ctx.tools, "a.txt", "x");
        let cancel = h.cancel.clone();
        let events = auto_respond(h.rx, Decision::AllowOnce);

        // Cancel before the loop reaches the tool call.
        cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        let (result, messages) = run(&TEST_AGENT, "read it".to_string(), Vec::new(), h.ctx).await;

        assert!(result.unwrap_err().contains("Cancelled"));
        for (i, message) in messages.iter().enumerate() {
            if !message.tool_calls.is_empty() {
                let replies = messages[i + 1..]
                    .iter()
                    .filter(|m| m.role == "tool")
                    .count();
                assert_eq!(
                    replies,
                    message.tool_calls.len(),
                    "every tool_call needs a reply"
                );
            }
        }
        drop(events);
    }

    #[tokio::test]
    async fn a_transport_failure_is_returned_without_losing_the_conversation() {
        let mut h = harness(Vec::new()).await;
        h.ctx.target.endpoint = "http://127.0.0.1:1".to_string();
        let events = auto_respond(h.rx, Decision::AllowOnce);

        let (result, messages) = run(&TEST_AGENT, "hello".to_string(), Vec::new(), h.ctx).await;

        assert!(result.unwrap_err().contains("Could not reach"));
        assert_eq!(messages.len(), 2, "system + user survive for a retry");
        assert_eq!(messages[1].text(), "hello");
        drop(events);
    }

    /// The second prompt in a session must not re-send the system prompt.
    #[tokio::test]
    async fn a_follow_up_prompt_continues_the_existing_conversation() {
        let h = harness(vec![text_turn("Second answer.")]).await;
        let events = auto_respond(h.rx, Decision::AllowOnce);
        let existing = vec![
            ChatMessage::system("prompt"),
            ChatMessage::user("first"),
            ChatMessage::assistant("First answer.", Vec::new()),
        ];

        let (result, messages) =
            run(&TEST_AGENT, "second".to_string(), existing, h.ctx).await;

        assert_eq!(result.unwrap(), "Second answer.");
        assert_eq!(messages.len(), 5);
        assert_eq!(
            messages.iter().filter(|m| m.role == "system").count(),
            1,
            "the system prompt must not be repeated"
        );
        drop(events);
    }

    #[tokio::test]
    async fn prose_streams_out_as_tokens_while_the_turn_runs() {
        let h = harness(vec![text_turn("streaming")]).await;
        let events = auto_respond(h.rx, Decision::AllowOnce);

        let (result, _) = run(&TEST_AGENT, "hi".to_string(), Vec::new(), h.ctx).await;
        assert!(result.is_ok());

        let tokens: String = events
            .await
            .unwrap()
            .iter()
            .filter_map(|e| match e {
                AgentEvent::Token { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(tokens, "streaming");
    }
}
