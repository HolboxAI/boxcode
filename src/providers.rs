/// Static registry backing the `/provider` and `/model` overlays. A "Custom
/// endpoint..." escape hatch lives alongside this list in app.rs so the tool's
/// "any OpenAI-compatible endpoint" generality is never limited to this table.
pub struct Provider {
    pub id: &'static str,
    pub label: &'static str,
    pub endpoint: &'static str,
    pub models: &'static [&'static str],
}

pub const PROVIDERS: &[Provider] = &[
    Provider {
        id: "deepseek",
        label: "DeepSeek",
        endpoint: "https://api.deepseek.com",
        // deepseek-chat / deepseek-reasoner were retired 2026-07-24; current
        // lineup is v4-pro (flagship) / v4-flash (faster, cheaper).
        models: &["deepseek-v4-pro", "deepseek-v4-flash"],
    },
    Provider {
        id: "openai",
        label: "OpenAI",
        endpoint: "https://api.openai.com",
        // gpt-4o / gpt-4-turbo / gpt-3.5-turbo are deprecated; current lineup
        // is the GPT-5.6 family: sol (frontier), terra (balanced), luna (cost).
        models: &["gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna"],
    },
];

pub fn find_provider(id: &str) -> Option<&'static Provider> {
    PROVIDERS.iter().find(|p| p.id == id)
}

/// The conventional env var a user is likely to already have exported for this
/// provider, e.g. "deepseek" -> "DEEPSEEK_API_KEY".
pub fn env_var_name(provider_id: &str) -> String {
    format!("{}_API_KEY", provider_id.to_uppercase())
}

/// The temperature a provider gets when `config.toml` has not set one
/// explicitly (see `LlmConfig::effective_temperature`).
///
/// `None` for every provider except DeepSeek: leaving the field off the
/// request entirely is the safe, unobtrusive default, since it lets each
/// endpoint's own default stand rather than second-guessing providers this
/// tool has no specific reason to disagree with. DeepSeek is the one
/// exception on record -- its models are used here mainly for agentic coding
/// work, where a wandering, non-zero default sampling temperature measurably
/// hurts tool-call reliability, so `0.0` (deterministic) is sent explicitly
/// rather than left to chance.
pub fn default_temperature(provider_id: &str) -> Option<f32> {
    match provider_id {
        "deepseek" => Some(0.0),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_provider_has_at_least_one_model() {
        for p in PROVIDERS {
            assert!(!p.models.is_empty(), "{} has no models", p.id);
        }
    }

    #[test]
    fn find_provider_looks_up_by_id() {
        assert_eq!(find_provider("deepseek").unwrap().label, "DeepSeek");
        assert!(find_provider("nonexistent").is_none());
    }

    #[test]
    fn env_var_name_uppercases_the_id() {
        assert_eq!(env_var_name("deepseek"), "DEEPSEEK_API_KEY");
        assert_eq!(env_var_name("openai"), "OPENAI_API_KEY");
    }

    /// DeepSeek is the one provider with a specific, on-record reason to
    /// override the endpoint's own default: agentic tool-call reliability.
    /// Everyone else, including an unrecognised or custom endpoint, gets
    /// `None` so the field is simply not sent.
    #[test]
    fn only_deepseek_gets_a_built_in_temperature_default() {
        assert_eq!(default_temperature("deepseek"), Some(0.0));
        assert_eq!(default_temperature("openai"), None);
        assert_eq!(default_temperature(""), None);
        assert_eq!(default_temperature("some-custom-endpoint"), None);
    }
}
