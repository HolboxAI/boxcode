//! Telling one kind of failure from another.
//!
//! Every failure used to render identically: the word `Error:` in red, then a
//! paragraph. "You have used today's allowance" and "the endpoint is
//! unreachable" looked the same, so the reader had to parse prose to work out
//! whether anything was actually broken and whether they could do something
//! about it.
//!
//! The axis that matters is not severity, it is **agency**:
//!
//! - Amber: expected, and there is something you can do -- a spent allowance,
//!   a full context window, a truncated reply.
//! - Red: something is wrong that you did not cause -- a rejected key, an
//!   unreachable endpoint, a provider fault.
//!
//! Classification works off markers this crate and the gateway both control
//! (see `markers`), rather than guessing from free prose. The tests feed the
//! real strings the app produces through `classify`, so a reworded message
//! that stops matching fails the build instead of silently going grey.

use crate::theme;
use ratatui::style::{Modifier, Style};

/// Substrings that classification keys on. Constants rather than literals at
/// the match site, so the producer and the classifier are edited together.
pub mod markers {
    /// Codes the gateway returns in its OpenAI-shaped error bodies.
    pub const GATEWAY_DEVICE_LIMIT: &str = "daily_limit_reached";
    pub const GATEWAY_PAUSED: &str = "free_tier_paused";
    pub const GATEWAY_BAD_TOKEN: &str = "invalid_token";
    pub const GATEWAY_NO_TOKEN: &str = "missing_token";
    pub const GATEWAY_BLOCKED: &str = "device_blocked";
    pub const GATEWAY_IP_LIMITED: &str = "registration_rate_limited";

    /// What providers call a conversation that no longer fits.
    pub const CONTEXT_EXCEEDED: &str = "context_length_exceeded";
    pub const CONTEXT_MAXIMUM: &str = "maximum context length";
    pub const CONTEXT_TOO_LONG: &str = "too many tokens";

    /// Produced by this crate.
    pub const LOCAL_LIMIT: &str = "Daily limit reached";
    pub const TRUNCATED: &str = "output cap and was cut off";
    pub const UNREACHABLE: &str = "Could not reach";
    pub const CONTEXT_FULL_LOCAL: &str = "conversation is too long";
}

/// What kind of thing went wrong.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// This user's own allowance for the day is spent -- either the local
    /// `[quota]` ceiling or the free tier's per-device budget.
    DailyLimit,
    /// The shared free tier is out of capacity for everyone, not just you.
    /// Distinct from `DailyLimit` because the remedy is different: waiting or
    /// using your own key, rather than raising your own limit.
    ProviderCapacity,
    /// The conversation no longer fits in the model's context window.
    ContextFull,
    /// The reply hit the output cap mid-sentence.
    Truncated,
    /// A key or device token was rejected.
    Auth,
    /// Slow down.
    RateLimited,
    /// The endpoint could not be reached at all.
    Offline,
    /// The provider answered, with a failure.
    Provider,
    /// This device is not allowed to use the free tier.
    Refused,
    /// Anything not recognised. Rendered as a plain error rather than
    /// mislabelled as something specific.
    Other,
}

impl Kind {
    /// Amber for "expected, you can act on it"; red for "something is wrong".
    pub fn color(self) -> ratatui::style::Color {
        match self {
            Kind::DailyLimit
            | Kind::ProviderCapacity
            | Kind::ContextFull
            | Kind::Truncated
            | Kind::RateLimited => theme::p().warning,
            Kind::Auth | Kind::Offline | Kind::Provider | Kind::Refused | Kind::Other => {
                theme::p().danger
            }
        }
    }

    pub fn style(self) -> Style {
        Style::default().fg(self.color()).add_modifier(Modifier::BOLD)
    }

