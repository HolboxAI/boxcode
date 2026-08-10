//! Optional daily spending limits -- `~/.boxcode/quota.json`.
//!
//! Distinct from `usage.rs`, which is the append-only history of what this
//! install has done and never refuses anything. This is the ceiling: a small
//! set of counters for the current day and the limits they are checked
//! against, so a runaway agentic loop cannot quietly spend a fortune while
//! nobody is watching.
//!
//! Three things are metered, and they are deliberately not interchangeable:
//!
//! - **Requests** are exact against every endpoint.
//! - **Tokens** are exact only when the endpoint reports them (see
//!   `llm::usage_of`); otherwise they fall back to the same character estimate
//!   `usage.rs` uses, and say so.
//! - **Money** exists only for a model the user has priced. There is no
//!   built-in price table: prices change without notice, differ per account,
//!   and do not exist at all for local models, so a confidently wrong dollar
//!   figure is worse than an absent one.
//!
//! Every limit defaults to zero, meaning no limit. Out of the box this counts
//! and reports; it starts refusing only once someone deliberately sets a
//! ceiling.

use crate::config::QuotaConfig;
use crate::dateutil;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Money is integer micro-dollars throughout. Never floats: `+=` on a float
/// accumulates representation error across thousands of requests, and this is a
/// budget.
pub const MICRO_PER_USD: u64 = 1_000_000;

/// What one request cost, in tokens.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TokenCount {
    pub prompt: u64,
    pub completion: u64,
    /// True when these came from counting characters rather than from the
    /// endpoint. Carried to the UI so an estimated spend figure is never shown
    /// as though it were billed.
    pub estimated: bool,
}

impl TokenCount {
    pub fn total(&self) -> u64 {
        self.prompt.saturating_add(self.completion)
    }
}

/// USD per million tokens, as the user supplies them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelPrice {
    #[serde(default)]
    pub input_per_mtok: f64,
    #[serde(default)]
    pub output_per_mtok: f64,
}

impl ModelPrice {
    /// Rounded **up**: undercharging by a fraction on every request is how a
    /// budget quietly overruns.
    pub fn micro_usd(&self, tokens: &TokenCount) -> u64 {
        let input = (tokens.prompt as f64 * self.input_per_mtok).ceil() as u64;
        let output = (tokens.completion as f64 * self.output_per_mtok).ceil() as u64;
        input.saturating_add(output)
    }
}

/// Counters for one UTC day.
///
/// UTC rather than local time, matching `dateutil` and the rest of this app: a
/// local-midnight reset would depend on a clock the user controls, and two
/// different day boundaries in one program is a bug waiting to be filed.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct DailyQuota {
    #[serde(default)]
    pub date: String,
    #[serde(default)]
    pub requests: u64,
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
    #[serde(default)]
    pub micro_usd: u64,
    /// Requests on a model with no configured price. Their tokens are counted
    /// but their cost is unknowable, so `micro_usd` understates the day
    /// whenever this is non-zero -- and the readout says so rather than
    /// implying the total is complete.
    #[serde(default)]
    pub unpriced_requests: u64,
    /// Set once any request in the day fell back to estimation.
    #[serde(default)]
    pub any_estimated: bool,
    /// Set by `/quota override`; cleared by the daily rollover like everything
    /// else, because an override is a decision about today.
    #[serde(default)]
    pub override_active: bool,
}

impl DailyQuota {
    pub fn total_tokens(&self) -> u64 {
        self.prompt_tokens.saturating_add(self.completion_tokens)
    }

    pub fn usd(&self) -> f64 {
        self.micro_usd as f64 / MICRO_PER_USD as f64
    }

    /// Zero the counters if the stored date is not `today`.
    pub fn roll_over(&mut self, today: &str) {
        if self.date != today {
            *self = Self { date: today.to_string(), ..Default::default() };
        }
    }

    /// Fold one finished request in. `price` is `None` when the model has no
    /// entry in `[quota.pricing]`, which is tracked separately rather than
    /// counted as zero dollars.
    pub fn record(&mut self, tokens: &TokenCount, price: Option<ModelPrice>) {
        self.requests = self.requests.saturating_add(1);
        self.prompt_tokens = self.prompt_tokens.saturating_add(tokens.prompt);
        self.completion_tokens = self.completion_tokens.saturating_add(tokens.completion);
        if tokens.estimated {
            self.any_estimated = true;
        }
        match price {
            Some(p) => self.micro_usd = self.micro_usd.saturating_add(p.micro_usd(tokens)),
            None => self.unpriced_requests = self.unpriced_requests.saturating_add(1),
        }
    }

