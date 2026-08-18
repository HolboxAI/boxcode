//! Session persistence -- `~/.boxcode/sessions/<stamp>.jsonl`, one file per
//! conversation, one JSON line per message, plus a header line naming the
//! workspace it belongs to.
//!
//! The same shape as `usage.jsonl` and `deployments.jsonl`, for the same
//! reasons: append-only writes survive a Ctrl-C mid-session, and a plain file
//! the user can `cat` beats a database they cannot. Every failure in here is
//! swallowed -- a session record is an amenity, never something worth
//! interrupting a turn over. Reading is forgiving the same way: a line that
//! does not parse (an older format, a truncated final write) is skipped, not
//! fatal.
//!
//! One file is one *context*, not one launch. When the conversation shrinks --
//! `/new` discarding it, or a compaction replacing it with a summary -- the
//! log rotates to a fresh file rather than appending a contradiction to the
//! old one. The old file stays on disk untouched, which is what makes
//! `/resume` after an accidental `/new` possible at all, and it means a
//! resumed-from file is never appended to either: resuming copies the loaded
//! messages into a new file and continues there.

use crate::app::Message;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};

/// First line of every session file: which project this conversation belongs
/// to, so `latest_for` can tell this directory's sessions from every other's.
#[derive(Serialize, Deserialize)]
struct Header {
    workspace: String,
    date: String,
}

fn sessions_dir() -> Option<PathBuf> {
    // A guard, not a path: with no home directory there is nowhere to keep
    // state -- same stance as `usage::usage_path`.
    std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    Some(crate::config::Config::config_dir().join("sessions"))
}

/// Milliseconds since the epoch, as the filename. Sortable as a number, so
/// "newest session" is "biggest filename", and unique enough for a directory
/// written by one interactive app.
fn stamp() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// The live conversation's writer. Owns a cursor over how much of
/// `app.messages` is already on disk; `append` is called every event-loop
/// tick and is a length comparison in the common case.
pub struct SessionLog {
    workspace: String,
    /// `None` until there is something to write: a launch that never sends a
    /// prompt must not leave an empty file behind.
    path: Option<PathBuf>,
    persisted: usize,
}

impl SessionLog {
    pub fn new(workspace: &str) -> Self {
        Self {
            workspace: workspace.to_string(),
            path: None,
            persisted: 0,
        }
    }

    /// The conversation was replaced wholesale (`/new`, a compaction, a
    /// resume): stop appending to the old file and start a fresh one with
    /// whatever `append` sees next. `App` signals this explicitly (see
    /// `App::session_reset`) because it cannot be inferred from length alone
    /// -- a compacted conversation can come out the same length it went in.
    pub fn reset(&mut self) {
        self.path = None;
        self.persisted = 0;
    }

    /// Bring the file up to date with the conversation. New messages are
    /// appended; a conversation that *shrank* under us rotates to a fresh
    /// file as a belt-and-braces fallback for a missed `reset`.
    pub fn append(&mut self, messages: &[Message]) {
        if messages.len() < self.persisted {
            self.reset();
        }
        if messages.len() == self.persisted {
            return;
        }

        let path = match &self.path {
            Some(p) => p.clone(),
            None => {
                let Some(p) = self.create_file() else { return };
                p
            }
        };
        let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&path)
        else {
            return;
        };
        for message in &messages[self.persisted..] {
            let Ok(line) = serde_json::to_string(message) else { continue };
            if writeln!(file, "{line}").is_err() {
                return;
            }
        }
        self.persisted = messages.len();
    }

    fn create_file(&mut self) -> Option<PathBuf> {
        let dir = sessions_dir()?;
        std::fs::create_dir_all(&dir).ok()?;
        let path = dir.join(format!("{}.jsonl", stamp()));
        let header = Header {
            workspace: self.workspace.clone(),
            date: crate::dateutil::today_string(),
        };
        let line = serde_json::to_string(&header).ok()?;
        let mut file = std::fs::OpenOptions::new().create_new(true).write(true).open(&path).ok()?;
        writeln!(file, "{line}").ok()?;
        self.path = Some(path.clone());
        Some(path)
    }
}

/// The newest session file recorded for this workspace, if any.
pub fn latest_for(workspace: &str) -> Option<PathBuf> {
    let dir = sessions_dir()?;
    let entries = std::fs::read_dir(&dir).ok()?;
    let mut best: Option<(u128, PathBuf)> = None;
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_stem().and_then(|s| s.to_str()) else { continue };
        let Ok(n) = name.parse::<u128>() else { continue };
        // The header says whose session this is; a file for another project
        // -- or with no readable header at all -- is not a candidate.
        let Ok(content) = std::fs::read_to_string(&path) else { continue };
        let Some(first) = content.lines().next() else { continue };
        let Ok(header) = serde_json::from_str::<Header>(first) else { continue };
        if header.workspace != workspace {
            continue;
        }
        // A header with no messages after it is a session that never started.
        if content.lines().nth(1).is_none() {
            continue;
        }
        if best.as_ref().is_none_or(|(b, _)| n > *b) {
            best = Some((n, path));
        }
    }
    best.map(|(_, p)| p)
}

