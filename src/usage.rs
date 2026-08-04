//! Daily usage tracking and quota enforcement.
//!
//! Three things are metered per local calendar day: requests, tokens, and money.
//! They are deliberately not interchangeable -- a request count is exact against
//! every endpoint, a token count is only exact when the endpoint reports one, and
//! a dollar figure only exists for a model the user has priced. Rather than paper
//! over that with a single number, each is tracked and reported on its own terms,
//! including when it is an estimate or missing entirely.

use crate::config::QuotaConfig;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Token counts for a single request.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TokenUsage {
    pub prompt: u64,
    pub completion: u64,
    /// True when these came from counting characters locally rather than from
    /// the endpoint. Carried all the way to the UI so a spend figure built on
    /// guesswork is never displayed as though it were billed.
    pub estimated: bool,
}

impl TokenUsage {
    /// Rough token count for text, used only when the endpoint reports nothing.
    ///
    /// Four characters per token is the usual English rule of thumb. It is wrong
    /// for code and very wrong for CJK, which is exactly why anything derived
    /// from it is labelled an estimate rather than presented as a fact.
    pub fn estimate_from_chars(chars: usize) -> u64 {
        (chars as u64).div_ceil(4)
    }
}

/// What one model costs, in USD per million tokens. Supplied by the user in
/// config.toml; there is no built-in table, because a wrong price is worse than
/// an absent one -- it produces a confident number the user may act on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelPrice {
    #[serde(default)]
    pub input_per_mtok: f64,
    #[serde(default)]
    pub output_per_mtok: f64,
}

impl ModelPrice {
    pub fn cost_of(&self, usage: &TokenUsage) -> f64 {
        (usage.prompt as f64 / 1_000_000.0) * self.input_per_mtok
            + (usage.completion as f64 / 1_000_000.0) * self.output_per_mtok
    }
}

/// Counters for one local calendar day, persisted to
/// `~/.tuisample-code/usage.json`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct DailyUsage {
    /// Local date, `YYYY-MM-DD`. A mismatch with today is what triggers a reset,
    /// so the file stays readable and hand-correctable.
    #[serde(default)]
    pub date: String,
    #[serde(default)]
    pub requests: u64,
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
    #[serde(default)]
    pub usd: f64,
    /// Requests whose model had no price entry. Their tokens are counted but
    /// their cost is unknowable, so `usd` is an understatement whenever this is
    /// non-zero -- and the UI says so rather than implying the total is complete.
    #[serde(default)]
    pub unpriced_requests: u64,
    /// Set once any request in the day fell back to character estimation.
    #[serde(default)]
    pub any_estimated: bool,
    /// Set by `/quota override`, cleared by the daily rollover like everything
    /// else. Persisted so it survives a restart within the same day; a restart
    /// is not a decision to re-impose the limit.
    #[serde(default)]
    pub override_active: bool,
}

impl DailyUsage {
    pub fn total_tokens(&self) -> u64 {
        self.prompt_tokens.saturating_add(self.completion_tokens)
    }

    /// Zero the counters if the stored date is not `today`.
    pub fn roll_over(&mut self, today: &str) {
        if self.date != today {
            *self = Self {
                date: today.to_string(),
                ..Default::default()
            };
        }
    }

    /// Fold one request into the day's totals.
    ///
    /// `price` is `None` when the model has no entry in `[quota.pricing]`, which
    /// is tracked separately rather than counted as zero dollars.
    pub fn record(&mut self, usage: &TokenUsage, price: Option<ModelPrice>) {
        self.requests = self.requests.saturating_add(1);
        self.prompt_tokens = self.prompt_tokens.saturating_add(usage.prompt);
        self.completion_tokens = self.completion_tokens.saturating_add(usage.completion);
        if usage.estimated {
            self.any_estimated = true;
        }
        match price {
            Some(price) => self.usd += price.cost_of(usage),
            None => self.unpriced_requests = self.unpriced_requests.saturating_add(1),
        }
    }