    /// A glyph, so the kind is legible before any text is read.
    pub fn icon(self) -> &'static str {
        match self {
            Kind::DailyLimit => "◔",
            Kind::ProviderCapacity => "◍",
            Kind::ContextFull => "▣",
            Kind::Truncated => "✂",
            Kind::Auth => "⚿",
            Kind::RateLimited => "⏳",
            Kind::Offline => "⊘",
            Kind::Provider => "⚠",
            Kind::Refused => "⊗",
            Kind::Other => "✗",
        }
    }

    /// The headline, replacing the undifferentiated `Error:`.
    pub fn headline(self) -> &'static str {
        match self {
            Kind::DailyLimit => "Daily limit reached",
            Kind::ProviderCapacity => "Free tier at capacity",
            Kind::ContextFull => "Conversation too long",
            Kind::Truncated => "Reply cut off",
            Kind::Auth => "Not authorised",
            Kind::RateLimited => "Rate limited",
            Kind::Offline => "Endpoint unreachable",
            Kind::Provider => "Provider error",
            Kind::Refused => "Not eligible",
            Kind::Other => "Error",
        }
    }

    /// One line on what to do, shown under the detail.
    ///
    /// Only where there is a genuine next step. A hint that says nothing is
    /// worse than none, because it trains people to skip the line that
    /// sometimes matters.
    pub fn hint(self) -> Option<&'static str> {
        match self {
            Kind::DailyLimit => Some(
                "Resets at UTC midnight · /quota to see the numbers · /provider to use your own key",
            ),
            Kind::ProviderCapacity => {
                Some("Not caused by you. Try later, or /provider to use your own key")
            }
            Kind::ContextFull => Some("/new starts a fresh conversation and clears the history"),
            Kind::Truncated => Some("Raise max_tokens under [llm], or ask for smaller pieces"),
            Kind::Auth => Some("/provider to set a working key"),
            Kind::RateLimited => Some("Wait a moment and send again"),
            Kind::Offline => Some("Check your connection, then /provider to confirm the endpoint"),
            Kind::Provider | Kind::Refused | Kind::Other => None,
        }
    }
}

