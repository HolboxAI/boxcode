//! `/rollback` -- put every file the model wrote this run back the way it
//! found it.
//!
//! The mechanism is a journal, not a diff replay and not a git operation. Each
//! time `write_file` or `edit_file` runs it hands back what the file held a
//! moment earlier -- content those tools already read in order to draw their
//! diff, so recording it costs a clone and nothing else. `/rollback` writes
//! those first-seen states back.
//!
//! Only the *first* record for a path is load-bearing. A file edited four
//! times has four before-states, three of which this run created itself;
//! restoring one of those would leave a half-undone file, which is worse than
//! either end of the range. So the earliest wins and the rest only count
//! towards "4 changes", which is the number worth showing.
//!
//! Two things this deliberately does not claim to undo:
//!
//! * **`run_command`.** A shell command can move, delete, generate or install
//!   anything, and no amount of reading the command string says which files it
//!   will touch. Rather than pretend, the journal records *that* commands ran
//!   and the confirmation names them, so the number on screen is never more
//!   confident than the mechanism behind it.
//! * **The user's own edits.** Nothing here touches a path the model never
//!   wrote to, and a file restored is restored to what the *model* found, so
//!   hand edits to other files survive untouched. A hand edit to a file the
//!   model also wrote is the one genuine collision, and it loses -- which is
//!   why `/rollback` asks first.
//!
//! Empty directories a write created are left behind. Removing them would mean
//! guessing which of the parents existed beforehand, and an empty directory is
//! never destructive; a wrongly-removed one could be.

use std::path::{Path, PathBuf};

/// The largest single file the journal will keep a copy of.
///
/// A cap on *memory*, never on the write itself: a file past this is still
/// written normally, it just cannot be offered as undoable. Sized so ordinary
/// source files -- which is everything an edit tool realistically touches --
/// are always covered, and a checked-in bundle or data blob is not.
pub const MAX_SNAPSHOT_BYTES: usize = 8 * 1024 * 1024;

/// The ceiling across the whole run, so a long session editing many large
/// files cannot quietly grow the process without bound.
pub const SNAPSHOT_BUDGET: usize = 64 * 1024 * 1024;

/// How many distinct shell commands the confirmation names before it stops
/// listing and just counts.
pub const SHELL_NAMES_SHOWN: usize = 5;

/// What a file held before one call touched it.
#[derive(Clone, Debug, PartialEq)]
pub enum Before {
    /// The file was not there. Undoing the call means deleting it.
    Absent,
    /// What the file held. Undoing means writing this back.
    Text(String),
    /// The file was there, but no copy was kept -- it was binary, too large,
    /// or the run had already spent its budget. Carries the sentence shown to
    /// the user, because "cannot undo this one" is only useful with the why.
    Unknown(String),
}

/// Everything one tool call leaves for `/rollback` to account for.
#[derive(Clone, Debug, PartialEq)]
pub enum Record {
    /// A file was written or edited.
    File {
        /// The path as the model asked for it -- what the transcript showed,
        /// so the confirmation names the same thing the approval did.
        display: String,
        /// Where it actually resolved to, which is what gets written back.
        path: PathBuf,
        before: Before,
    },
    /// A shell command ran. No paths, because there are none to be had.
    Shell { command: String },
}

/// Turn the read the write/edit tools already performed into a `Before`.
///
/// The three failure modes are told apart on purpose. "Not found" is a real,
/// exactly-undoable state (delete the file again). "Not valid UTF-8" is a
/// binary file being overwritten -- there *is* a before-state, this just is
/// not the tool that can hold it, and saying so beats offering an undo that
/// would corrupt it. Anything else is reported verbatim.
pub fn snapshot(read: std::io::Result<String>) -> Before {
    match read {
        Ok(text) if text.len() > MAX_SNAPSHOT_BYTES => Before::Unknown(format!(
            "it was {}, past the {} this keeps copies up to",
            bytes(text.len()),
            bytes(MAX_SNAPSHOT_BYTES)
        )),
        Ok(text) => Before::Text(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Before::Absent,
        Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
            Before::Unknown("it is not a text file, so no copy was kept".to_string())
        }
        Err(e) => Before::Unknown(format!("it could not be read first ({e})")),
    }
}

