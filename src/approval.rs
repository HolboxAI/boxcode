//! The typed seam between "the model wants to do something" and "the user
//! decided" -- step one of extracting the agent loop from `App` (see
//! `upgrade-plan.md`, Phase 3).
//!
//! Today these two types travel a very short distance: `App::advance_approvals`
//! builds an [`ApprovalRequest`], the approval popup renders it out of
//! `Overlay::ToolApproval`, and the keypress becomes a [`Decision`] consumed by
//! `App::decide`. That is deliberate. The point of this module is not what it
//! does now but what it pins down: every approval flows through exactly one
//! request type and one decision function, no matter where the question came
//! from. When the agent loop becomes its own task, these become the messages
//! on the channel between it and the UI -- and nothing about what the user
//! sees or answers has to change, because the popup already speaks this type.
//!
//! Nothing in here weakens or reroutes the safety decisions themselves:
//! blocked commands are refused before a request is ever built, plan mode
//! filters before it, and `[tools] approval` is judged before it. A request existing means "the user must answer this",
//! and there is no constructor that skips the queue.

use crate::llm::ToolCall;
use crate::tools::Action;

/// One thing the model wants to do, waiting for the user's answer.
#[derive(Clone, Debug, PartialEq)]
pub struct ApprovalRequest {
    /// The call exactly as the model made it -- what executes if allowed.
    pub call: ToolCall,
    /// What that call *means*, already interpreted -- what the popup shows.
    /// Carried alongside `call` rather than re-derived so the thing displayed
    /// and the thing decided can never drift apart.
    pub action: Action,
    /// How many more calls are queued behind this one.
    pub remaining: usize,
    /// For a `Write` or an `Edit`, what the file looks like before and after
    /// -- the diff the popup draws instead of describing the change in prose.
    ///
    /// It lives on the request rather than inside [`Action`] because it is not
    /// part of what the call *means*; it is what the question looks like when
    /// asked. `Action` stays a pure interpretation of the tool call, which is
    /// what lets the same value be compared, logged and matched on without
    /// dragging a snapshot of the filesystem along with it.
    ///
    /// Computed once, when the request is built, against the file as it is at
    /// that moment. `None` when there is nothing to show -- see
    /// [`crate::tools::preview_change`] -- and the popup falls back to its
    /// plain rendering.
    pub preview: Option<crate::diff::FileDiff>,
}

/// The user's answer to one [`ApprovalRequest`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Run it. For a plan, adopt it; for a deployment, hand over to the
    /// deploy flow.
    Allowed,
    /// Don't. The model is told the call was declined and carries on without
    /// it; nothing is executed.
    Refused,
}

impl Decision {
    pub fn is_allowed(self) -> bool {
        self == Decision::Allowed
    }
}

/// What to do with one queued call -- the pure decision `App::advance_approvals`
/// makes as it walks `pending_tools`, extracted so a headless (RPC-driven)
/// session can make the exact same decision without needing any of `App`'s
/// other state.
///
/// Confirmed narrow before this was written, not assumed: an independent
/// review traced every predicate `advance_approvals` calls and found the
/// whole six-way ordering depends on exactly the call, the workspace it
/// would run in, plan mode, and the configured approval policy -- nothing
/// else out of `App`'s ~60 fields. That review also found this ordering
/// changed 15 times in `needs_approval` alone across three weeks of this
/// project's history, several of those changes closing real safety gaps
/// ("Put the guardrail above approval, where no setting can reach it") --
/// which is why this is a single shared function a headless session calls
/// into, rather than a second implementation a headless session maintains
/// on its own. Two independently-maintained copies of *this specific*
/// logic drifting apart is not a cosmetic bug; it is the RPC surface
/// running something the TUI would have refused.
///
/// `App::advance_approvals` and any headless equivalent match on this and
/// apply their own effects for each variant (push a message, mutate a
/// plan, show a prompt, ...) -- this function decides, callers act.
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    /// Refused outright, unconditionally. No approval mode or setting
    /// reaches this -- see `danger::Risk::Blocked` and its own callers for
    /// why that has to stay true.
    Blocked(String),
    /// Refused because plan mode is on and this call is not read-only.
    PlanRefused(String),
    /// `plan_progress` bookkeeping against a plan the user already
    /// approved -- always auto-resolved, never asked, because there is
    /// nothing here for a prompt to protect.
    Progress { step: usize, done: bool, note: Option<String> },
    /// `update_todos` bookkeeping -- always auto-resolved, same reasoning
    /// as `Progress`.
    Todos(Vec<crate::tools::TodoItem>),
    /// Nothing here needs asking about -- either the configured policy
    /// auto-approves it, or there was nothing coherent to interpret in the
    /// first place, in which case the runner is better placed to explain
    /// the malformed arguments back to the model than a prompt would be.
    AutoApprove,
    /// Stop and ask. Carries the interpreted action so a caller can render
    /// or wire it into an `ApprovalRequest` without re-deriving it.
    Ask(crate::tools::Action),
}

