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
}
