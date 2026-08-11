//! What was deployed, where, and whether it worked --
//! `~/.boxcode/deployments.jsonl`, one line per finished deployment.
//!
//! Deliberately the same shape as `usage.rs`: append-only JSONL in the same
//! directory, every failure swallowed, a corrupt line skipped rather than
//! fatal. A history file is a courtesy, never something a deployment should be
//! interrupted over.
//!
//! # What is not stored
//!
//! No tokens, no environment-variable values, no command output. That is
//! enforced by the shape of [`Deployment`] rather than by remembering to strip
//! things: there is no field a secret could go in, and the type that holds
//! secrets elsewhere ([`crate::deploy::Secret`]) is not `Serialize`, so adding
//! one later fails to compile rather than silently writing a token to disk.
//!
//! Environment variable *names* are recorded, and only names -- knowing that a
//! deployment carried `DATABASE_URL` is what makes the history useful for
//! diagnosis, and a name is not a secret.

use crate::dateutil;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::PathBuf;

/// One finished deployment.
///
/// Every field is deliberately plain text that could be shown to anyone with
/// access to the machine -- which is exactly who can read this file.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Deployment {
    /// UTC, `YYYY-MM-DD`, matching `usage.jsonl`.
    pub date: String,
    /// Seconds since the epoch, so same-day entries still sort.
    #[serde(default)]
    pub at: u64,
    pub project: String,
    /// Where on disk it was deployed from.
    pub path: String,
    pub provider: String,
    pub target: String,
    pub status: String,
    #[serde(default)]
    pub url: Option<String>,
    /// Names only. See the module doc.
    #[serde(default)]
    pub env_keys: Vec<String>,
    /// One line, when it failed. Already redacted on the way in.
    #[serde(default)]
    pub detail: Option<String>,
}

impl Deployment {
    /// A one-line summary for the `/deployments` readout.
    pub fn summary(&self) -> String {
        match &self.url {
            Some(url) => format!("{} → {url}", self.status),
            None => self.status.clone(),
        }
    }
}

fn history_path() -> Option<PathBuf> {
    // Via `config`, not derived here: that is the one place that inherits the
    // pre-rename directory, and a module that builds the path itself would
    // create `~/.boxcode` first and quietly strand the old one.
    std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    Some(crate::config::Config::config_dir().join("deployments.jsonl"))
}

/// Seconds since the epoch, or 0 if the clock is unreadable. Only ever used to
/// order entries within a day, so a bad clock costs ordering, not correctness.
pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Append one entry. Every failure is swallowed: see the module doc.
pub fn record(entry: &Deployment) {
    let Some(path) = history_path() else { return };
    let Some(parent) = path.parent() else { return };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    let Ok(line) = serde_json::to_string(entry) else {
        return;
    };
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(file, "{line}");
    }
}

/// The most recent `limit` deployments, newest first. A missing or unreadable
/// file reads as "no history yet".
pub fn recent(limit: usize) -> Vec<Deployment> {
    let Some(path) = history_path() else {
        return Vec::new();
    };
    let Ok(content) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    parse_recent(&content, limit)
}

/// Split out from `recent` so the ordering and skipping rules are testable
/// against a string, with no `$HOME` involved.
fn parse_recent(content: &str, limit: usize) -> Vec<Deployment> {
    let mut entries: Vec<Deployment> = content
        .lines()
        .filter_map(|line| serde_json::from_str::<Deployment>(line).ok())
        .collect();
    // Newest first. `at` is the tiebreaker within a day; file order breaks the
    // rest, which is already chronological because this file is append-only.
    entries.reverse();
    entries.sort_by_key(|entry| std::cmp::Reverse(entry.at));
    entries.truncate(limit);
    entries
}

/// Today's date, for a new entry.
pub fn today() -> String {
    dateutil::today_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::test_support::with_isolated_home;

    fn entry(project: &str, at: u64, status: &str) -> Deployment {
        Deployment {
            date: "2026-08-10".to_string(),
            at,
            project: project.to_string(),
            path: format!("/Users/dev/{project}"),
            provider: "vercel".to_string(),
            target: "Production".to_string(),
            status: status.to_string(),
            url: Some(format!("https://{project}.vercel.app")),
            env_keys: vec!["API_URL".to_string()],
            detail: None,
        }
    }

    /// The property this module exists to guarantee. `Deployment` has no field
    /// a secret could occupy, so this asserts the whole serialized form.
    #[test]
    fn a_recorded_deployment_contains_no_secret_material() {
        let json = serde_json::to_string(&entry("my-app", 1, "Success")).unwrap();
        for forbidden in ["token", "secret", "password", "VERCEL_TOKEN", "Bearer"] {
            assert!(
                !json.to_lowercase().contains(&forbidden.to_lowercase()),
                "history entry mentions {forbidden}: {json}"
            );
        }
        // Variable *names* are kept -- knowing a deployment carried API_URL is
        // what makes the history useful, and a name is not a secret.
        assert!(json.contains("API_URL"), "{json}");
    }

    #[test]
    fn entries_come_back_newest_first_and_capped() {
        let content = [entry("a", 100, "Success"), entry("b", 200, "Failed"), entry("c", 300, "Success")]
            .iter()
            .map(|e| serde_json::to_string(e).unwrap())
            .collect::<Vec<_>>()
            .join("\n");

        let recent = parse_recent(&content, 2);
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].project, "c");
        assert_eq!(recent[1].project, "b");
    }

    #[test]
    fn a_corrupt_line_is_skipped_rather_than_fatal() {
        let good = serde_json::to_string(&entry("kept", 1, "Success")).unwrap();
        let content = format!("not json at all\n{good}\n{{\"partial\": true}}\n");
        let recent = parse_recent(&content, 10);
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].project, "kept");
    }

    #[test]
    fn an_empty_history_is_empty_rather_than_an_error() {
        assert!(parse_recent("", 10).is_empty());
    }

    /// An entry written before `env_keys`/`detail`/`at` existed must still
    /// load -- the same upgrade-safety rule `config.rs` follows for its tables.
    #[test]
    fn an_entry_from_an_older_build_still_loads() {
        let old = r#"{"date":"2026-01-01","project":"old","path":"/tmp/old","provider":"netlify","target":"Production","status":"Success"}"#;
        let recent = parse_recent(old, 10);
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].project, "old");
        assert!(recent[0].env_keys.is_empty());
        assert_eq!(recent[0].url, None);
    }

    #[test]
    fn a_summary_names_the_url_when_there_is_one() {
        assert_eq!(
            entry("my-app", 1, "Success").summary(),
            "Success → https://my-app.vercel.app"
        );
        let mut failed = entry("my-app", 1, "Failed");
        failed.url = None;
        assert_eq!(failed.summary(), "Failed");
    }

    /// The one test that goes through the real file, catching the bugs the
    /// pure ones cannot: a wrong path, or `record` and `recent` disagreeing
    /// about where the file lives.
    #[test]
    fn recording_and_reading_round_trip_through_the_real_file() {
        with_isolated_home(|| {
            assert!(recent(10).is_empty(), "a fresh $HOME has no history");

            record(&entry("first", 1, "Success"));
            record(&entry("second", 2, "Failed"));

            let history = recent(10);
            assert_eq!(history.len(), 2);
            assert_eq!(history[0].project, "second");
            assert_eq!(history[1].project, "first");
        });
    }
}