/// What `/rollback` will do to one file.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    /// Write this content back over what is there now.
    Restore(String),
    /// The file did not exist before the run. Delete it.
    Delete,
    /// Nothing can be done, and this is why.
    Blocked(String),
}

/// One file's entry in the plan, as shown in the confirmation and as executed.
#[derive(Clone, Debug, PartialEq)]
pub struct Step {
    pub display: String,
    pub path: PathBuf,
    /// How many calls touched this file. Shown, not used -- the undo is to the
    /// state before the first of them however many there were.
    pub touches: usize,
    pub action: Action,
}

impl Step {
    /// The one-line form the confirmation popup lists.
    pub fn label(&self) -> String {
        let times = match self.touches {
            1 => String::new(),
            n => format!(" ({n} changes)"),
        };
        match &self.action {
            Action::Restore(_) => format!("restore  {}{times}", self.display),
            Action::Delete => format!("delete   {}{times}  — created this session", self.display),
            Action::Blocked(why) => format!("keep     {} — cannot undo: {why}", self.display),
        }
    }

    /// True for the entries that will actually change something on disk, which
    /// is what the confirmation counts.
    pub fn is_actionable(&self) -> bool {
        !matches!(self.action, Action::Blocked(_))
    }
}

struct FileRecord {
    display: String,
    path: PathBuf,
    before: Before,
    touches: usize,
}

/// Everything this run has done that `/rollback` reasons about.
///
/// Lives for the process, not for the conversation: `/compact` rewrites the
/// context but changes nothing on disk, so a rollback window that closed when
/// the context did would be surprising in exactly the wrong direction. `/new`
/// clears it, because "forget what we discussed" reasonably includes "and stop
/// offering to undo it".
#[derive(Default)]
pub struct Journal {
    files: Vec<FileRecord>,
    shell: Vec<String>,
    held: usize,
}

impl Journal {
    pub fn record(&mut self, record: Record) {
        match record {
            Record::Shell { command } => {
                let command = command.trim().to_string();
                if !command.is_empty() && !self.shell.contains(&command) {
                    self.shell.push(command);
                }
            }
            Record::File {
                display,
                path,
                before,
            } => {
                // Second and later touches of the same file only raise the
                // count. Their before-states are ones this run created, and
                // restoring one would half-undo the file -- see the module
                // comment.
                if let Some(existing) = self.files.iter_mut().find(|f| f.path == path) {
                    existing.touches += 1;
                    return;
                }
                let before = match &before {
                    Before::Text(t) if self.held + t.len() > SNAPSHOT_BUDGET => {
                        Before::Unknown(format!(
                            "this run has already kept {} of undo history, its limit",
                            bytes(SNAPSHOT_BUDGET)
                        ))
                    }
                    _ => before,
                };
                if let Before::Text(t) = &before {
                    self.held += t.len();
                }
                self.files.push(FileRecord {
                    display,
                    path,
                    before,
                    touches: 1,
                });
            }
        }
    }

    /// What `/rollback` would do, in the order the files were first touched.
    ///
    /// Pure: it reads nothing off disk. Whether a file still differs from its
    /// before-state is only knowable at the moment of writing, so that
    /// question belongs to [`apply`], which answers it once rather than
    /// letting the preview and the execution disagree.
    pub fn plan(&self) -> Vec<Step> {
        self.files
            .iter()
            .map(|f| Step {
                display: f.display.clone(),
                path: f.path.clone(),
                touches: f.touches,
                action: match &f.before {
                    Before::Text(t) => Action::Restore(t.clone()),
                    Before::Absent => Action::Delete,
                    Before::Unknown(why) => Action::Blocked(why.clone()),
                },
            })
            .collect()
    }