/// Work out what a failure message is about.
///
/// Ordered most specific first: a gateway body naming `free_tier_paused` also
/// contains the word "limit", so matching loosely would file a fleet-wide
/// outage as the user's own spent allowance and send them to raise a limit that
/// is not the problem.
pub fn classify(text: &str) -> Kind {
    use markers as m;
    let lower = text.to_lowercase();

    if text.contains(m::GATEWAY_PAUSED) || lower.contains("free tier has reached its capacity") {
        return Kind::ProviderCapacity;
    }
    if text.contains(m::GATEWAY_DEVICE_LIMIT)
        || text.contains(m::LOCAL_LIMIT)
        || lower.contains("daily free-tier limit reached")
    {
        return Kind::DailyLimit;
    }
    if text.contains(m::CONTEXT_EXCEEDED)
        || lower.contains(m::CONTEXT_MAXIMUM)
        || lower.contains(m::CONTEXT_TOO_LONG)
        || lower.contains(m::CONTEXT_FULL_LOCAL)
    {
        return Kind::ContextFull;
    }
    if lower.contains(m::TRUNCATED) {
        return Kind::Truncated;
    }
    if text.contains(m::GATEWAY_BLOCKED) || lower.contains("not eligible for the free tier") {
        return Kind::Refused;
    }
    if text.contains(m::GATEWAY_BAD_TOKEN)
        || text.contains(m::GATEWAY_NO_TOKEN)
        || lower.contains("no longer registered")
        || lower.contains("invalid api key")
        || lower.contains("http 401")
        || lower.contains("http 403")
    {
        return Kind::Auth;
    }
    if text.contains(m::GATEWAY_IP_LIMITED) || lower.contains("http 429") || lower.contains("rate limit")
    {
        return Kind::RateLimited;
    }
    if text.contains(m::UNREACHABLE) || lower.contains("connection") || lower.contains("dns") {
        return Kind::Offline;
    }
    if lower.contains("http ") || lower.contains("provider returned") {
        return Kind::Provider;
    }
    Kind::Other
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The messages this crate actually produces, verbatim. If one is reworded
    /// so it stops classifying, this fails rather than the notice silently
    /// degrading to a grey "Error".
    #[test]
    fn the_real_messages_this_app_produces_are_classified() {
        let cases: &[(&str, Kind)] = &[
            // quota.rs
            (
                "Daily limit reached — requests 5 of 5. Resets in 3h 2m (UTC midnight).\n\
                 Type /quota override to keep going today",
                Kind::DailyLimit,
            ),
            // llm.rs, from the gateway's 402
            (
                "Daily free-tier limit reached ($0.25 of $0.25). Resets at 2026-08-07T00:00:00.000Z.\n\n\
                 The free tier resets at UTC midnight.",
                Kind::DailyLimit,
            ),
            // llm.rs, from the gateway's 503
            (
                "The free tier has reached its capacity for today and is paused.\n\n\
                 Add your own API key with /provider to continue immediately.",
                Kind::ProviderCapacity,
            ),
            // llm.rs truncation notice
            (
                "The reply hit the 16384-token output cap and was cut off. Raise `max_tokens`.",
                Kind::Truncated,
            ),
            // llm.rs, invalidated device token
            (
                "This device is no longer registered for the free tier. Restart the app to enrol again.",
                Kind::Auth,
            ),
            // llm.rs connectivity
            (
                "Could not reach http://localhost:8000/v1/chat/completions: error trying to connect",
                Kind::Offline,
            ),
            // gateway, blocked device
            (
                r#"{"error":{"code":"device_blocked","message":"This device is not eligible for the free tier."}}"#,
                Kind::Refused,
            ),
            // gateway, registration rate limit
            (
                r#"{"error":{"code":"registration_rate_limited","message":"Too many new devices."}}"#,
                Kind::RateLimited,
            ),
        ];
        for (text, expected) in cases {
            assert_eq!(classify(text), *expected, "misclassified: {text}");
        }
    }

    /// The one that matters most: a fleet-wide outage must not be filed as the
    /// user's own spent allowance, or they are sent to raise a limit that is
    /// not the problem.
    #[test]
    fn a_capacity_outage_is_not_mistaken_for_a_personal_limit() {
        let paused = r#"{"error":{"code":"free_tier_paused","message":"The free tier has reached its capacity for today and is paused."}}"#;
        assert_eq!(classify(paused), Kind::ProviderCapacity);
        assert_ne!(classify(paused), Kind::DailyLimit);
    }

    #[test]
    fn context_length_is_recognised_in_the_shapes_providers_use() {
        for text in [
            r#"{"error":{"code":"context_length_exceeded","message":"..."}}"#,
            "This model's maximum context length is 65536 tokens",
            "the request contains too many tokens",
            "This conversation is too long for the model's context window.",
        ] {
            assert_eq!(classify(text), Kind::ContextFull, "{text}");
        }
    }

    #[test]
    fn something_unrecognised_stays_a_plain_error_rather_than_being_mislabelled() {
        assert_eq!(classify("something went sideways"), Kind::Other);
        assert_eq!(Kind::Other.headline(), "Error");
        assert!(Kind::Other.hint().is_none());
    }

    /// Amber means "you can act on this"; red means "something is wrong".
    /// Getting these the wrong way round is what makes a wall of red
    /// unreadable.
    #[test]
    fn actionable_kinds_are_amber_and_faults_are_red() {
        for k in [Kind::DailyLimit, Kind::ProviderCapacity, Kind::ContextFull, Kind::Truncated] {
            assert_eq!(k.color(), theme::p().warning, "{k:?} is expected, not a fault");
        }
        for k in [Kind::Auth, Kind::Offline, Kind::Provider, Kind::Other] {
            assert_eq!(k.color(), theme::p().danger, "{k:?} is a fault");
        }
    }

    #[test]
    fn every_kind_has_a_distinct_icon_and_headline() {
        let all = [
            Kind::DailyLimit,
            Kind::ProviderCapacity,
            Kind::ContextFull,
            Kind::Truncated,
            Kind::Auth,
            Kind::RateLimited,
            Kind::Offline,
            Kind::Provider,
            Kind::Refused,
            Kind::Other,
        ];
        let icons: std::collections::HashSet<_> = all.iter().map(|k| k.icon()).collect();
        let heads: std::collections::HashSet<_> = all.iter().map(|k| k.headline()).collect();
        assert_eq!(icons.len(), all.len(), "icons must be distinguishable");
        assert_eq!(heads.len(), all.len(), "headlines must be distinguishable");
    }

    /// A hint that says nothing is worse than none: it trains people to skip
    /// the line that sometimes matters.
    #[test]
    fn hints_name_a_real_next_step() {
        for k in [Kind::DailyLimit, Kind::ContextFull, Kind::Truncated, Kind::Auth] {
            let hint = k.hint().expect("should have a hint");
            assert!(
                hint.contains('/') || hint.contains("max_tokens") || hint.contains("Wait"),
                "{k:?} hint names no action: {hint}"
            );
        }
    }
}
