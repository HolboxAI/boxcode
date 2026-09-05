//! The wire protocol between `boxcode` and an external client (e.g.
//! `boxcode-ide`) -- the Agent Client Protocol (ACP), v1, over JSON-RPC 2.0
//! on stdio. Part of P0 (see `boxcode-ide`'s `docs/PLAN.md`, and `agent.rs`'s
//! own doc comment: "step three of Phase 3 ... turns this trio into the body
//! of a spawned `AgentLoop` task").
//!
//! This is a rewrite of an earlier, bespoke JSON-RPC schema this module used
//! to define. That schema had zero callers (confirmed by grep before this
//! rewrite) and was never wired to a real client, so replacing it here cost
//! nothing in practice -- and the real industry convergence, confirmed by
//! research before writing a line of this, is on ACP specifically: Cursor,
//! Kiro (who published a real engineering account of building exactly this
//! kind of shared-core-many-frontends architecture and choosing ACP for the
//! client/agent boundary), Codex, and Gemini CLI are all registered ACP
//! agents. A bespoke schema only ever works for `boxcode-ide`; ACP works
//! with Zed, JetBrains, Neovim and Emacs too, for the same backend work.
//!
//! **Version: v1, deliberately, not v2.** Confirmed by reading both schemas
//! (`agentclientprotocol/agent-client-protocol`, `schema/v1/schema.json` and
//! `schema/v2/schema.json`) plus the repo's own `CHANGELOG.md` and
//! `Cargo.toml` feature gates: v1 ships as `agent-client-protocol-schema`
//! 1.7.0; v2 is `2.0.0-alpha.3`, gated behind an explicitly-opt-in
//! `unstable_protocol_v2` Cargo feature the crate's own comment describes as
//! "intentionally NOT part of the `unstable` umbrella" because it's a
//! different wire version. v1 is what real clients speak today.
//!
//! **`fs/*` and `terminal/*` client methods are deliberately not
//! implemented.** They exist in v1 (an agent can ask the client to read/
//! write files or run commands in a client-managed terminal), but are
//! optional -- `ClientCapabilities.fs`/`.terminal` default to `false`/absent
//! capabilities, and an agent that never requests them is fully spec-
//! compliant. `boxcode` already executes its own tools locally
//! (`tools::execute`), which is the same model v2 standardized on for
//! everyone (v2 deleted the client-delegated fs/terminal surface entirely,
//! per that schema's own migration notes) -- so there is nothing to gain by
//! implementing v1's optional version of it.
//!
//! Wire types are kept deliberately separate from the internal types they
//! mirror (`StreamEvent`, `Verdict`, `ToolOutcome`, ...) rather than
//! `#[derive(Serialize)]`-ing those types directly: internal types stay free
//! to change shape for the TUI's own needs without silently breaking the
//! wire contract, and the wire contract stays free to evolve independently.
//! The `From`/`fn to_acp` conversions below are the only place the two
//! sides are allowed to know about each other.
//!
//! **Concepts with no ACP equivalent, by design, not oversight** (confirmed
//! against the full v1 schema before writing this, not discovered by trial
//! and error):
//! - Subagent progress (`StreamEvent::AgentActivity`) -- ACP has no nested-
//!   session or subagent model at all. Dropped for now; `_meta` is the
//!   sanctioned extension point if this needs to cross the wire later.
//! - The write/edit truncation flag on `StreamEvent::ToolCalls`
//!   (`finish_reason == "length"`) -- no ACP field for it. Closest fit is
//!   ending the turn with `StopReason::MaxTokens`, not attempted here yet.
//! - Cache-hit token accounting (`ApiUsage::cached_prompt_tokens`) -- ACP
//!   has no such field.
//! - Raw reasoning content (`StreamEvent::Reasoning`) -- matches an existing
//!   product decision, not a new one: `App::append_reasoning`'s own doc
//!   comment already says this "is never shown, persisted, replayed, or
//!   sent back on the wire." ACP's own docs assume thought content IS shown
//!   to the client (its worked examples stream real reasoning text) and
//!   define no privacy/redaction concept anywhere in the schema -- confirmed
//!   by an exhaustive grep, not assumed. The compromise that stays spec-
//!   legal: `AgentThoughtChunk`'s `content` is required (chunks can't be
//!   content-free), so reasoning is not streamed as chunks at all; nothing
//!   currently signals "the model is thinking" on the wire either, since
//!   that would need `SessionUpdate::CurrentModeUpdate` or a `_meta` signal
//!   this module doesn't build yet. Documented as a real, known gap rather
//!   than silently sending nothing and calling it done.

