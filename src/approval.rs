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
//! filters before it, and `require_approval`/`auto_approve_read_only` are
//! judged before it. A request existing means "the user must answer this",
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
