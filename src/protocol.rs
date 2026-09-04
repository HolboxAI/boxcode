//! The wire protocol between `boxcode` and an external client (e.g.
//! `boxcode-ide`) -- JSON-RPC 2.0 over NDJSON on stdio, one JSON object per
//! line. Part of P0 (see `boxcode-ide`'s `docs/PLAN.md`, and `agent.rs`'s own
//! doc comment: "step three of Phase 3 ... turns this trio into the body of
//! a spawned `AgentLoop` task").
//!
//! Scope of this module, deliberately: the wire *types* and their
//! conversions from the internal [`crate::llm::StreamEvent`] /
//! [`crate::approval::ApprovalRequest`] / [`crate::approval::Decision`] types
//! that already exist and are already shaped correctly for this -- see
//! `approval.rs`'s own doc comment, written before this module existed:
//! "When the agent loop becomes its own task, these become the messages on
//! the channel between it and the UI."
//!
//! What is deliberately NOT here: the headless state machine that decides
//! *when* to emit a [`NotificationParams::ApprovalRequest`] or advance a
//! turn. That logic
//! currently lives inside `App` (`advance_approvals`, `finish_stream`,
//! `finish_tools` in `app.rs`) and extracting it is real, separate,
//! higher-risk work -- `App` does not just react to agent events, it decides
//! things (whether to compact, whether a response leaked tool-call markup as
//! prose, when a turn is really over). This module exists so that
//! extraction has a stable, tested wire contract to land on, and so a
//! `boxcode-ide` client can be written against something real in the
//! meantime.
//!
//! Wire types are kept deliberately separate from the internal types they
//! mirror rather than `#[derive(Serialize)]`-ing those types directly: the
//! internal types stay free to change shape for the TUI's own needs without
//! silently breaking the wire contract, and the wire contract stays free to
//! evolve (rename a field for external clarity, add one) without touching
//! internal code. The `From` conversions below are the only place the two
//! sides are allowed to know about each other.
//!
//! One deliberate omission, matching an existing product decision rather
//! than an oversight: [`crate::llm::StreamEvent::Reasoning`] carries the
//! model's raw chain-of-thought text, and `App::append_reasoning`'s own doc
//! comment is explicit that this "is never shown, persisted, replayed, or
//! sent back on the wire." This module keeps that guarantee -- reasoning
//! becomes [`NotificationParams::Thinking`], a bare signal with no content,
//! mirroring what `App::is_thinking()` already exposes to the TUI.

use crate::approval::{ApprovalRequest, Decision};
use crate::diff::FileDiff;
use crate::llm::{ApiUsage, StreamEvent, ToolCall};
use crate::tools::ToolOutcome;
use serde::{Deserialize, Serialize};

/// One JSON-RPC 2.0 notification -- no `id`, no response expected. Every
/// [`StreamEvent`] this process ever emits (bar `Reasoning`, see module
/// docs) becomes one of these, tagged with the turn's request id so a
/// client juggling more than one in-flight turn -- there is at most one
/// today, but the wire contract should not assume that stays true --
/// can tell them apart the same way `agent::handle_event`'s stale-id guard
/// already does internally.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Notification {
    pub jsonrpc: JsonRpcVersion,
    pub method: NotificationMethod,
    pub params: NotificationParams,
}

/// A request the client sends: to start a turn, answer a pending approval,
/// or cancel. Carries an `id` because a well-behaved client may want to
/// correlate it with whatever local action prompted it, even though this
/// protocol answers requests via notifications on the turn's request id
/// rather than a matching JSON-RPC response -- streaming output has no
/// single "the response," so forcing one would mean modeling a whole turn
/// as one giant reply instead of the sequence of notifications it actually
/// is.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Request {
    pub jsonrpc: JsonRpcVersion,
    pub id: u64,
    pub method: RequestMethod,
    pub params: RequestParams,
}

/// Always `"2.0"`. A distinct unit-like type rather than a bare `String` so
/// a malformed envelope (wrong or missing version) fails to deserialize
/// instead of silently parsing with a wrong value sitting in a string field
/// nobody checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JsonRpcVersion;

impl Serialize for JsonRpcVersion {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str("2.0")
    }
}