use crate::approval::{ApprovalRequest, Decision};
use crate::headless::BrowserCheckResult;
use crate::llm::{ApiUsage, ToolCall as InternalToolCall};
use crate::tools::{Action, ToolOutcome};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// JSON-RPC 2.0 envelope -- generic, not ACP-specific. ACP is standard
// JSON-RPC 2.0 (confirmed against the schema repo directly): the `"jsonrpc":
// "2.0"` field is always present on the wire, unlike Codex's own app-server
// convention (which omits it) -- that was a Codex-specific choice this
// module's earlier draft mistakenly generalized from, not an ACP rule.
// ---------------------------------------------------------------------------

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

/// One JSON-RPC request (expects a response) -- `id` is required and MUST
/// be echoed back on the matching response. Used both for requests this
/// process receives (`initialize`, `session/new`, `session/prompt`,
/// unimplemented `fs/*`/`terminal/*`) and requests it sends
/// (`session/request_permission`) -- ACP is bidirectional over one
/// connection, both sides originate requests.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct RpcRequest {
    pub jsonrpc: JsonRpcVersion,
    pub id: RequestId,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

/// A JSON-RPC id is a string or a number on the wire -- ACP itself just
/// says "an identifier established by the Client" without constraining the
/// type further, so this accepts either rather than assuming callers only
/// ever send integers.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Hash)]
#[serde(untagged)]
pub enum RequestId {
    Number(i64),
    String(String),
}

/// A successful JSON-RPC response.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct RpcResponse {
    pub jsonrpc: JsonRpcVersion,
    pub id: RequestId,
    pub result: serde_json::Value,
}

