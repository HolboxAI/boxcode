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

    /// An entry written before `env_keys`/`detail`/`at` existed must still
    /// load -- the same upgrade-safety rule `config.rs` follows for its tables.
    #[test]
    fn an_entry_from_an_older_build_still_loads() {
        let old = r#"{"date":"2026-01-01","project":"old","path":"/tmp/old","provider":"netlify","target":"Production","status":"Success"}"#;
        let entry: Deployment = serde_json::from_str(old).expect("an older record still loads");
        assert_eq!(entry.project, "old");
        assert!(entry.env_keys.is_empty());
        assert_eq!(entry.url, None);
        assert_eq!(entry.at, 0);
    }

    /// The one test that goes through the real file, catching what the pure
    /// ones cannot: a wrong path, or a `record` that silently writes nowhere.
    #[test]
    fn recording_appends_to_the_real_file() {
        with_isolated_home(|| {
            record(&entry("first", 1, "Success"));
            record(&entry("second", 2, "Failed"));

            let path = history_path().expect("a path under $HOME");
            let written = std::fs::read_to_string(&path).expect("the file exists");
            let lines: Vec<&str> = written.lines().collect();
            assert_eq!(lines.len(), 2, "one line per deployment: {written}");

            // Append, never overwrite: the first entry has to survive the
            // second, or the history only ever holds the last deployment.
            let first: Deployment = serde_json::from_str(lines[0]).unwrap();
            let second: Deployment = serde_json::from_str(lines[1]).unwrap();
            assert_eq!(first.project, "first");
            assert_eq!(second.project, "second");
        });
    }
}
