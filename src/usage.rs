//! Local, per-install usage history -- `~/.tuisample-code/usage.jsonl`, one
//! line per completed turn.
//!
//! This is the only usage record that exists anywhere. There is no login and
//! this app sends nothing about what any install actually asked the model to
//! do -- see `telemetry.rs` for the one thing that does leave the machine,
//! which is not this. A user's own history lives only on their own disk, in
//! a plain-text file they can read, `cat`, or delete themselves at any time.

use crate::dateutil;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::PathBuf;

#[derive(Serialize, Deserialize)]
struct Record {
    date: String,
    approx_tokens: usize,
    model: String,
}

fn usage_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    Some(PathBuf::from(home).join(".tuisample-code").join("usage.jsonl"))
}

/// Appends one record for a turn that streamed `approx_tokens`. A no-op for
/// a turn that streamed nothing (declined before any tokens arrived, or
/// failed instantly) -- that isn't usage -- and a no-op if `$HOME`/
/// `$USERPROFILE` can't be resolved, since there is nowhere to write to.
/// Every failure here is swallowed: a usage record is a nice-to-have, never
/// something worth interrupting a turn over.
pub fn record_turn(approx_tokens: usize, model: &str) {
    if approx_tokens == 0 {
        return;
    }
    let Some(path) = usage_path() else { return };
    let Some(parent) = path.parent() else { return };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    let record = Record {
        date: dateutil::today_string(),
        approx_tokens,
        model: model.to_string(),
    };
    let Ok(line) = serde_json::to_string(&record) else { return };
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(file, "{line}");
    }
}

/// What `/usage` shows.
#[derive(Default, Debug, PartialEq)]
pub struct Summary {
    pub today_tokens: usize,
    pub week_tokens: usize,
    pub all_time_tokens: usize,
    pub days_active: usize,
}

/// Reads and summarises the local history. A missing or corrupt file reads
/// as "no history yet" -- never a hard error, since this is a courtesy
/// readout, not a source of truth anything else depends on. Malformed
/// individual lines are skipped rather than failing the whole read, so one
/// bad line (a crash mid-write, a hand-edit) doesn't erase everything above
/// and below it.
pub fn summary() -> Summary {
    let Some(path) = usage_path() else { return Summary::default() };
    let Ok(content) = std::fs::read_to_string(&path) else { return Summary::default() };
    summarize(&content)
}

fn summarize(content: &str) -> Summary {
    let today = dateutil::today_string();
    let week_cutoff = dateutil::days_ago_string(7);
    let mut seen_dates = std::collections::HashSet::new();
    let mut summary = Summary::default();

    for line in content.lines() {
        let Ok(record) = serde_json::from_str::<Record>(line) else { continue };
        summary.all_time_tokens += record.approx_tokens;
        if record.date == today {
            summary.today_tokens += record.approx_tokens;
        }
        if record.date.as_str() >= week_cutoff.as_str() {
            summary.week_tokens += record.approx_tokens;
        }
        seen_dates.insert(record.date);
    }
    summary.days_active = seen_dates.len();
    summary
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(date: &str, tokens: usize) -> String {
        serde_json::to_string(&Record {
            date: date.to_string(),
            approx_tokens: tokens,
            model: "deepseek-v4-flash".to_string(),
        })
        .unwrap()
    }

    #[test]
    fn an_empty_history_summarises_to_all_zeroes() {
        assert_eq!(summarize(""), Summary::default());
    }

    #[test]
    fn a_malformed_line_is_skipped_not_fatal() {
        let today = dateutil::today_string();
        let content = format!("not json at all\n{}\n", line(&today, 100));
        let s = summarize(&content);
        assert_eq!(s.today_tokens, 100, "the one good line must still count");
    }

    #[test]
    fn today_and_all_time_and_days_active_are_tracked_separately() {
        let today = dateutil::today_string();
        let long_ago = "2020-01-01";
        let content = format!("{}\n{}\n{}\n", line(&today, 100), line(&today, 50), line(long_ago, 9000));

        let s = summarize(&content);
        assert_eq!(s.today_tokens, 150, "only today's two lines");
        assert_eq!(s.all_time_tokens, 9150, "every line, regardless of date");
        assert_eq!(s.days_active, 2, "two distinct dates seen");
    }

    #[test]
    fn week_tokens_excludes_anything_older_than_seven_days() {
        let today = dateutil::today_string();
        let within_week = dateutil::days_ago_string(3);
        let outside_week = dateutil::days_ago_string(10);
        let content = format!(
            "{}\n{}\n{}\n",
            line(&today, 10),
            line(&within_week, 20),
            line(&outside_week, 999)
        );

        let s = summarize(&content);
        assert_eq!(s.week_tokens, 30, "999 from 10 days ago must not count");
        assert_eq!(s.all_time_tokens, 1029, "but it still counts toward all-time");
    }

    /// Everything above tests `summarize()` against an in-memory string --
    /// this is the one test that actually goes through `usage_path()`,
    /// `record_turn()`'s file write, and `summary()`'s file read, to catch
    /// bugs the pure logic tests can't (a wrong path, a permissions issue,
    /// the two functions disagreeing about where the file lives).
    #[test]
    fn record_turn_and_summary_round_trip_through_the_real_file() {
        crate::config::test_support::with_isolated_home(|| {
            assert_eq!(summary(), Summary::default(), "a fresh $HOME has no history yet");

            record_turn(120, "deepseek-v4-flash");
            record_turn(80, "deepseek-v4-flash");
            // Zero-token turns (declined before anything streamed) must not
            // pollute the log -- see record_turn's early return.
            record_turn(0, "deepseek-v4-flash");

            let s = summary();
            assert_eq!(s.today_tokens, 200);
            assert_eq!(s.all_time_tokens, 200);
            assert_eq!(s.days_active, 1);
        });
    }
}