/// A JSON-RPC error response.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct RpcError {
    pub jsonrpc: JsonRpcVersion,
    pub id: RequestId,
    pub error: RpcErrorObject,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct RpcErrorObject {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

/// A JSON-RPC notification -- no `id`, no response expected. Used for
/// `session/update` (agent to client) and `session/cancel` (client to
/// agent).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct RpcNotification {
    pub jsonrpc: JsonRpcVersion,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

impl RpcNotification {
    /// One line of NDJSON, `\n`-terminated, ready to write to a stdout pipe.
    pub fn to_ndjson_line(&self) -> String {
        format!("{}\n", serde_json::to_string(self).expect("RpcNotification always serializes"))
    }
}

impl RpcRequest {
    pub fn to_ndjson_line(&self) -> String {
        format!("{}\n", serde_json::to_string(self).expect("RpcRequest always serializes"))
    }
}

// ---------------------------------------------------------------------------
// initialize
// ---------------------------------------------------------------------------

/// A newtype so a malformed or out-of-range version fails to deserialize
/// rather than silently accepting garbage -- mirrors the schema's own
/// `uint16` constraint (`minimum: 0, maximum: 65535`).
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProtocolVersion(pub u16);

/// The version `boxcode` speaks. `1`, not `2` -- see module docs for why.
pub const PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion(1);

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct InitializeRequest {
    #[serde(rename = "protocolVersion")]
    pub protocol_version: ProtocolVersion,
    #[serde(rename = "clientCapabilities", default, skip_serializing_if = "Option::is_none")]
    pub client_capabilities: Option<serde_json::Value>,
    #[serde(rename = "clientInfo", default, skip_serializing_if = "Option::is_none")]
    pub client_info: Option<Implementation>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct InitializeResponse {
    #[serde(rename = "protocolVersion")]
    pub protocol_version: ProtocolVersion,
    /// `{}` is the correct "nothing extra supported" value here, not
    /// omission -- an absent `agentCapabilities` on the wire has its own
    /// (different) meaning per the schema's defaults. Kept as raw JSON since
    /// this module doesn't yet advertise any specific capability
    /// sub-object; `serde_json::json!({})` is what callers should pass.
    #[serde(rename = "agentCapabilities")]
    pub agent_capabilities: serde_json::Value,
    #[serde(rename = "authMethods", default)]
    pub auth_methods: Vec<serde_json::Value>,
    #[serde(rename = "agentInfo", default, skip_serializing_if = "Option::is_none")]
    pub agent_info: Option<Implementation>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Implementation {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub version: String,
}

// ---------------------------------------------------------------------------
// session/new
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionId(pub String);

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct NewSessionRequest {
    pub cwd: String,
    #[serde(rename = "additionalDirectories", default)]
    pub additional_directories: Vec<String>,
    /// Required by the v1 schema (`required: ["cwd", "mcpServers"]`) --
    /// `boxcode` doesn't act as an MCP client yet, so this is always sent
    /// empty, not omitted.
    #[serde(rename = "mcpServers", default)]
    pub mcp_servers: Vec<serde_json::Value>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct NewSessionResponse {
    #[serde(rename = "sessionId")]
    pub session_id: SessionId,
}

// ---------------------------------------------------------------------------
// session/prompt -- the turn. Blocks (per v1) until `stopReason` is known;
// progress streams via session/update notifications in the meantime.
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct PromptRequest {
    #[serde(rename = "sessionId")]
    pub session_id: SessionId,
    pub prompt: Vec<ContentBlock>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct PromptResponse {
    #[serde(rename = "stopReason")]
    pub stop_reason: StopReason,
}

/// `Text` and `Image` are implemented -- the v1 schema's own baseline
/// obligation is exactly this: "the Agent MUST support `ContentBlock::Text`
/// and `ContentBlock::ResourceLink`, while other variants are optionally
/// enabled via `PromptCapabilities`." `Image` is one of those opt-in
/// variants, advertised via `InitializeResponse.agent_capabilities.
/// promptCapabilities.image` (see `transport.rs`'s `initialize` handler).
/// `ResourceLink` isn't implemented yet either (no client-side
/// file-reference flow exists in `boxcode-ide` yet to produce one) --
/// deserializing an unrecognized variant should fail loudly rather than
/// silently drop content, hence no catch-all arm.
///
/// `Image`'s two fields (`data`, `mime_type`) are exactly the ACP schema's
/// required `ImageContent` fields -- `annotations`/`uri`/`_meta` are all
/// optional on the wire and unused here, same minimalism as this module's
/// existing `ToolCallContent::Image` (the unrelated outbound counterpart:
/// that one goes from boxcode to the client's UI for a human to look at;
/// this one comes from the client into a prompt boxcode is about to send
/// to the model).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "type")]
pub enum ContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image")]
    Image {
        data: String,
        #[serde(rename = "mimeType")]
        mime_type: String,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    MaxTurnRequests,
    Refusal,
    Cancelled,
}

// ---------------------------------------------------------------------------
// session/cancel
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct CancelNotification {
    #[serde(rename = "sessionId")]
    pub session_id: SessionId,
}

// ---------------------------------------------------------------------------
// session/update -- the progress stream during a turn.
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct SessionNotification {
    #[serde(rename = "sessionId")]
    pub session_id: SessionId,
    pub update: SessionUpdate,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct MessageId(pub String);

/// A subset of v1's 11 `SessionUpdate` variants -- the ones an initial
/// working turn loop actually needs. `available_commands_update`,
/// `current_mode_update`, `config_option_update`, and `session_info_update`
/// are real, legal ACP variants this module doesn't emit yet; add them when
/// something in `boxcode` actually needs to report one, rather than
/// speculatively wiring all eleven now.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "sessionUpdate")]
pub enum SessionUpdate {
    #[serde(rename = "agent_message_chunk")]
    AgentMessageChunk {
        content: ContentBlock,
        #[serde(rename = "messageId", default, skip_serializing_if = "Option::is_none")]
        message_id: Option<MessageId>,
    },
    #[serde(rename = "tool_call")]
    ToolCall(AcpToolCall),
    #[serde(rename = "tool_call_update")]
    ToolCallUpdate(ToolCallUpdate),
    #[serde(rename = "plan")]
    Plan(Plan),
    #[serde(rename = "usage_update")]
    UsageUpdate {
        used: u64,
        size: u64,
    },
}

// ---------------------------------------------------------------------------
// Tool calls
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct ToolCallId(pub String);

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallStatus {
    Pending,
    InProgress,
    Completed,
    Failed,
}

/// Named `AcpToolCall` (not `ToolCall`) to avoid colliding with
/// `crate::llm::ToolCall` in this module's own imports -- the ACP schema's
/// own name is unqualified `ToolCall`, this is purely a local Rust naming
/// accommodation, not a wire-format difference.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct AcpToolCall {
    #[serde(rename = "toolCallId")]
    pub tool_call_id: ToolCallId,
    pub title: String,
    pub kind: ToolKind,
    pub status: ToolCallStatus,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct ToolCallUpdate {
    #[serde(rename = "toolCallId")]
    pub tool_call_id: ToolCallId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<ToolKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<ToolCallStatus>,
    /// `ToolOutcome::content`/`display` land here as `Text`; a pending
    /// write/edit's before/after text lands here as `Diff` (see
    /// `HeadlessSession::ask_permission`, `tools::preview_change_text`) --
    /// only the two ACP `ToolCallContent` variants a client actually needs
    /// to review a change before approving it. The rest of the real union
    /// (terminal refs) is not implemented yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<ToolCallContent>,
}

/// The two ACP v1 `ToolCallContent` variants this module implements -- see
/// `ToolCallUpdate::content`'s own doc comment for which ones and why.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "type")]
pub enum ToolCallContent {
    #[serde(rename = "content")]
    Text { text: String },
    /// Plain whole-file before/after text, not pre-computed hunks: ACP
    /// clients are expected to have their own real diff renderer (VS
    /// Code's `vscode.changes`, for one) and hunk/render it themselves,
    /// rather than being handed a hunk list shaped for boxcode's own TUI
    /// (`crate::diff::FileDiff`, which stays TUI-only).
    #[serde(rename = "diff")]
    Diff {
        path: String,
        #[serde(rename = "oldText", default, skip_serializing_if = "Option::is_none")]
        old_text: Option<String>,
        #[serde(rename = "newText")]
        new_text: String,
    },
    /// A `check_in_browser` result -- base64-encoded image bytes for the
    /// client to render (e.g. inline in a chat panel), not something
    /// boxcode itself interprets. Correction to an earlier version of this
    /// comment: `llm.rs`'s `ChatMessage.images` already feeds the same
    /// screenshot to the model separately (see that field's own doc
    /// comment), and `ContentBlock::Image` above now does the same for
    /// client-attached images arriving via `session/prompt` -- this variant
    /// stays purely the human-facing rendering path; it is not, and was
    /// never meant to become, boxcode's only multimodal path.
    #[serde(rename = "image")]
    Image {
        #[serde(rename = "mimeType")]
        mime_type: String,
        data: String,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolKind {
    Read,
    Edit,
    Delete,
    Move,
    Search,
    Execute,
    Think,
    Fetch,
    SwitchMode,
    Other,
}

// ---------------------------------------------------------------------------
// Plans -- boxcode's Verdict::Progress (plan-step bookkeeping) and
// Verdict::Todos (the model's own checklist) both map onto this, as two
// conceptually separate plans. v1 has no `planId` to keep them apart on the
// wire (that's a v2 addition) -- see the Plan struct's own doc for how this
// module handles that.
// ---------------------------------------------------------------------------

/// v1's `Plan` has no id -- only one plan can exist per session on the
/// wire. `boxcode` has two independent bookkeeping concepts
/// (`Verdict::Progress` against an approved plan, `Verdict::Todos` as the
/// model's own short-term checklist) that would collide if both tried to
/// emit a `Plan` update. Not resolved by this module: whichever one is
/// wired into `HeadlessSession` first should own the wire's single `Plan`
/// slot; the other stays local-only until v2's `planId` is worth adopting
/// for this specifically.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Plan {
    pub entries: Vec<PlanEntry>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct PlanEntry {
    pub content: String,
    pub priority: PlanEntryPriority,
    pub status: PlanEntryStatus,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PlanEntryPriority {
    High,
    Medium,
    Low,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PlanEntryStatus {
    Pending,
    InProgress,
    Completed,
}

// ---------------------------------------------------------------------------
// session/request_permission -- boxcode's Verdict::Ask, on the wire.
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct RequestPermissionRequest {
    #[serde(rename = "sessionId")]
    pub session_id: SessionId,
    #[serde(rename = "toolCall")]
    pub tool_call: ToolCallUpdate,
    pub options: Vec<PermissionOption>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct RequestPermissionResponse {
    pub outcome: RequestPermissionOutcome,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct PermissionOptionId(pub String);

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct PermissionOption {
    #[serde(rename = "optionId")]
    pub option_id: PermissionOptionId,
    pub name: String,
    pub kind: PermissionOptionKind,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PermissionOptionKind {
    AllowOnce,
    AllowAlways,
    RejectOnce,
    RejectAlways,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "outcome")]
pub enum RequestPermissionOutcome {
    #[serde(rename = "cancelled")]
    Cancelled,
    #[serde(rename = "selected")]
    Selected {
        #[serde(rename = "optionId")]
        option_id: PermissionOptionId,
    },
}

// ---------------------------------------------------------------------------
// session/checkInBrowser -- not part of the official ACP v1 schema (the spec
// has no concept of a browser tab at all), but the same shape as
// session/request_permission: boxcode sends it mid-turn, needs the client's
// answer to continue, and the transport-level deferred-response machinery
// that `session/request_permission` already proved out applies unchanged.
// See `headless.rs`'s `BrowserCheckAsk`/`BrowserCheckResult` for the Rust
// side of this.
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct CheckInBrowserRequest {
    #[serde(rename = "sessionId")]
    pub session_id: SessionId,
    pub url: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "outcome")]
pub enum CheckInBrowserOutcome {
    #[serde(rename = "screenshot")]
    Screenshot {
        #[serde(rename = "mimeType")]
        mime_type: String,
        data: String,
    },
    #[serde(rename = "failed")]
    Failed { reason: String },
}

/// The two standing options `boxcode` offers on every permission request --
/// "allow once" / "reject once" only. `allow_always`/`reject_always`
/// (remembering the choice) aren't implemented yet; `config.tools.approval`
/// is `boxcode`'s existing equivalent of "remember my choice" and already
/// applies before a request is ever built (see `approval::verdict_for`), so
/// there's nothing for an "always" *option on this specific request* to do
/// that isn't already covered.
pub fn standard_options() -> Vec<PermissionOption> {
    vec![
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
    ]
}

impl From<CheckInBrowserOutcome> for BrowserCheckResult {
    fn from(outcome: CheckInBrowserOutcome) -> Self {
        match outcome {
            CheckInBrowserOutcome::Screenshot { mime_type, data } => {
                BrowserCheckResult::Screenshot { mime_type, data }
            }
            CheckInBrowserOutcome::Failed { reason } => BrowserCheckResult::Failed(reason),
        }
    }
}

impl From<RequestPermissionOutcome> for Decision {
    fn from(outcome: RequestPermissionOutcome) -> Self {
        match outcome {
            RequestPermissionOutcome::Cancelled => Decision::Refused,
            RequestPermissionOutcome::Selected { option_id } => {
                if option_id.0 == "allow" {
                    Decision::Allowed
                } else {
                    Decision::Refused
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// session/rollback -- not part of ACP's own schema, and unlike
// session/checkInBrowser, not agent-initiated either: this is a plain
// client-to-agent request, the same shape as session/prompt or session/new,
// just answered synchronously (rollback::apply is local disk I/O, not an
// LLM round trip, so none of session/prompt's own deferred-response
// machinery is needed here -- see transport.rs's docs on why that one
// specifically has to be deferred).
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct RollbackRequest {
    #[serde(rename = "sessionId")]
    pub session_id: SessionId,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct RollbackResponse {
    /// The same human-readable text `App`'s own `/rollback` shows in the
    /// TUI transcript (`rollback::Report::summary`) -- one string a client
    /// can show as-is rather than re-deriving its own wording from
    /// structured fields this module doesn't otherwise need.
    pub summary: String,
}

// ---------------------------------------------------------------------------
// Conversions from boxcode's internal types
// ---------------------------------------------------------------------------

impl From<&InternalToolCall> for ToolCallId {
    fn from(call: &InternalToolCall) -> Self {
        ToolCallId(call.id.clone())
    }
}

/// `Action` (`crate::tools::Action`) isn't mirrored variant-for-variant --
/// same reasoning as this module's earlier draft: a large, still-growing
/// enum whose only externally-relevant fact today is what kind of tool it
/// is and its rendered label. `Action::label()` becomes the ACP `title`;
/// this match only decides `ToolKind`.
fn tool_kind_for(action: &Action) -> ToolKind {
    match action {
        Action::Read { .. } | Action::List { .. } | Action::Glob { .. } | Action::Grep { .. } => {
            ToolKind::Read
        }
        Action::Write { .. } | Action::Edit { .. } => ToolKind::Edit,
        Action::Command { .. } => ToolKind::Execute,
        Action::Search { .. } | Action::DesignStarter | Action::CheckContrast { .. } => {
            ToolKind::Search
        }
        Action::Agent { .. } => ToolKind::Think,
        Action::Deploy { .. }
        | Action::DeployBackend { .. }
        | Action::EnableAuth { .. }
        | Action::DbQuery { .. }
        | Action::ListChangeRequests { .. }
        | Action::ResolveChangeRequest { .. }
        | Action::Publish { .. }
        | Action::CheckInBrowser { .. } => ToolKind::Fetch,
        Action::Plan(_) | Action::Progress { .. } | Action::Todos(_) => ToolKind::SwitchMode,
    }
}

impl AcpToolCall {
    /// The initial, `pending` announcement of a call awaiting a decision --
    /// built from a [`Verdict::Ask`]'s carried [`Action`], the same
    /// interpreted value [`ApprovalRequest`] already carries.
    pub fn pending(call: &InternalToolCall, action: &Action) -> Self {
        Self {
            tool_call_id: call.into(),
            title: action.label(),
            kind: tool_kind_for(action),
            status: ToolCallStatus::Pending,
        }
    }
}

impl From<&ApprovalRequest> for RequestPermissionRequest {
    fn from(request: &ApprovalRequest) -> Self {
        Self {
            // Caller fills in session_id -- ApprovalRequest doesn't carry
            // one, it's a per-session concept this module's types don't
            // otherwise need to know about.
            session_id: SessionId(String::new()),
            tool_call: ToolCallUpdate {
                tool_call_id: (&request.call).into(),
                title: Some(request.action.label()),
                kind: Some(tool_kind_for(&request.action)),
                status: Some(ToolCallStatus::Pending),
                content: None,
            },
            options: standard_options(),
        }
    }
}

impl From<&ToolOutcome> for ToolCallUpdate {
    fn from(outcome: &ToolOutcome) -> Self {
        Self {
            tool_call_id: ToolCallId(outcome.call_id.clone()),
            title: None,
            kind: None,
            status: Some(ToolCallStatus::Completed),
            content: Some(ToolCallContent::Text { text: outcome.content.clone() }),
        }
    }
}

impl From<ApiUsage> for SessionUpdate {
    fn from(usage: ApiUsage) -> Self {
        // ACP's usage_update models context-window occupancy (used/size),
        // not cumulative billing -- a real impedance mismatch with
        // ApiUsage's prompt/completion split, confirmed against the schema
        // before writing this, not guessed. `used` is the closest available
        // proxy (total tokens spent this turn); `size` has no source in
        // ApiUsage at all -- boxcode doesn't track the model's context
        // limit anywhere yet, so this is a real gap, not a rounding choice.
        SessionUpdate::UsageUpdate { used: usage.total() as u64, size: 0 }
    }
}

/// Builds the two [`SessionUpdate`]s a resolved [`Verdict::Progress`] or
/// [`Verdict::Todos`] becomes -- see [`Plan`]'s own docs for why only one
/// of these should actually be wired into a live session today.
pub fn plan_for_todos(items: &[crate::tools::TodoItem]) -> Plan {
    Plan {
        entries: items
            .iter()
            .map(|item| PlanEntry {
                content: item.content.clone(),
                // TodoItem has no priority concept; ACP requires one.
                // Medium is the least presumptive default -- neither
                // over- nor under-stating urgency for something boxcode
                // itself never asked the model to rank.
                priority: PlanEntryPriority::Medium,
                status: match item.status {
                    crate::tools::TodoStatus::Pending => PlanEntryStatus::Pending,
                    crate::tools::TodoStatus::InProgress => PlanEntryStatus::InProgress,
                    crate::tools::TodoStatus::Completed => PlanEntryStatus::Completed,
                },
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Pinned against literal JSON, not just Rust round-trip symmetry --
    /// this is a contract with real external clients (or, eventually,
    /// boxcode-ide), not just with itself.
    #[test]
    fn initialize_request_matches_the_documented_v1_shape() {
        let req = InitializeRequest {
            protocol_version: PROTOCOL_VERSION,
            client_capabilities: None,
            client_info: None,
        };
        let value = serde_json::to_value(&req).unwrap();
        assert_eq!(value, json!({ "protocolVersion": 1 }));
    }

    #[test]
    fn a_prompt_request_matches_the_documented_v1_shape() {
        let req = PromptRequest {
            session_id: SessionId("sess_1".to_string()),
            prompt: vec![ContentBlock::Text { text: "start a new webapp project".to_string() }],
        };
        let value = serde_json::to_value(&req).unwrap();
        assert_eq!(
            value,
            json!({
                "sessionId": "sess_1",
                "prompt": [{ "type": "text", "text": "start a new webapp project" }]
            })
        );
    }

    #[test]
    fn a_prompt_request_with_an_image_block_matches_the_documented_v1_shape() {
        let req = PromptRequest {
            session_id: SessionId("sess_1".to_string()),
            prompt: vec![
                ContentBlock::Text { text: "what's wrong with this button?".to_string() },
                ContentBlock::Image { data: "aGVsbG8=".to_string(), mime_type: "image/png".to_string() },
            ],
        };
        let value = serde_json::to_value(&req).unwrap();
        assert_eq!(
            value,
            json!({
                "sessionId": "sess_1",
                "prompt": [
                    { "type": "text", "text": "what's wrong with this button?" },
                    { "type": "image", "data": "aGVsbG8=", "mimeType": "image/png" }
                ]
            })
        );
    }

    #[test]
    fn a_prompt_response_carries_the_stop_reason_snake_case() {
        let resp = PromptResponse { stop_reason: StopReason::EndTurn };
        let value = serde_json::to_value(&resp).unwrap();
        assert_eq!(value, json!({ "stopReason": "end_turn" }));
    }

    #[test]
    fn tool_call_update_omits_unset_fields_rather_than_nulling_them() {
        let update = ToolCallUpdate {
            tool_call_id: ToolCallId("call_1".to_string()),
            title: None,
            kind: None,
            status: Some(ToolCallStatus::Completed),
            content: Some(ToolCallContent::Text { text: "done".to_string() }),
        };
        let value = serde_json::to_value(&update).unwrap();
        assert_eq!(
            value,
            json!({
                "toolCallId": "call_1",
                "status": "completed",
                "content": { "type": "content", "text": "done" }
            })
        );
    }

    /// The other half of `ToolCallContent`: a diff carries its path and
    /// both texts, tagged the same way ACP's own `content` variant is --
    /// one client-side switch on `type`, not two different envelopes.
    #[test]
    fn tool_call_content_diff_serializes_with_old_and_new_text() {
        let content = ToolCallContent::Diff {
            path: "src/app.rs".to_string(),
            old_text: Some("before".to_string()),
            new_text: "after".to_string(),
        };
        let value = serde_json::to_value(&content).unwrap();
        assert_eq!(
            value,
            json!({ "type": "diff", "path": "src/app.rs", "oldText": "before", "newText": "after" })
        );
    }

    /// A brand-new file has no "before" -- `oldText` must be omitted, not
    /// sent as `null`, matching every other optional field's convention in
    /// this module.
    #[test]
    fn tool_call_content_diff_omits_old_text_for_a_new_file() {
        let content = ToolCallContent::Diff {
            path: "src/new.rs".to_string(),
            old_text: None,
            new_text: "fresh".to_string(),
        };
        let value = serde_json::to_value(&content).unwrap();
        assert_eq!(value, json!({ "type": "diff", "path": "src/new.rs", "newText": "fresh" }));
    }

    /// `check_in_browser`'s wire shape: base64 bytes plus the MIME type the
    /// client needs to render them, tagged the same way `Text`/`Diff` are.
    #[test]
    fn tool_call_content_image_serializes_with_mime_type_and_data() {
        let content = ToolCallContent::Image {
            mime_type: "image/png".to_string(),
            data: "aGVsbG8=".to_string(),
        };
        let value = serde_json::to_value(&content).unwrap();
        assert_eq!(
            value,
            json!({ "type": "image", "mimeType": "image/png", "data": "aGVsbG8=" })
        );
    }

    #[test]
    fn a_session_update_tags_itself_by_sessionupdate_field() {
        let update = SessionUpdate::AgentMessageChunk {
            content: ContentBlock::Text { text: "hi".to_string() },
            message_id: None,
        };
        let value = serde_json::to_value(&update).unwrap();
        assert_eq!(
            value,
            json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "hi" } })
        );
    }

    #[test]
    fn a_selected_permission_outcome_becomes_the_matching_decision() {
        assert_eq!(
            Decision::from(RequestPermissionOutcome::Selected {
                option_id: PermissionOptionId("allow".to_string())
            }),
            Decision::Allowed
        );
        assert_eq!(
            Decision::from(RequestPermissionOutcome::Selected {
                option_id: PermissionOptionId("reject".to_string())
            }),
            Decision::Refused
        );
        assert_eq!(Decision::from(RequestPermissionOutcome::Cancelled), Decision::Refused);
    }

    #[test]
    fn an_unsupported_jsonrpc_version_is_rejected() {
        let line = json!({
            "jsonrpc": "1.0",
            "id": 1,
            "method": "initialize",
            "params": { "protocolVersion": 1 }
        })
        .to_string();
        assert!(serde_json::from_str::<RpcRequest>(&line).is_err());
    }

    #[test]
    fn a_todo_list_becomes_a_well_formed_plan() {
        use crate::tools::{TodoItem, TodoStatus};
        let plan = plan_for_todos(&[
            TodoItem { content: "write the tests".to_string(), status: TodoStatus::InProgress },
            TodoItem { content: "ship it".to_string(), status: TodoStatus::Pending },
        ]);
        assert_eq!(plan.entries.len(), 2);
        assert_eq!(plan.entries[0].status, PlanEntryStatus::InProgress);
        assert_eq!(plan.entries[1].status, PlanEntryStatus::Pending);
    }
}