    /// The commands whose effects the plan above does not cover, if any ran.
    pub fn shell_warning(&self) -> Option<String> {
        if self.shell.is_empty() {
            return None;
        }
        let shown: Vec<&str> = self
            .shell
            .iter()
            .take(SHELL_NAMES_SHOWN)
            .map(|c| c.as_str())
            .collect();
        let rest = self.shell.len().saturating_sub(shown.len());
        let tail = match rest {
            0 => String::new(),
            n => format!(", and {n} more"),
        };
        Some(format!(
            "{} shell command(s) also ran this session and are not undone by this: {}{tail}",
            self.shell.len(),
            shown.join("; ")
        ))
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    pub fn clear(&mut self) {
        self.files.clear();
        self.shell.clear();
        self.held = 0;
    }
}

/// What actually happened when the plan ran.
#[derive(Default, Debug, PartialEq)]
pub struct Report {
    pub restored: Vec<String>,
    pub deleted: Vec<String>,
    /// Already identical to its before-state -- counted separately so the
    /// summary never claims to have undone something that needed nothing.
    pub unchanged: Vec<String>,
    pub failed: Vec<(String, String)>,
    pub blocked: Vec<(String, String)>,
}

impl Report {
    pub fn summary(&self) -> String {
        let mut lines = Vec::new();
        let touched = self.restored.len() + self.deleted.len();
        lines.push(match touched {
            0 => "Rolled back nothing — every file was already as the session found it."
                .to_string(),
            _ => format!(
                "Rolled back {touched} file(s): {} restored, {} deleted.",
                self.restored.len(),
                self.deleted.len()
            ),
        });
        for name in &self.restored {
            lines.push(format!("  restored  {name}"));
        }
        for name in &self.deleted {
            lines.push(format!("  deleted   {name}"));
        }
        if !self.unchanged.is_empty() {
            lines.push(format!(
                "  {} file(s) already matched and were left alone.",
                self.unchanged.len()
            ));
        }
        for (name, why) in &self.blocked {
            lines.push(format!("  kept      {name} — {why}"));
        }
        for (name, why) in &self.failed {
            lines.push(format!("  FAILED    {name} — {why}"));
        }
        lines.join("\n")
    }

    /// The same news, written for the model rather than for the user.
    ///
    /// This has to reach the wire. The model has just been told, over several
    /// tool results, that it wrote these files; if the undo happens only in
    /// the transcript then its next edit is built on a picture of the disk
    /// that is no longer true. Naming the files is the whole point -- a vague
    /// "some changes were reverted" would leave it guessing which.
    pub fn notice(&self) -> String {
        let mut names: Vec<&str> = Vec::new();
        names.extend(self.restored.iter().map(|s| s.as_str()));
        names.extend(self.deleted.iter().map(|s| s.as_str()));
        if names.is_empty() {
            return "The user ran /rollback. Nothing changed on disk — every file was already \
                    as the session found it."
                .to_string();
        }
        format!(
            "The user ran /rollback: the following files have been put back to the state they \
             were in before this session, undoing your edits to them — {}. Do not assume any \
             earlier write or edit of yours still stands. Re-read any of these you need before \
             changing it again.",
            names.join(", ")
        )
    }
}

/// Run the plan. The only part of `/rollback` that touches the disk, called
/// from the event loop rather than from `App` -- the same division `plan.save`
/// and the usage log already follow.
///
/// A file that fails is reported and the rest still run: a permission error on
/// one path is no reason to leave the other nine half-rolled-back.
pub fn apply(steps: &[Step]) -> Report {
    let mut report = Report::default();
    for step in steps {
        match &step.action {
            Action::Blocked(why) => report.blocked.push((step.display.clone(), why.clone())),
            Action::Restore(before) => match restore(&step.path, before) {
                Ok(true) => report.restored.push(step.display.clone()),
                Ok(false) => report.unchanged.push(step.display.clone()),
                Err(e) => report.failed.push((step.display.clone(), e)),
            },
            Action::Delete => match delete(&step.path) {
                Ok(true) => report.deleted.push(step.display.clone()),
                Ok(false) => report.unchanged.push(step.display.clone()),
                Err(e) => report.failed.push((step.display.clone(), e)),
            },
        }
    }
    report
}

/// `Ok(true)` when the file was actually changed, `Ok(false)` when it already
/// held exactly this.
fn restore(path: &Path, before: &str) -> Result<bool, String> {
    if std::fs::read_to_string(path).is_ok_and(|now| now == before) {
        return Ok(false);
    }
    // A write recreates a parent the user has since removed; without this the
    // restore of a file inside a deleted directory fails for a reason that has
    // nothing to do with the file.
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
    }
    std::fs::write(path, before).map_err(|e| e.to_string())?;
    Ok(true)
}

/// `Ok(false)` when the file is already gone -- the end state this wanted, so
/// not a failure.
fn delete(path: &Path) -> Result<bool, String> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.to_string()),
    }
}