    fn path() -> Option<PathBuf> {
        crate::paths::state_file("quota.json")
    }

    /// Today's counters. Every failure -- missing, unreadable, corrupt -- reads
    /// as a fresh day rather than an error: a guard rail that refuses to let
    /// the app start is worse than one that loses a count.
    pub fn load(today: &str) -> Self {
        let mut quota = Self::path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|text| serde_json::from_str::<Self>(&text).ok())
            .unwrap_or_default();
        quota.roll_over(today);
        quota
    }

    /// Best-effort persist, written after every request so a quota survives the
    /// Ctrl-C that ends most sessions.
    pub fn save(&self) {
        let Some(path) = Self::path() else { return };
        let Some(parent) = path.parent() else { return };
        if std::fs::create_dir_all(parent).is_err() {
            return;
        }
        if let Ok(text) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(&path, text);
        }
    }
}

/// Whether another request may be sent.
#[derive(Clone, Debug, PartialEq)]
pub enum Verdict {
    Ok,
    /// Past `warn_at_percent` on some limit, but still allowed.
    Warn(String),
    /// A limit is spent; carries the message shown in the transcript.
    Blocked(String),
}

/// Evaluate today's totals against the configured limits.
pub fn evaluate(quota: &DailyQuota, config: &QuotaConfig) -> Verdict {
    if !config.enabled {
        return Verdict::Ok;
    }

    let mut spent: Vec<String> = Vec::new();
    let mut close: Vec<String> = Vec::new();
    let mut check = |used: f64, limit: f64, text: String| {
        // A limit of zero means no limit, so a default config never blocks.
        if limit <= 0.0 {
            return;
        }
        if used >= limit {
            spent.push(text);
        } else if used / limit * 100.0 >= config.warn_at_percent as f64 {
            close.push(text);
        }
    };

    check(
        quota.requests as f64,
        config.max_requests_per_day as f64,
        format!("requests {} of {}", quota.requests, config.max_requests_per_day),
    );
    check(
        quota.total_tokens() as f64,
        config.max_tokens_per_day as f64,
        format!(
            "tokens {} of {}",
            quota.total_tokens(),
            config.max_tokens_per_day
        ),
    );
    check(
        quota.usd(),
        config.max_usd_per_day,
        format!("spend ${:.2} of ${:.2}", quota.usd(), config.max_usd_per_day),
    );

    if !spent.is_empty() {
        if quota.override_active {
            return Verdict::Warn(format!(
                "Over the daily limit ({}) — override active for today.",
                spent.join("; ")
            ));
        }
        return Verdict::Blocked(format!(
            "Daily limit reached — {}. Resets in {} (UTC midnight).\n\
             Type /quota override to keep going today, or raise the limit under [quota] in \
             ~/.boxcode/config.toml.",
            spent.join("; "),
            time_until_utc_midnight()
        ));
    }
    if !close.is_empty() {
        return Verdict::Warn(format!("Approaching the daily limit — {}.", close.join("; ")));
    }
    Verdict::Ok
}

/// Human-readable time until the next UTC midnight, e.g. `6h 12m`.
pub fn time_until_utc_midnight() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let remaining = 86_400 - (secs % 86_400);
    let (h, m) = (remaining / 3600, (remaining % 3600) / 60);
    if h > 0 {
        format!("{h}h {m}m")
    } else if m > 0 {
        format!("{m}m")
    } else {
        "under a minute".to_string()
    }
}

/// Today's date, for callers that should not need to know it is UTC.
pub fn today() -> String {
    dateutil::today_string()
}

/// Group digits so a five- or six-figure token count can be read at a glance.
pub fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// Money, at a precision a small daily budget can actually be read against.
///
/// Two decimals is the wrong unit below a dollar: against a $0.25/day ceiling a
/// real spend of `$0.0167` rounds to `$0.02`, which is too coarse to act on.
/// Below a dollar this keeps four, trimming trailing zeros so a round figure
/// still reads as `$0.25` rather than `$0.2500`.
pub fn format_usd(usd: f64) -> String {
    if usd >= 1.0 {
        return format!("${usd:.2}");
    }
    let text = format!("{usd:.4}");
    let trimmed = text.trim_end_matches('0');
    let trimmed = trimmed.strip_suffix('.').unwrap_or(trimmed);
    // Never fewer than two, though: "$0.2" reads as a typo rather than a price.
    if trimmed.split('.').nth(1).is_none_or(|d| d.len() < 2) {
        return format!("${usd:.2}");
    }
    format!("${trimmed}")
}