/// The pure decision itself -- see [`Verdict`]'s docs for why this exists
/// as a free function taking exactly these four things, not a method on
/// `App`. Mirrors `App::advance_approvals`'s ordering exactly: verified
/// against it line by line, not reproduced from memory. Change the
/// ordering here, not in two places -- that is the entire point.
pub fn verdict_for(
    call: &crate::llm::ToolCall,
    workspace_root: &std::path::Path,
    mode: crate::tools::Mode,
    approval_mode: crate::config::ApprovalMode,
) -> Verdict {
    let action = crate::tools::describe_action(call);

    // 1. Blocked outright -- ranked above plan mode so a catastrophic
    // command is reported as blocked rather than merely out of scope.
    let risk = match &action {
        Some(a) => crate::tools::action_risk(a, workspace_root),
        None => crate::danger::Risk::Normal,
    };
    if let crate::danger::Risk::Blocked(reason) = risk {
        return Verdict::Blocked(reason);
    }

    // 2. Plan mode outranks every approval setting.
    if mode.is_plan() {
        if let Some(a) = &action {
            if let Some(reason) = crate::tools::plan_mode_block(a) {
                return Verdict::PlanRefused(reason);
            }
        }
    }

    // 3 & 4. Pure in-memory bookkeeping against state the user already
    // agreed to -- never prompted, same reasoning for both: there is
    // nothing here for a prompt to protect.
    match &action {
        Some(crate::tools::Action::Progress { step, done, note }) => {
            return Verdict::Progress { step: *step, done: *done, note: note.clone() };
        }
        Some(crate::tools::Action::Todos(items)) => {
            return Verdict::Todos(items.clone());
        }
        _ => {}
    }

    // 5. Whether the configured policy needs to ask at all.
    let needs_approval = match action {
        Some(ref a) => {
            risk.is_dangerous()
                || matches!(a, crate::tools::Action::Plan(_))
                || match approval_mode {
                    crate::config::ApprovalMode::Destructive => false,
                    crate::config::ApprovalMode::Always => !is_read_only_action(a),
                }
        }
        // Nothing coherent to show, so nothing to approve -- see the
        // `AutoApprove` variant's docs.
        None => false,
    };
    if !needs_approval {
        return Verdict::AutoApprove;
    }

    // 6. Everything else stops and asks. `action` is `Some` here: the
    // `None` branch above always sets `needs_approval` false.
    Verdict::Ask(action.expect("needs_approval only true when action is Some"))
}

/// Whether `action` changes nothing, for `ApprovalMode::Always`. Mirrors
/// `App::is_read_only_action` exactly -- see that copy's own docs for why
/// `write_file`/`edit_file` are deliberately absent from this list.
fn is_read_only_action(action: &crate::tools::Action) -> bool {
    match action {
        crate::tools::Action::Read { .. }
        | crate::tools::Action::List { .. }
        | crate::tools::Action::Glob { .. }
        | crate::tools::Action::Grep { .. }
        | crate::tools::Action::DesignStarter
        | crate::tools::Action::CheckContrast { .. }
        | crate::tools::Action::Agent { .. } => true,
        crate::tools::Action::Command { command, .. } => crate::tools::is_read_only(command),
        _ => false,
    }
}