/// Byte counts as a human reads them. Only ever used in sentences explaining
/// why something was too big, so the rounding is deliberate.
fn bytes(n: usize) -> String {
    const MIB: usize = 1024 * 1024;
    const KIB: usize = 1024;
    if n >= MIB {
        format!("{:.0} MB", n as f64 / MIB as f64)
    } else if n >= KIB {
        format!("{:.0} kB", n as f64 / KIB as f64)
    } else {
        format!("{n} bytes")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wrote(display: &str, path: &Path, before: Before) -> Record {
        Record::File {
            display: display.to_string(),
            path: path.to_path_buf(),
            before,
        }
    }

    /// The core promise: a file the run created is deleted again, and a file
    /// it changed goes back to what it held.
    #[test]
    fn a_created_file_is_deleted_and_an_edited_one_is_restored() {
        let dir = tempfile::tempdir().unwrap();
        let made = dir.path().join("new.rs");
        let changed = dir.path().join("old.rs");
        std::fs::write(&changed, "after\n").unwrap();
        std::fs::write(&made, "brand new\n").unwrap();

        let mut journal = Journal::default();
        journal.record(wrote("new.rs", &made, Before::Absent));
        journal.record(wrote(
            "old.rs",
            &changed,
            Before::Text("before\n".to_string()),
        ));

        let report = apply(&journal.plan());
        assert!(!made.exists(), "the created file should be gone");
        assert_eq!(std::fs::read_to_string(&changed).unwrap(), "before\n");
        assert_eq!(report.deleted, vec!["new.rs".to_string()]);
        assert_eq!(report.restored, vec!["old.rs".to_string()]);
        assert!(report.failed.is_empty());
    }

    /// Four edits to one file undo to the state before the *first* of them,
    /// not to the state before the last -- the intermediate states are ones
    /// this run created, and stopping at one would leave a half-undone file.
    #[test]
    fn repeated_edits_undo_to_the_state_the_session_started_from() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.rs");
        std::fs::write(&path, "v4\n").unwrap();

        let mut journal = Journal::default();
        for v in ["v0", "v1", "v2", "v3"] {
            journal.record(wrote("a.rs", &path, Before::Text(format!("{v}\n"))));
        }

        let steps = journal.plan();
        assert_eq!(steps.len(), 1, "one file, one step, however many edits");
        assert_eq!(steps[0].touches, 4);
        apply(&steps);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "v0\n");
    }

    /// A file the user has since deleted by hand is simply recreated; the
    /// missing parent directory is not a failure either.
    #[test]
    fn restoring_recreates_a_file_and_its_parent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/deep/a.rs");

        let mut journal = Journal::default();
        journal.record(wrote(
            "nested/deep/a.rs",
            &path,
            Before::Text("original\n".to_string()),
        ));

        let report = apply(&journal.plan());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "original\n");
        assert_eq!(report.restored.len(), 1);
        assert!(report.failed.is_empty());
    }

    /// Already-correct files are reported apart from restored ones, so the
    /// summary never claims to have undone something that needed nothing.
    #[test]
    fn a_file_already_matching_is_counted_as_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.rs");
        std::fs::write(&path, "same\n").unwrap();

        let mut journal = Journal::default();
        journal.record(wrote("a.rs", &path, Before::Text("same\n".to_string())));

        let report = apply(&journal.plan());
        assert!(report.restored.is_empty());
        assert_eq!(report.unchanged, vec!["a.rs".to_string()]);
        assert!(report.summary().starts_with("Rolled back nothing"));
    }

    /// Deleting a file that is already gone is the end state that was wanted,
    /// so it is not an error.
    #[test]
    fn deleting_an_already_missing_file_is_not_a_failure() {
        let dir = tempfile::tempdir().unwrap();
        let mut journal = Journal::default();
        journal.record(wrote("gone.rs", &dir.path().join("gone.rs"), Before::Absent));

        let report = apply(&journal.plan());
        assert!(report.failed.is_empty());
        assert_eq!(report.unchanged, vec!["gone.rs".to_string()]);
    }

    /// A binary file gets no snapshot, and says so rather than being silently
    /// dropped from the list -- an undo that quietly skips a file is worse
    /// than one that admits it cannot.
    #[test]
    fn an_unsnapshottable_file_is_listed_as_blocked_not_hidden() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blob.bin");
        std::fs::write(&path, [0xff, 0xfe, 0x00]).unwrap();

        let before = snapshot(std::fs::read_to_string(&path));
        assert!(matches!(before, Before::Unknown(_)));

        let mut journal = Journal::default();
        journal.record(wrote("blob.bin", &path, before));
        let steps = journal.plan();
        assert_eq!(steps.len(), 1);
        assert!(!steps[0].is_actionable());

        let report = apply(&steps);
        assert_eq!(report.blocked.len(), 1);
        assert_eq!(
            std::fs::read(&path).unwrap(),
            [0xff, 0xfe, 0x00],
            "a file with no snapshot must be left exactly as it is"
        );
    }

    /// A missing file reads as `Absent`, which is a real undoable state --
    /// distinct from "could not be read", which is not.
    #[test]
    fn a_missing_file_snapshots_as_absent() {
        let dir = tempfile::tempdir().unwrap();
        let read = std::fs::read_to_string(dir.path().join("nope.rs"));
        assert_eq!(snapshot(read), Before::Absent);
    }

    /// The per-file cap keeps the journal bounded without affecting the write
    /// itself; the entry survives, saying why it cannot be undone.
    #[test]
    fn a_file_past_the_size_cap_is_recorded_as_unrecoverable() {
        let huge = "x".repeat(MAX_SNAPSHOT_BYTES + 1);
        assert!(matches!(snapshot(Ok(huge)), Before::Unknown(_)));
    }

    /// Shell commands are named, not counted into the file total -- the whole
    /// point is that the file list is not the whole story.
    #[test]
    fn shell_commands_are_warned_about_and_deduped() {
        let mut journal = Journal::default();
        assert!(journal.shell_warning().is_none());

        journal.record(Record::Shell {
            command: "npm install".to_string(),
        });
        journal.record(Record::Shell {
            command: "npm install".to_string(),
        });
        journal.record(Record::Shell {
            command: "rm -rf build".to_string(),
        });

        let warning = journal.shell_warning().expect("commands ran");
        assert!(warning.contains("npm install"));
        assert!(warning.contains("rm -rf build"));
        assert!(warning.starts_with("2 shell command"), "deduped: {warning}");
        assert!(
            journal.is_empty(),
            "a command is not a file; it must not make the plan look non-empty"
        );
    }

    /// The model has to be told which files moved under it, by name. A vague
    /// notice would leave it guessing, which is the failure this prevents.
    #[test]
    fn the_notice_to_the_model_names_every_file() {
        let report = Report {
            restored: vec!["src/main.rs".to_string()],
            deleted: vec!["src/api.rs".to_string()],
            ..Default::default()
        };
        let notice = report.notice();
        assert!(notice.contains("src/main.rs"));
        assert!(notice.contains("src/api.rs"));
        assert!(notice.contains("/rollback"));
    }

    /// One unwritable file must not abandon the others.
    #[test]
    fn a_failure_on_one_file_does_not_stop_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let good = dir.path().join("good.rs");
        std::fs::write(&good, "after\n").unwrap();

        let steps = vec![
            Step {
                display: "locked/a.rs".to_string(),
                // A path whose parent is a *file*, so creating it must fail.
                path: good.join("impossible.rs"),
                touches: 1,
                action: Action::Restore("x".to_string()),
            },
            Step {
                display: "good.rs".to_string(),
                path: good.clone(),
                touches: 1,
                action: Action::Restore("before\n".to_string()),
            },
        ];

        let report = apply(&steps);
        assert_eq!(report.failed.len(), 1);
        assert_eq!(report.restored, vec!["good.rs".to_string()]);
        assert_eq!(std::fs::read_to_string(&good).unwrap(), "before\n");
    }

    /// `/new` closes the window; nothing is left to undo afterwards.
    #[test]
    fn clearing_the_journal_empties_the_plan_and_the_warning() {
        let mut journal = Journal::default();
        journal.record(wrote(
            "a.rs",
            Path::new("/tmp/a.rs"),
            Before::Text("x".to_string()),
        ));
        journal.record(Record::Shell {
            command: "ls".to_string(),
        });

        journal.clear();
        assert!(journal.is_empty());
        assert!(journal.plan().is_empty());
        assert!(journal.shell_warning().is_none());
    }
}