    pub fn path() -> PathBuf {
        let home = std::env::var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("."));
        home.join(".tuisample-code").join("usage.json")
    }

    /// Read today's counters from disk.
    ///
    /// Every failure -- missing file, unreadable, corrupt JSON -- yields a fresh
    /// day rather than an error. Usage tracking is a guard rail, and a guard rail
    /// that refuses to let the app start is worse than one that loses a count.
    pub fn load(today: &str) -> Self {
        let mut usage = std::fs::read_to_string(Self::path())
            .ok()
            .and_then(|text| serde_json::from_str::<Self>(&text).ok())
            .unwrap_or_default();
        usage.roll_over(today);
        usage
    }

    /// Best-effort persist. Errors are returned so callers may surface them once,
    /// but no caller treats a failed write as fatal.
    pub fn save(&self) -> Result<(), Box<dyn std::error::Error>> {
        let path = Self::path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }
}

/// Whether another request may be sent, and what to tell the user.
#[derive(Clone, Debug, PartialEq)]
pub enum QuotaVerdict {
    /// Under every configured limit.
    Ok,
    /// Past `warn_at_percent` on at least one limit, but still allowed.
    Warn(String),
    /// A limit is spent. Carries the message shown in the transcript.
    Blocked(String),
}

/// Which limit tripped, so the message can name it.
fn describe(kind: &str, used: String, limit: String) -> String {
    format!("{kind} {used} of {limit}")
}

/// Evaluate today's totals against the configured limits.
///
/// A limit of zero means "no limit", so a default config tracks usage without
/// ever blocking anything -- upgrading must not suddenly stop someone's work.
pub fn evaluate(usage: &DailyUsage, quota: &QuotaConfig, resets_in: &str) -> QuotaVerdict {
    if !quota.enabled {
        return QuotaVerdict::Ok;
    }

    let mut exceeded: Vec<String> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();

    let mut check = |used: f64, limit: f64, text: String| {
        if limit <= 0.0 {
            return;
        }
        if used >= limit {
            exceeded.push(text);
        } else if used / limit * 100.0 >= quota.warn_at_percent as f64 {
            warnings.push(text);
        }
    };

    check(
        usage.requests as f64,
        quota.max_requests_per_day as f64,
        describe(
            "requests:",
            usage.requests.to_string(),
            quota.max_requests_per_day.to_string(),
        ),
    );
    check(
        usage.total_tokens() as f64,
        quota.max_tokens_per_day as f64,
        describe(
            "tokens:",
            format_tokens(usage.total_tokens()),
            format_tokens(quota.max_tokens_per_day),
        ),
    );
    check(
        usage.usd,
        quota.max_usd_per_day,
        describe(
            "spend:",
            format!("${:.2}", usage.usd),
            format!("${:.2}", quota.max_usd_per_day),
        ),
    );

    if !exceeded.is_empty() {
        // The override is checked here rather than earlier so that an overridden
        // day still computes its warnings and still knows it is over.
        if usage.override_active {
            return QuotaVerdict::Warn(format!(
                "Over quota ({}) — override active for today.",
                exceeded.join("; ")
            ));
        }
        return QuotaVerdict::Blocked(format!(
            "Daily quota reached — {}. Resets in {resets_in}.\nType /quota override to continue today, or raise the limit in ~/.tuisample-code/config.toml.",
            exceeded.join("; ")
        ));
    }
    if !warnings.is_empty() {
        return QuotaVerdict::Warn(format!("Approaching daily quota — {}.", warnings.join("; ")));
    }
    QuotaVerdict::Ok
}

/// `1234` -> `1.2k`, `1234567` -> `1.2M`. Keeps the header narrow enough to sit
/// beside the endpoint and model without pushing them off a normal terminal.
pub fn format_tokens(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        1_000..=999_999 => format!("{:.1}k", n as f64 / 1_000.0),
        _ => format!("{:.1}M", n as f64 / 1_000_000.0),
    }
}