/// The messages recorded in one session file, in order. Unparseable lines --
/// the header, a truncated final write, an older format -- are skipped.
pub fn load(path: &Path) -> Vec<Message> {
    let Ok(content) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    content
        .lines()
        .filter_map(|line| serde_json::from_str::<Message>(line).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Role;

    fn msg(role: Role, content: &str) -> Message {
        Message::new(role, content)
    }

    #[test]
    fn a_conversation_round_trips_through_the_file() {
        crate::config::test_support::with_isolated_home(|| {
            let mut log = SessionLog::new("/tmp/proj");
            let messages = vec![
                msg(Role::User, "add a health check"),
                msg(Role::Assistant, "Done — added /health."),
            ];
            log.append(&messages);

            let latest = latest_for("/tmp/proj").expect("a session was recorded");
            let loaded = load(&latest);
            assert_eq!(loaded.len(), 2);
            assert!(loaded[0].role == Role::User);
            assert_eq!(loaded[0].content, "add a health check");
            assert_eq!(loaded[1].content, "Done — added /health.");
        });
    }

    /// A session recorded by a build that predates a field must still load.
    /// This is what makes `--resume` survive an upgrade instead of silently
    /// starting the conversation over.
    #[test]
    fn a_session_written_before_diffs_existed_still_loads() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("old.jsonl");
        std::fs::write(
            &path,
            "{\"role\":\"tool\",\"content\":\"Wrote 12 bytes\",\"display\":\"write a.rs\"}\n",
        )
        .expect("write");

        let loaded = load(&path);
        assert_eq!(loaded.len(), 1, "the line was dropped instead of loaded");
        assert!(loaded[0].role == Role::Tool);
        assert!(loaded[0].diff.is_none());
    }

    /// Appending is incremental: calling with the same list twice writes
    /// nothing new, and growing the list writes only the growth.
    #[test]
    fn append_writes_each_message_exactly_once() {
        crate::config::test_support::with_isolated_home(|| {
            let mut log = SessionLog::new("/tmp/proj");
            let mut messages = vec![msg(Role::User, "one")];
            log.append(&messages);
            log.append(&messages);
            messages.push(msg(Role::Assistant, "two"));
            log.append(&messages);

            let loaded = load(&latest_for("/tmp/proj").expect("recorded"));
            assert_eq!(loaded.len(), 2, "no duplicates from repeated appends");
        });
    }

    /// `/new` and compaction shrink the conversation; the log answers by
    /// starting a fresh file, leaving the old one intact to be resumed.
    #[test]
    fn a_shrunken_conversation_rotates_to_a_new_file() {
        crate::config::test_support::with_isolated_home(|| {
            let mut log = SessionLog::new("/tmp/proj");
            log.append(&[msg(Role::User, "the original conversation")]);
            let first = latest_for("/tmp/proj").expect("first session");

            // Compaction: the conversation is replaced by one summary
            // message, and App signals the replacement.
            std::thread::sleep(std::time::Duration::from_millis(2));
            log.reset();
            log.append(&[msg(Role::Summary, "it was about health checks")]);

            let second = latest_for("/tmp/proj").expect("second session");
            assert_ne!(first, second, "the summary went to a fresh file");
            assert_eq!(load(&first).len(), 1, "the original is untouched");
            assert!(load(&second)[0].role == Role::Summary);
        });
    }

    /// A launch that never says anything must leave nothing on disk, and
    /// other projects' sessions are never offered here.
    #[test]
    fn empty_sessions_and_other_workspaces_are_invisible() {
        crate::config::test_support::with_isolated_home(|| {
            let mut log = SessionLog::new("/tmp/proj");
            log.append(&[]);
            assert!(latest_for("/tmp/proj").is_none(), "nothing was said, nothing is kept");

            let mut other = SessionLog::new("/tmp/other");
            other.append(&[msg(Role::User, "hi")]);
            assert!(latest_for("/tmp/proj").is_none(), "another project's session");
            assert!(latest_for("/tmp/other").is_some());
        });
    }

    /// Garbage in the file -- a truncated line, an old format -- costs that
    /// line, not the session.
    #[test]
    fn unparseable_lines_are_skipped_not_fatal() {
        crate::config::test_support::with_isolated_home(|| {
            let mut log = SessionLog::new("/tmp/proj");
            log.append(&[msg(Role::User, "kept")]);
            let path = latest_for("/tmp/proj").expect("recorded");
            let mut content = std::fs::read_to_string(&path).unwrap();
            content.push_str("{\"role\":\"user\",\"content\":  TRUNCATED");
            std::fs::write(&path, content).unwrap();

            let loaded = load(&path);
            assert_eq!(loaded.len(), 1);
            assert_eq!(loaded[0].content, "kept");
        });
    }
}