/// The user's own ceilings, counted on this machine.
pub fn describe(quota: &DailyQuota, config: &QuotaConfig) -> String {
    let limit = |used: String, limit: String, unlimited: bool| {
        if unlimited { format!("{used} — no limit set") } else { format!("{used} of {limit}") }
    };

    let mut lines = vec!["Your own limits (counted on this machine)".to_string()];
    lines.push(format!(
        "  Requests: {}",
        limit(
            thousands(quota.requests),
            thousands(config.max_requests_per_day),
            config.max_requests_per_day == 0
        )
    ));
    lines.push(format!(
        "  Tokens:   {}{}",
        limit(
            thousands(quota.total_tokens()),
            thousands(config.max_tokens_per_day),
            config.max_tokens_per_day == 0
        ),
        if quota.any_estimated {
            "  (estimated — this endpoint does not report exact counts)"
        } else {
            ""
        }
    ));

    lines.push(format!(
        "  Spend:    {}",
        limit(
            format!("${:.4}", quota.usd()),
            format!("${:.2}", config.max_usd_per_day),
            config.max_usd_per_day == 0.0
        )
    ));
    // Naming the gap matters more than the number: a total that silently
    // omits half the day's requests is worse than no total.
    if quota.unpriced_requests > 0 {
        lines.push(format!(
            "            excludes {} request(s) on a model with no price in [quota.pricing]",
            quota.unpriced_requests
        ));
    }

    if quota.override_active {
        lines.push("  Override: active for today".to_string());
    }
    // A readout that says "no limit set" three times should also say how to set
    // one, rather than leaving the user to find the config file.
    if !config.has_limits() {
        lines.push("  No limits of your own yet. Set one with:".to_string());
        lines.push("    /quota set requests 200  ·  /quota set tokens 500000  ·  /quota set usd 0.10".to_string());
    } else {
        lines.push("  /quota set <requests|tokens|usd> <n> to change · /quota clear to remove".to_string());
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(requests: u64, tokens: u64, usd: f64) -> QuotaConfig {
        QuotaConfig {
            enabled: true,
            max_requests_per_day: requests,
            max_tokens_per_day: tokens,
            max_usd_per_day: usd,
            warn_at_percent: 80,
            include_usage: true,
            pricing: std::collections::HashMap::new(),
        }
    }

    fn used(requests: u64, prompt: u64, completion: u64, micro: u64) -> DailyQuota {
        DailyQuota {
            date: "2026-08-06".to_string(),
            requests,
            prompt_tokens: prompt,
            completion_tokens: completion,
            micro_usd: micro,
            ..Default::default()
        }
    }

    /// The upgrade-safety property: someone who has not opted into a limit must
    /// never have a prompt refused because this feature shipped.
    #[test]
    fn a_default_config_never_blocks_anything() {
        let heavy = used(10_000, 50_000_000, 50_000_000, 9_999_000_000);
        assert_eq!(evaluate(&heavy, &QuotaConfig::default()), Verdict::Ok);
    }

    #[test]
    fn a_zero_limit_is_unlimited_for_that_metric_alone() {
        let c = cfg(5, 0, 0.0);
        assert_eq!(evaluate(&used(1, 9_000_000, 0, 500_000_000), &c), Verdict::Ok);
        assert!(matches!(evaluate(&used(5, 0, 0, 0), &c), Verdict::Blocked(_)));
    }

    #[test]
    fn each_metric_can_trip_the_limit_on_its_own() {
        let c = cfg(100, 1000, 1.0);
        assert!(matches!(evaluate(&used(100, 0, 0, 0), &c), Verdict::Blocked(_)));
        assert!(matches!(evaluate(&used(1, 600, 400, 0), &c), Verdict::Blocked(_)));
        assert!(matches!(evaluate(&used(1, 0, 0, 1_000_000), &c), Verdict::Blocked(_)));
    }

    #[test]
    fn the_block_message_names_the_metric_and_the_way_out() {
        match evaluate(&used(10, 0, 0, 0), &cfg(10, 0, 0.0)) {
            Verdict::Blocked(m) => {
                assert!(m.contains("requests 10 of 10"), "{m}");
                assert!(m.contains("/quota override"), "{m}");
                assert!(m.contains("UTC midnight"), "{m}");
            }
            other => panic!("expected Blocked, got {other:?}"),
        }
    }

    #[test]
    fn warnings_start_at_the_configured_percentage() {
        let c = cfg(10, 0, 0.0);
        assert_eq!(evaluate(&used(7, 0, 0, 0), &c), Verdict::Ok);
        assert!(matches!(evaluate(&used(8, 0, 0, 0), &c), Verdict::Warn(_)));
    }

    #[test]
    fn an_override_downgrades_a_block_to_a_warning() {
        let mut over = used(11, 0, 0, 0);
        over.override_active = true;
        assert!(matches!(evaluate(&over, &cfg(10, 0, 0.0)), Verdict::Warn(_)));
    }

    #[test]
    fn disabling_the_feature_stops_all_enforcement() {
        let mut c = cfg(1, 1, 0.01);
        c.enabled = false;
        assert_eq!(evaluate(&used(99, 99, 99, 99), &c), Verdict::Ok);
    }

    /// An override is a decision about *today*, not a standing exemption.
    #[test]
    fn a_new_day_clears_the_counters_and_the_override() {
        let mut q = used(50, 1000, 1000, 5_000_000);
        q.override_active = true;
        q.roll_over("2026-08-07");
        assert_eq!(q.requests, 0);
        assert_eq!(q.total_tokens(), 0);
        assert_eq!(q.micro_usd, 0);
        assert!(!q.override_active);
    }

    #[test]
    fn the_same_day_is_left_alone() {
        let mut q = used(50, 0, 0, 0);
        q.roll_over("2026-08-06");
        assert_eq!(q.requests, 50);
    }

    #[test]
    fn recording_accumulates_tokens_and_cost() {
        let mut q = DailyQuota::default();
        let price = ModelPrice { input_per_mtok: 1.0, output_per_mtok: 2.0 };
        q.record(&TokenCount { prompt: 1_000_000, completion: 1_000_000, estimated: false }, Some(price));
        assert_eq!(q.requests, 1);
        assert_eq!(q.total_tokens(), 2_000_000);
        assert_eq!(q.micro_usd, 3_000_000);
        assert!((q.usd() - 3.0).abs() < 1e-9);
    }

    /// The honesty property: a model with no configured price must not silently
    /// contribute $0.00 to a total someone might act on.
    #[test]
    fn an_unpriced_model_is_counted_separately_rather_than_as_free() {
        let mut q = DailyQuota::default();
        q.record(&TokenCount { prompt: 5_000_000, completion: 5_000_000, estimated: false }, None);
        assert_eq!(q.micro_usd, 0);
        assert_eq!(q.unpriced_requests, 1);
        assert!(describe(&q, &QuotaConfig::default()).contains("no price"));
    }

    #[test]
    fn estimated_counts_are_marked_in_the_readout() {
        let mut q = DailyQuota::default();
        q.record(&TokenCount { prompt: 100, completion: 100, estimated: true }, None);
        assert!(q.any_estimated);
        assert!(describe(&q, &QuotaConfig::default()).contains("estimated"));
    }

    /// Against a 25-cent-a-day ceiling, cents are the wrong unit.
    #[test]
    fn money_keeps_enough_precision_to_read_against_a_small_budget() {
        assert_eq!(format_usd(0.0167), "$0.0167");
        assert_eq!(format_usd(0.2333), "$0.2333");
        // ...but a round figure is not padded out with noise.
        assert_eq!(format_usd(0.25), "$0.25");
        assert_eq!(format_usd(0.1), "$0.10");
        assert_eq!(format_usd(0.0), "$0.00");
        assert_eq!(format_usd(1.5), "$1.50");
    }

    #[test]
    fn token_counts_are_grouped_for_reading() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(53_410), "53,410");
        assert_eq!(thousands(1_234_567), "1,234,567");
    }

    /// Integer micro-dollars exist so this holds exactly over many requests.
    #[test]
    fn cost_accumulates_without_drift() {
        let price = ModelPrice { input_per_mtok: 0.14, output_per_mtok: 0.28 };
        let tokens = TokenCount { prompt: 333, completion: 777, estimated: false };
        let mut q = DailyQuota::default();
        for _ in 0..10_000 {
            q.record(&tokens, Some(price));
        }
        assert_eq!(q.micro_usd, price.micro_usd(&tokens) * 10_000);
    }

    #[test]
    fn the_reset_countdown_is_always_a_sensible_duration() {
        let text = time_until_utc_midnight();
        assert!(text.ends_with('m') || text == "under a minute", "{text}");
    }

    #[test]
    fn an_unset_limit_reads_as_unset_rather_than_zero() {
        let out = describe(&DailyQuota::default(), &QuotaConfig::default());
        assert!(out.contains("no limit set"), "{out}");
    }
}