/// One-line summary for the header, e.g. `12 req · 8.4k tok · $0.03`.
pub fn summary_line(usage: &DailyUsage) -> String {
    let mut parts = vec![
        format!("{} req", usage.requests),
        format!(
            "{}{} tok",
            if usage.any_estimated { "~" } else { "" },
            format_tokens(usage.total_tokens())
        ),
    ];
    // An unpriced model must not be reported as costing nothing, so the dollar
    // figure is only shown once at least one priced request exists, and is
    // marked with + whenever it is known to be incomplete.
    if usage.unpriced_requests < usage.requests {
        parts.push(format!(
            "${:.2}{}",
            usage.usd,
            if usage.unpriced_requests > 0 { "+" } else { "" }
        ));
    } else if usage.requests > 0 {
        parts.push("$ unpriced".to_string());
    }
    parts.join(" · ")
}

/// Local calendar date as `YYYY-MM-DD`.
pub fn today_local() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

/// Human-readable time until the next local midnight, e.g. `6h 12m`.
pub fn time_until_local_midnight() -> String {
    use chrono::Timelike;
    let now = chrono::Local::now();
    let secs_today = now.hour() as i64 * 3600 + now.minute() as i64 * 60 + now.second() as i64;
    let remaining = 86_400 - secs_today;
    let hours = remaining / 3600;
    let minutes = (remaining % 3600) / 60;
    if hours > 0 {
        format!("{hours}h {minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m")
    } else {
        "under a minute".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::test_support::with_isolated_home;
    use std::collections::HashMap;

    fn quota(requests: u64, tokens: u64, usd: f64) -> QuotaConfig {
        QuotaConfig {
            enabled: true,
            max_requests_per_day: requests,
            max_tokens_per_day: tokens,
            max_usd_per_day: usd,
            warn_at_percent: 80,
            include_usage: true,
            pricing: HashMap::new(),
        }
    }

    fn used(requests: u64, prompt: u64, completion: u64, usd: f64) -> DailyUsage {
        DailyUsage {
            date: "2026-08-04".to_string(),
            requests,
            prompt_tokens: prompt,
            completion_tokens: completion,
            usd,
            ..Default::default()
        }
    }

    #[test]
    fn a_default_config_never_blocks_anything() {
        // The upgrade-safety property: someone who has not opted into a limit
        // must never have a prompt refused because this feature shipped.
        let q = QuotaConfig::default();
        let heavy = used(10_000, 50_000_000, 50_000_000, 9_999.0);
        assert_eq!(evaluate(&heavy, &q, "1h"), QuotaVerdict::Ok);
    }

    #[test]
    fn a_zero_limit_means_unlimited_for_that_metric_alone() {
        // Requests capped, tokens and spend not: only the request limit can trip.
        // Enormous token and dollar figures must pass straight through.
        let q = quota(5, 0, 0.0);
        assert_eq!(evaluate(&used(1, 9_000_000, 0, 500.0), &q, "1h"), QuotaVerdict::Ok);
        assert!(matches!(
            evaluate(&used(5, 0, 0, 0.0), &q, "1h"),
            QuotaVerdict::Blocked(_)
        ));
    }

    #[test]
    fn each_metric_can_trip_the_quota_on_its_own() {
        let q = quota(100, 1000, 1.0);
        assert!(matches!(
            evaluate(&used(100, 0, 0, 0.0), &q, "1h"),
            QuotaVerdict::Blocked(_)
        ));
        assert!(matches!(
            evaluate(&used(1, 600, 400, 0.0), &q, "1h"),
            QuotaVerdict::Blocked(_)
        ));
        assert!(matches!(
            evaluate(&used(1, 0, 0, 1.0), &q, "1h"),
            QuotaVerdict::Blocked(_)
        ));
    }

    #[test]
    fn the_block_message_names_the_metric_and_the_reset_time() {
        let q = quota(10, 0, 0.0);
        match evaluate(&used(10, 0, 0, 0.0), &q, "6h 12m") {
            QuotaVerdict::Blocked(msg) => {
                assert!(msg.contains("requests: 10 of 10"), "{msg}");
                assert!(msg.contains("6h 12m"), "{msg}");
                assert!(msg.contains("/quota override"), "{msg}");
            }
            other => panic!("expected Blocked, got {other:?}"),
        }
    }

    #[test]
    fn warnings_start_at_the_configured_percentage() {
        let q = quota(10, 0, 0.0);
        assert_eq!(evaluate(&used(7, 0, 0, 0.0), &q, "1h"), QuotaVerdict::Ok);
        assert!(matches!(
            evaluate(&used(8, 0, 0, 0.0), &q, "1h"),
            QuotaVerdict::Warn(_)
        ));
    }

    #[test]
    fn an_override_downgrades_a_block_to_a_warning() {
        let q = quota(10, 0, 0.0);
        let mut over = used(11, 0, 0, 0.0);
        over.override_active = true;
        match evaluate(&over, &q, "1h") {
            QuotaVerdict::Warn(msg) => assert!(msg.contains("override active"), "{msg}"),
            other => panic!("expected Warn, got {other:?}"),
        }
    }

    #[test]
    fn disabling_the_quota_stops_all_enforcement() {
        let mut q = quota(1, 1, 0.01);
        q.enabled = false;
        assert_eq!(evaluate(&used(99, 99, 99, 99.0), &q, "1h"), QuotaVerdict::Ok);
    }

    #[test]
    fn a_new_day_clears_the_counters_and_the_override() {
        let mut usage = used(50, 1000, 1000, 5.0);
        usage.override_active = true;
        usage.roll_over("2026-08-05");
        assert_eq!(usage.date, "2026-08-05");
        assert_eq!(usage.requests, 0);
        assert_eq!(usage.total_tokens(), 0);
        assert_eq!(usage.usd, 0.0);
        // An override is a decision about *today*, not a standing exemption.
        assert!(!usage.override_active);
    }

    #[test]
    fn the_same_day_is_left_untouched() {
        let mut usage = used(50, 1000, 1000, 5.0);
        usage.roll_over("2026-08-04");
        assert_eq!(usage.requests, 50);
    }

    #[test]
    fn recording_accumulates_tokens_and_cost() {
        let mut usage = DailyUsage::default();
        let price = ModelPrice {
            input_per_mtok: 1.0,
            output_per_mtok: 2.0,
        };
        usage.record(
            &TokenUsage {
                prompt: 1_000_000,
                completion: 1_000_000,
                estimated: false,
            },
            Some(price),
        );
        assert_eq!(usage.requests, 1);
        assert_eq!(usage.total_tokens(), 2_000_000);
        assert!((usage.usd - 3.0).abs() < 1e-9, "{}", usage.usd);
        assert_eq!(usage.unpriced_requests, 0);
    }

    /// The honesty property: a model with no configured price must not silently
    /// contribute $0.00 to a spend total the user might trust.
    #[test]
    fn an_unpriced_model_is_counted_separately_rather_than_as_free() {
        let mut usage = DailyUsage::default();
        usage.record(
            &TokenUsage {
                prompt: 5_000_000,
                completion: 5_000_000,
                estimated: false,
            },
            None,
        );
        assert_eq!(usage.usd, 0.0);
        assert_eq!(usage.unpriced_requests, 1);
        // ...and the summary says so instead of printing $0.00.
        assert!(summary_line(&usage).contains("unpriced"), "{}", summary_line(&usage));
    }

    #[test]
    fn a_partly_priced_day_marks_the_total_as_incomplete() {
        let mut usage = DailyUsage::default();
        let price = ModelPrice {
            input_per_mtok: 1.0,
            output_per_mtok: 1.0,
        };
        usage.record(&TokenUsage { prompt: 1_000_000, completion: 0, estimated: false }, Some(price));
        usage.record(&TokenUsage { prompt: 1_000_000, completion: 0, estimated: false }, None);
        let line = summary_line(&usage);
        assert!(line.contains("$1.00+"), "incomplete totals must be marked: {line}");
    }

    #[test]
    fn estimated_token_counts_are_marked_in_the_summary() {
        let mut usage = DailyUsage::default();
        usage.record(&TokenUsage { prompt: 400, completion: 400, estimated: true }, None);
        assert!(usage.any_estimated);
        assert!(summary_line(&usage).contains("~800 tok"), "{}", summary_line(&usage));
    }

    #[test]
    fn character_estimation_rounds_up_so_short_text_is_never_free() {
        assert_eq!(TokenUsage::estimate_from_chars(0), 0);
        assert_eq!(TokenUsage::estimate_from_chars(1), 1);
        assert_eq!(TokenUsage::estimate_from_chars(4), 1);
        assert_eq!(TokenUsage::estimate_from_chars(5), 2);
    }

    #[test]
    fn token_counts_are_abbreviated_for_the_header() {
        assert_eq!(format_tokens(999), "999");
        assert_eq!(format_tokens(1_500), "1.5k");
        assert_eq!(format_tokens(2_500_000), "2.5M");
    }

    #[test]
    fn usage_survives_a_save_and_load_round_trip() {
        with_isolated_home(|| {
            let mut usage = used(7, 100, 200, 0.5);
            usage.override_active = true;
            usage.save().expect("save should succeed");

            let loaded = DailyUsage::load("2026-08-04");
            assert_eq!(loaded.requests, 7);
            assert_eq!(loaded.total_tokens(), 300);
            assert!(loaded.override_active);
        });
    }

    #[test]
    fn loading_on_a_later_day_returns_a_clean_slate() {
        with_isolated_home(|| {
            used(7, 100, 200, 0.5).save().unwrap();
            assert_eq!(DailyUsage::load("2026-09-01").requests, 0);
        });
    }

    /// A corrupt or truncated usage file must not stop the app from starting.
    #[test]
    fn a_corrupt_usage_file_degrades_to_a_fresh_day() {
        with_isolated_home(|| {
            let path = DailyUsage::path();
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, "{ this is not json").unwrap();

            let loaded = DailyUsage::load("2026-08-04");
            assert_eq!(loaded.requests, 0);
            assert_eq!(loaded.date, "2026-08-04");
        });
    }

    /// The persisted field names are a compatibility surface: renaming one makes
    /// every existing `usage.json` silently deserialize to zero, handing users
    /// back a full allowance mid-day. Pin them.
    #[test]
    fn the_on_disk_format_is_stable_and_readable() {
        let usage = DailyUsage {
            date: "2026-08-04".to_string(),
            requests: 12,
            prompt_tokens: 8_000,
            completion_tokens: 400,
            usd: 0.0342,
            unpriced_requests: 1,
            any_estimated: true,
            override_active: false,
        };
        let json = serde_json::to_string_pretty(&usage).unwrap();
        for field in [
            "date",
            "requests",
            "prompt_tokens",
            "completion_tokens",
            "usd",
            "unpriced_requests",
            "any_estimated",
            "override_active",
        ] {
            assert!(json.contains(&format!("\"{field}\"")), "missing {field} in {json}");
        }
        assert_eq!(serde_json::from_str::<DailyUsage>(&json).unwrap(), usage);
    }

    /// A file written by an older build lacks the newer keys and must still load
    /// with its counters intact rather than resetting the day.
    #[test]
    fn a_usage_file_missing_newer_fields_keeps_the_counts_it_has() {
        let older = r#"{"date":"2026-08-04","requests":9,"prompt_tokens":100,"completion_tokens":50}"#;
        let parsed: DailyUsage = serde_json::from_str(older).expect("must still parse");
        assert_eq!(parsed.requests, 9);
        assert_eq!(parsed.total_tokens(), 150);
        assert_eq!(parsed.usd, 0.0);
        assert!(!parsed.override_active);
    }

    #[test]
    fn a_missing_usage_file_is_not_an_error() {
        with_isolated_home(|| {
            assert!(!DailyUsage::path().exists());
            assert_eq!(DailyUsage::load("2026-08-04").requests, 0);
        });
    }

    #[test]
    fn saving_creates_the_parent_directory() {
        with_isolated_home(|| {
            assert!(!DailyUsage::path().parent().unwrap().exists());
            DailyUsage::default().save().expect("save should succeed");
            assert!(DailyUsage::path().exists());
        });
    }

    #[test]
    fn the_reset_countdown_is_always_a_sensible_duration() {
        let text = time_until_local_midnight();
        assert!(
            text.ends_with('m') || text == "under a minute",
            "unexpected countdown: {text}"
        );
    }
}