impl<'de> Deserialize<'de> for JsonRpcVersion {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        if s == "2.0" {
            Ok(JsonRpcVersion)
        } else {
            Err(serde::de::Error::custom(format!(
                "unsupported jsonrpc version {s:?}, only \"2.0\" is understood"
            )))
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotificationMethod {
    #[serde(rename = "stream/token")]
    Token,
    /// See module docs: carries no reasoning text, only that the model is
    /// (or has stopped) thinking -- mirrors `App::is_thinking()`.
    #[serde(rename = "stream/thinking")]
    Thinking,
    /// One or more calls the model wants to make, each needing the user's
    /// decision -- the wire form of [`ApprovalRequest`], including its
    /// precomputed diff preview.
    #[serde(rename = "stream/approvalRequest")]
    ApprovalRequest,
    #[serde(rename = "stream/toolsFinished")]
    ToolsFinished,
    #[serde(rename = "stream/agentActivity")]
    AgentActivity,
    #[serde(rename = "stream/usage")]
    Usage,
    #[serde(rename = "stream/done")]
    Done,
    #[serde(rename = "stream/notice")]
    Notice,
    #[serde(rename = "stream/error")]
    Error,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestMethod {
    #[serde(rename = "turn/send")]
    TurnSend,
    #[serde(rename = "approval/decide")]
    ApprovalDecide,
    #[serde(rename = "turn/cancel")]
    TurnCancel,
}

/// Params for every [`NotificationMethod`] variant, tagged by an internal
/// `kind` field so `{ "method": "stream/token", "params": { ... } }` still
/// round-trips even though `method` and the shape of `params` are
/// correlated -- JSON-RPC does not let a decoder pick `params`'s type from
/// the sibling `method` field automatically, so this carries its own tag
/// rather than relying on the caller to have read `method` first.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "kind")]
pub enum NotificationParams {
    Token {
        request_id: u64,
        token: String,
    },
    Thinking {
        request_id: u64,
        thinking: bool,
    },
    ApprovalRequest {
        request_id: u64,
        request: ApprovalRequestDto,
    },
    ToolsFinished {
        request_id: u64,
        outcomes: Vec<ToolOutcomeDto>,
    },
    AgentActivity {
        request_id: u64,
        call_id: String,
        label: String,
        rounds: usize,
    },
    Usage {
        request_id: u64,
        usage: ApiUsage,
    },
    Done {
        request_id: u64,
    },
    Notice {
        request_id: u64,
        note: String,
    },
    Error {
        request_id: u64,
        error: String,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "kind")]
pub enum RequestParams {
    /// The one field a headless client controls today: the user's message.
    /// Everything `agent::fire_request` also needs -- workspace, config,
    /// history -- is server-side state, not something a wire request
    /// carries; a client sends what a person typed, not how to build a
    /// prompt.
    TurnSend {
        message: String,
    },
    ApprovalDecide {
        request_id: u64,
        call_id: String,
        decision: DecisionDto,
    },
    TurnCancel {
        request_id: u64,
    },
}

/// The wire form of [`ToolCall`] plus [`crate::tools::Action`]'s rendered
/// label and, when there is one, the diff the popup would draw -- everything
/// [`ApprovalRequest`] already carries, none of it re-derived. `Action`
/// itself is deliberately not mirrored variant-for-variant here: it is a
/// large, still-growing enum whose only externally-relevant fact today is
/// the one line it renders as (`Action::label`) and, for a `Write`/`Edit`,
/// the diff already computed alongside it. A client that needs to
/// special-case a specific action kind rather than just display the label
/// does not exist yet; extend this DTO with a real field when one does,
/// rather than speculatively mirroring an enum with call sites that would
/// go untested here.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct ApprovalRequestDto {
    pub call: ToolCall,
    /// `Action::label()` -- e.g. `"write src/App.tsx"`. See the struct docs
    /// for why this is a label, not the full `Action`.
    pub action_label: String,
    pub remaining: usize,
    pub preview: Option<FileDiff>,
}

impl From<&ApprovalRequest> for ApprovalRequestDto {
    fn from(req: &ApprovalRequest) -> Self {
        Self {
            call: req.call.clone(),
            action_label: req.action.label(),
            remaining: req.remaining,
            preview: req.preview.clone(),
        }
    }
}

/// The wire form of [`ToolOutcome`]. `rollback` is deliberately dropped --
/// it is local undo-journal bookkeeping (see `ToolOutcome::rollback`'s own
/// docs: "not sent to the model"), meaningful only to the process holding
/// the workspace on disk, never to a remote client.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct ToolOutcomeDto {
    pub call_id: String,
    pub display: String,
    pub content: String,
    pub diff: Option<FileDiff>,
}

impl From<&ToolOutcome> for ToolOutcomeDto {
    fn from(outcome: &ToolOutcome) -> Self {
        Self {
            call_id: outcome.call_id.clone(),
            display: outcome.display.clone(),
            content: outcome.content.clone(),
            diff: outcome.diff.clone(),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecisionDto {
    Allowed,
    Refused,
}

impl From<DecisionDto> for Decision {
    fn from(dto: DecisionDto) -> Self {
        match dto {
            DecisionDto::Allowed => Decision::Allowed,
            DecisionDto::Refused => Decision::Refused,
        }
    }
}

impl From<Decision> for DecisionDto {
    fn from(decision: Decision) -> Self {
        match decision {
            Decision::Allowed => DecisionDto::Allowed,
            Decision::Refused => DecisionDto::Refused,
        }
    }
}

impl Notification {
    /// Build every [`Notification`] this process can send from one
    /// [`StreamEvent`], tagged with the turn's request id -- the direct wire
    /// counterpart of `agent::handle_event`'s match, minus the one variant
    /// [`StreamEvent::Reasoning`] handles specially (see module docs) and
    /// [`StreamEvent::ToolCalls`], which cannot become a
    /// [`NotificationMethod::ApprovalRequest`] here: building an
    /// [`ApprovalRequestDto`] needs the interpreted [`crate::tools::Action`]
    /// and precomputed diff that only `App::advance_approvals` currently
    /// derives (danger-checking, plan-mode filtering, `[tools] approval`
    /// policy all happen there first) -- exactly the state-machine logic
    /// this module's docs say is out of scope for this slice. A caller with
    /// an already-built [`ApprovalRequest`] should reach for
    /// [`Notification::approval_request`] directly instead. `ToolCalls`'s
    /// `bool` (`finish_reason == "length"`, added after this module's first
    /// draft -- see `App::request_tools_truncated`) isn't dropped silently:
    /// there is simply nowhere on the wire for it to go until that same
    /// state-machine extraction happens, since a headless client needs to
    /// know a write/edit was truncated at exactly the point it would decide
    /// whether to trust it -- i.e. as part of the eventual approval-request
    /// notification, not a separate one.
    pub fn from_stream_event(request_id: u64, event: &StreamEvent) -> Option<Self> {
        let params = match event {
            StreamEvent::Token(token) => NotificationParams::Token {
                request_id,
                token: token.clone(),
            },
            StreamEvent::Reasoning(_) => NotificationParams::Thinking {
                request_id,
                thinking: true,
            },
            StreamEvent::ToolCalls(_, _) => return None,
            StreamEvent::ToolsFinished(outcomes) => NotificationParams::ToolsFinished {
                request_id,
                outcomes: outcomes.iter().map(ToolOutcomeDto::from).collect(),
            },
            StreamEvent::AgentActivity {
                call_id,
                label,
                rounds,
            } => NotificationParams::AgentActivity {
                request_id,
                call_id: call_id.clone(),
                label: label.clone(),
                rounds: *rounds,
            },
            StreamEvent::Usage(usage) => NotificationParams::Usage {
                request_id,
                usage: *usage,
            },
            StreamEvent::Done => NotificationParams::Done { request_id },
            StreamEvent::Notice(note) => NotificationParams::Notice {
                request_id,
                note: note.clone(),
            },
            StreamEvent::Error(error) => NotificationParams::Error {
                request_id,
                error: error.clone(),
            },
        };
        Some(Self::from_params(params))
    }

    /// The one notification [`Notification::from_stream_event`] cannot
    /// build on its own -- see that method's docs.
    pub fn approval_request(request_id: u64, request: &ApprovalRequest) -> Self {
        Self::from_params(NotificationParams::ApprovalRequest {
            request_id,
            request: ApprovalRequestDto::from(request),
        })
    }

    fn from_params(params: NotificationParams) -> Self {
        let method = match &params {
            NotificationParams::Token { .. } => NotificationMethod::Token,
            NotificationParams::Thinking { .. } => NotificationMethod::Thinking,
            NotificationParams::ApprovalRequest { .. } => NotificationMethod::ApprovalRequest,
            NotificationParams::ToolsFinished { .. } => NotificationMethod::ToolsFinished,
            NotificationParams::AgentActivity { .. } => NotificationMethod::AgentActivity,
            NotificationParams::Usage { .. } => NotificationMethod::Usage,
            NotificationParams::Done { .. } => NotificationMethod::Done,
            NotificationParams::Notice { .. } => NotificationMethod::Notice,
            NotificationParams::Error { .. } => NotificationMethod::Error,
        };
        Self {
            jsonrpc: JsonRpcVersion,
            method,
            params,
        }
    }

    /// One line of NDJSON, `\n`-terminated, ready to write to a stdout pipe
    /// -- the same framing `codexAppServerClient.ts` (in the Code-OSS source
    /// `boxcode-ide` forks) uses on its side of an equivalent bridge.
    pub fn to_ndjson_line(&self) -> String {
        format!(
            "{}\n",
            serde_json::to_string(self).expect("Notification always serializes")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::FunctionCall;
    use crate::tools::Action;
    use serde_json::json;

    fn call(name: &str) -> ToolCall {
        ToolCall {
            id: "call_1".to_string(),
            kind: "function".to_string(),
            function: FunctionCall {
                name: name.to_string(),
                arguments: "{}".to_string(),
            },
        }
    }

    /// The schema is a contract with a separate TypeScript client that does
    /// not exist yet -- pinned against literal JSON, not just "it
    /// round-trips", so a future refactor here has to notice it changed the
    /// wire shape rather than only keeping Rust's own serialize/deserialize
    /// symmetric with itself.
    #[test]
    fn a_token_event_becomes_the_documented_wire_shape() {
        let notification =
            Notification::from_stream_event(7, &StreamEvent::Token("hello".to_string())).unwrap();
        let value: serde_json::Value =
            serde_json::from_str(&notification.to_ndjson_line()).unwrap();
        assert_eq!(
            value,
            json!({
                "jsonrpc": "2.0",
                "method": "stream/token",
                "params": { "kind": "Token", "request_id": 7, "token": "hello" }
            })
        );
    }

    #[test]
    fn reasoning_carries_no_text_onto_the_wire() {
        let notification = Notification::from_stream_event(
            1,
            &StreamEvent::Reasoning("the model's private chain of thought".to_string()),
        )
        .unwrap();
        let line = notification.to_ndjson_line();
        assert!(!line.contains("private chain of thought"));
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(
            value,
            json!({
                "jsonrpc": "2.0",
                "method": "stream/thinking",
                "params": { "kind": "Thinking", "request_id": 1, "thinking": true }
            })
        );
    }

    #[test]
    fn tool_calls_alone_produce_no_notification_truncated_or_not() {
        assert!(Notification::from_stream_event(
            1,
            &StreamEvent::ToolCalls(vec![call("write_file")], false)
        )
        .is_none());
        assert!(Notification::from_stream_event(
            1,
            &StreamEvent::ToolCalls(vec![call("write_file")], true)
        )
        .is_none());
    }

    #[test]
    fn an_approval_request_carries_the_label_and_preview_not_the_raw_action() {
        let request = ApprovalRequest {
            call: call("write_file"),
            action: Action::Write {
                path: "src/App.tsx".to_string(),
                content: "x".to_string(),
            },
            remaining: 2,
            preview: None,
        };
        let notification = Notification::approval_request(3, &request);
        let value: serde_json::Value =
            serde_json::from_str(&notification.to_ndjson_line()).unwrap();
        assert_eq!(value["method"], "stream/approvalRequest");
        assert_eq!(
            value["params"]["request"]["action_label"],
            request.action.label()
        );
        assert_eq!(value["params"]["request"]["remaining"], 2);
        assert!(value["params"]["request"]["preview"].is_null());
        // The raw Action enum -- with its own field names, its own shape --
        // never appears on the wire, only its rendered label.
        assert!(!notification.to_ndjson_line().contains("App.tsx\":\"x\""));
    }

    #[test]
    fn a_decision_round_trips_through_its_dto() {
        assert_eq!(
            Decision::from(DecisionDto::from(Decision::Allowed)),
            Decision::Allowed
        );
        assert_eq!(
            Decision::from(DecisionDto::from(Decision::Refused)),
            Decision::Refused
        );
    }

    #[test]
    fn a_turn_send_request_parses_from_the_documented_wire_shape() {
        let line = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "turn/send",
            "params": { "kind": "TurnSend", "message": "start a new webapp project" }
        })
        .to_string();
        let request: Request = serde_json::from_str(&line).unwrap();
        assert_eq!(request.method, RequestMethod::TurnSend);
        match request.params {
            RequestParams::TurnSend { message } => {
                assert_eq!(message, "start a new webapp project")
            }
            other => panic!("expected TurnSend, got {other:?}"),
        }
    }

    #[test]
    fn an_unsupported_jsonrpc_version_is_rejected_not_silently_accepted() {
        let line = json!({
            "jsonrpc": "1.0",
            "id": 1,
            "method": "turn/send",
            "params": { "kind": "TurnSend", "message": "x" }
        })
        .to_string();
        assert!(serde_json::from_str::<Request>(&line).is_err());
    }
}
