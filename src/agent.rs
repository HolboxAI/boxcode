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
//! Why this shape: step three of the phase turns this trio into the body of a
//! spawned `AgentLoop` task -- fire, ingest, execute *is* the loop. Once they
//! live here, that move is a change of caller, not of code. The deployment
//! flow is deliberately absent: it takes over the terminal and converses with
//! the user directly, which makes it part of the UI's world, not the agent's.

use crate::app::{App, AppState};
use crate::llm::{self, StreamEvent};
use crate::tools;
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
    let tools_config = app.config.tools.clone();
    match workspace {
        Some(ws) => {
            let ws = ws.clone();
            let id = app.request_id;
            let tx = tx.clone();
            let handle = tokio::spawn(async move {
                let mut outcomes = Vec::with_capacity(calls.len());
                for call in &calls {
                    outcomes.push(tools::execute(call, &ws, &tools_config).await);
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
