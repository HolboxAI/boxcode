//! An approved plan, as a file in the project.
//!
//! A plan is written to disk at exactly one moment: when the user approves it.
//! Nothing the model proposes reaches a file before that, and a revision is
//! not a revision until it has been approved too -- so whatever is on disk is
//! always something a human agreed to. That invariant is the whole reason the
//! file is worth trusting later, and it is why nothing in this module writes
//! outside `Plan::save`.
//!
//! Markdown with YAML-ish frontmatter, at the top of the project rather than
//! under `~/.boxcode`, because a plan is a thing people read, edit by hand,
//! commit, and review before the code exists. A plan nobody can find is a plan
//! nobody checks the work against.
//!
//! One file, at a fixed name, and no registry of past plans. The presence of
//! `plan.md` *is* the state: it is there, so it is the plan, and boxcode picks
//! it up. Nothing to list, nothing to select, nothing to remember having
//! archived. Deleting the file is how you finish with it.
//!
//! The frontmatter parser here is deliberately tiny: flat `key: value` scalars,
//! no nesting, no lists, no anchors. Everything structural (steps, non-goals)
//! lives in the markdown body where a person can edit it without knowing YAML,
//! and a real YAML dependency would buy nothing but a larger binary.

use crate::dateutil;
use std::path::{Path, PathBuf};

/// The plan, relative to the project root. A fixed, visible, obvious name:
/// someone who has never run boxcode should be able to open the project and
/// tell what this is.
pub const PLAN_FILE: &str = "plan.md";

/// One step of the plan, and whether it has been done.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Step {
    pub description: String,
    pub done: bool,
    /// Why this one could not be finished, when it could not be. Recorded
    /// rather than dropped: a plan that stops halfway is far more useful to
    /// come back to if it says where it stopped and why.
    pub blocked: Option<String>,
}

impl Step {
    pub fn new(description: impl Into<String>) -> Self {
        Self {
            description: description.into(),
            done: false,
            blocked: None,
        }
    }
}

/// How far along a plan is. Derived from the steps rather than stored, so the
/// two can never disagree -- a stored status is one more thing to forget to
/// update, and a plan whose header claims "done" over three unticked boxes is
/// worse than no status at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// Approved, nothing started.
    Approved,
    InProgress,
    Done,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Approved => "approved",
            Status::InProgress => "in-progress",
            Status::Done => "done",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    pub title: String,
    /// The approach, in prose. Markdown, shown to the user and kept in the
    /// file for whoever reads it later.
    pub summary: String,
    pub steps: Vec<Step>,
    /// What this plan deliberately does not cover. Cheap to write and worth a
    /// lot on the way back: it is the difference between "they forgot this"
    /// and "they decided against this".
    pub not_doing: Vec<String>,
    pub created: String,
    pub updated: String,
    /// The commit the plan was written against, when the project is a git
    /// repo. Never used to block anything -- only to warn, on resume, that the
    /// ground has moved since this was agreed.
    pub base_commit: Option<String>,
    pub model: String,
    /// Where this lives. Set on save and on load; not part of the file.
    pub path: PathBuf,
}

impl Plan {
    pub fn status(&self) -> Status {
        if self.steps.is_empty() || self.steps.iter().all(|s| s.done) {
            // An empty step list means the plan was approved as prose alone.
            // Treating that as "done" would be wrong, so it stays Approved.
            if self.steps.is_empty() {
                return Status::Approved;
            }
            return Status::Done;
        }
        if self.steps.iter().any(|s| s.done) {
            Status::InProgress
        } else {
            Status::Approved
        }
    }

    /// Steps finished, out of the total.
    pub fn progress(&self) -> (usize, usize) {
        (self.steps.iter().filter(|s| s.done).count(), self.steps.len())
    }

    pub fn is_finished(&self) -> bool {
        self.status() == Status::Done
    }

    /// The next step that still needs doing, 1-indexed for display.
    pub fn next_step(&self) -> Option<(usize, &Step)> {
        self.steps
            .iter()
            .enumerate()
            .find(|(_, s)| !s.done)
            .map(|(i, s)| (i + 1, s))
    }

    /// Mark a 1-indexed step done, or blocked with a reason.
    ///
    /// Returns the step's description on success so the caller can say what it
    /// just recorded, or an error naming the valid range -- which the model
    /// reads and corrects from, since an out-of-range step number is by far
    /// the most likely thing for it to get wrong here.
    pub fn mark(&mut self, step: usize, done: bool, note: Option<String>) -> Result<String, String> {
        if step == 0 || step > self.steps.len() {
            return Err(format!(
                "There is no step {step}. This plan has {} step{}, numbered 1 to {}.",
                self.steps.len(),
                if self.steps.len() == 1 { "" } else { "s" },
                self.steps.len()
            ));
        }
        let entry = &mut self.steps[step - 1];
        entry.done = done;
        entry.blocked = if done { None } else { note };
        self.updated = dateutil::today_string();
        Ok(entry.description.clone())
    }

    /// The plan as it appears on disk.
    pub fn render(&self) -> String {
        let mut out = String::from("---\n");
        // Titles are free text and could contain anything, so the one
        // character that would break a `key: value` line is quoted away.
        out.push_str(&format!("title: {}\n", scalar(&self.title)));
        out.push_str(&format!("status: {}\n", self.status().as_str()));
        out.push_str(&format!("created: {}\n", self.created));
        out.push_str(&format!("updated: {}\n", self.updated));
        if let Some(commit) = &self.base_commit {
            out.push_str(&format!("base_commit: {commit}\n"));
        }
        out.push_str(&format!("model: {}\n", scalar(&self.model)));
        out.push_str("---\n\n");

        out.push_str(&format!("# {}\n\n", self.title));
        if !self.summary.trim().is_empty() {
            out.push_str(self.summary.trim());
            out.push_str("\n\n");
        }

        if !self.steps.is_empty() {
            out.push_str("## Steps\n\n");
            for (i, step) in self.steps.iter().enumerate() {
                let box_ = if step.done { "x" } else { " " };
                out.push_str(&format!("- [{box_}] {}. {}\n", i + 1, step.description));
                if let Some(why) = &step.blocked {
                    out.push_str(&format!("      blocked: {why}\n"));
                }
            }
            out.push('\n');
        }

        if !self.not_doing.is_empty() {
            out.push_str("## Not doing\n\n");
            for item in &self.not_doing {
                out.push_str(&format!("- {item}\n"));
            }
            out.push('\n');
        }

        out
    }

    /// Read a plan back from the text of a file.
    ///
    /// Tolerant on purpose. These files are meant to be edited by hand, and a
    /// person who reworded a step or ticked a box in their editor must not be
    /// met with a parse error -- so anything unrecognised is skipped rather
    /// than rejected, and only a missing title is fatal.
    pub fn parse(text: &str, path: impl Into<PathBuf>) -> Result<Plan, String> {
        let path = path.into();
        let (front, body) = split_frontmatter(text);

        let mut plan = Plan {
            title: field(&front, "title").unwrap_or_default(),
            summary: String::new(),
            steps: Vec::new(),
            not_doing: Vec::new(),
            created: field(&front, "created").unwrap_or_else(dateutil::today_string),
            updated: field(&front, "updated").unwrap_or_else(dateutil::today_string),
            base_commit: field(&front, "base_commit"),
            model: field(&front, "model").unwrap_or_default(),
            path,
        };

        let mut section = Section::Summary;
        let mut summary = String::new();
        for line in body.lines() {
            let trimmed = line.trim();

            if let Some(heading) = trimmed.strip_prefix("## ") {
                section = match heading.trim().to_ascii_lowercase().as_str() {
                    "steps" => Section::Steps,
                    "not doing" => Section::NotDoing,
                    _ => Section::Other,
                };
                continue;
            }
            // The H1 repeats the title, and is the fallback when frontmatter
            // was stripped or hand-mangled.
            if let Some(h1) = trimmed.strip_prefix("# ") {
                if plan.title.is_empty() {
                    plan.title = h1.trim().to_string();
                }
                continue;
            }

            match section {
                Section::Summary => {
                    summary.push_str(line);
                    summary.push('\n');
                }
                Section::Steps => {
                    if let Some(step) = parse_step(trimmed) {
                        plan.steps.push(step);
                    } else if let Some(why) = trimmed.strip_prefix("blocked:") {
                        if let Some(last) = plan.steps.last_mut() {
                            last.blocked = Some(why.trim().to_string());
                        }
                    }
                }
                Section::NotDoing => {
                    if let Some(item) = trimmed.strip_prefix("- ") {
                        plan.not_doing.push(item.trim().to_string());
                    }
                }
                Section::Other => {}
            }
        }
        plan.summary = summary.trim().to_string();

        if plan.title.trim().is_empty() {
            return Err("this file has no title, so it is not a plan".to_string());
        }
        Ok(plan)
    }

    pub fn save(&self) -> Result<(), String> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("could not create {}: {e}", parent.display()))?;
        }
        std::fs::write(&self.path, self.render())
            .map_err(|e| format!("could not write {}: {e}", self.path.display()))
    }

    pub fn load(path: &Path) -> Result<Plan, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("could not read {}: {e}", path.display()))?;
        Plan::parse(&text, path)
    }

    /// The path relative to the project root, for display. Absolute paths in
    /// a transcript are noise -- the user knows which project they are in.
    pub fn display_path(&self, workspace: &Path) -> String {
        self.path
            .strip_prefix(workspace)
            .unwrap_or(&self.path)
            .display()
            .to_string()
    }
}

enum Section {
    Summary,
    Steps,
    NotDoing,
    Other,
}

/// `- [x] 3. Wrap the router` -> a done step. The number is display only; the
/// position in the list is what identifies a step, so a hand-edited file with
/// misnumbered entries still round-trips.
fn parse_step(line: &str) -> Option<Step> {
    let rest = line.strip_prefix("- [")?;
    let (mark, rest) = rest.split_at(rest.char_indices().nth(1)?.0);
    let rest = rest.strip_prefix("] ")?;
    let description = rest
        .split_once(". ")
        .filter(|(num, _)| num.chars().all(|c| c.is_ascii_digit()))
        .map(|(_, text)| text)
        .unwrap_or(rest);
    Some(Step {
        description: description.trim().to_string(),
        done: mark.eq_ignore_ascii_case("x"),
        blocked: None,
    })
}

fn split_frontmatter(text: &str) -> (String, String) {
    let text = text.trim_start_matches('\u{feff}');
    let Some(rest) = text.strip_prefix("---\n") else {
        return (String::new(), text.to_string());
    };
    match rest.split_once("\n---") {
        Some((front, body)) => (
            front.to_string(),
            body.trim_start_matches('\n').trim_start().to_string(),
        ),
        None => (String::new(), text.to_string()),
    }
}

fn field(front: &str, key: &str) -> Option<String> {
    front.lines().find_map(|line| {
        let (k, v) = line.split_once(':')?;
        (k.trim() == key).then(|| unscalar(v.trim()))
    })
}

/// Quote only when a bare value would not survive the round trip.
fn scalar(value: &str) -> String {
    let value = value.replace(['\n', '\r'], " ");
    if value.contains(": ") || value.starts_with(['"', '\'', '[', '{', '#', '-']) {
        format!("\"{}\"", value.replace('"', "\\\""))
    } else {
        value
    }
}

fn unscalar(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.len() >= 2 && trimmed.starts_with('"') && trimmed.ends_with('"') {
        return trimmed[1..trimmed.len() - 1].replace("\\\"", "\"");
    }
    trimmed.to_string()
}

/// Where the plan lives for this project.
pub fn path(workspace: &Path) -> PathBuf {
    workspace.join(PLAN_FILE)
}

/// The project's plan, if it has one.
///
/// `None` when there is no file, which is the ordinary case and not a problem.
/// `Some(Err)` when there is a `plan.md` that cannot be read as a plan -- worth
/// saying out loud rather than ignoring, since the user is entitled to think a
/// file by that name is being used.
pub fn open(workspace: &Path) -> Option<Result<Plan, String>> {
    let path = path(workspace);
    path.is_file().then(|| Plan::load(&path))
}

/// The short commit the project is currently on, if it is a git repo at all.
///
/// Best-effort and silent: a missing git, a detached worktree, or a repo with
/// no commits yet all just mean the plan carries no base commit, which costs
/// a staleness warning later and nothing else.
impl Plan {
    /// `(written_against, now)` when the project has moved since this plan was
    /// agreed, `None` when it hasn't or when there is nothing to compare.
    ///
    /// Only ever produces a warning. A plan agreed three weeks and forty
    /// commits ago may name files that have since moved, and a model told to
    /// follow it will do so confidently -- but refusing to carry on would be
    /// wrong just as often, since most of it is usually still right.
    pub fn stale_against(&self, workspace: &Path) -> Option<(String, String)> {
        let base = self.base_commit.clone()?;
        let head = head_commit(workspace)?;
        (base != head).then_some((base, head))
    }
}

pub fn head_commit(workspace: &Path) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .current_dir(workspace)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let sha = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!sha.is_empty()).then_some(sha)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Plan {
        Plan {
            title: "Rate limiting for the items API".to_string(),
            summary: "Fixed window, keyed by API key.".to_string(),
            steps: vec![
                Step::new("Add the limiter in src/rate_limit.py"),
                Step::new("Wrap the router in src/app.py"),
            ],
            not_doing: vec!["Distributed limiting — needs Redis".to_string()],
            created: "2026-08-11".to_string(),
            updated: "2026-08-11".to_string(),
            base_commit: Some("3c21dfb".to_string()),
            model: "deepseek-v4-flash".to_string(),
            path: PathBuf::from("/tmp/project/plan.md"),
        }
    }

    /// The file is the thing that outlives the session, so everything the plan
    /// knows has to survive a trip through it.
    #[test]
    fn a_plan_round_trips_through_its_file() {
        let original = sample();
        let parsed = Plan::parse(&original.render(), &original.path).expect("should parse");
        assert_eq!(parsed, original);
    }

    #[test]
    fn progress_survives_the_round_trip() {
        let mut plan = sample();
        plan.mark(1, true, None).unwrap();
        plan.mark(2, false, Some("waiting on the Redis decision".to_string()))
            .unwrap();

        let parsed = Plan::parse(&plan.render(), &plan.path).unwrap();
        assert!(parsed.steps[0].done);
        assert!(!parsed.steps[1].done);
        assert_eq!(
            parsed.steps[1].blocked.as_deref(),
            Some("waiting on the Redis decision")
        );
    }

    /// Status is derived, never stored, so it cannot drift from the boxes.
    #[test]
    fn status_follows_the_steps() {
        let mut plan = sample();
        assert_eq!(plan.status(), Status::Approved);
        assert_eq!(plan.progress(), (0, 2));

        plan.mark(1, true, None).unwrap();
        assert_eq!(plan.status(), Status::InProgress);

        plan.mark(2, true, None).unwrap();
        assert_eq!(plan.status(), Status::Done);
        assert_eq!(plan.progress(), (2, 2));

        // And a rendered "done" plan still reads as done after a round trip.
        let parsed = Plan::parse(&plan.render(), &plan.path).unwrap();
        assert_eq!(parsed.status(), Status::Done);
    }

    #[test]
    fn a_plan_with_no_steps_is_approved_rather_than_done() {
        let mut plan = sample();
        plan.steps.clear();
        assert_eq!(plan.status(), Status::Approved);
        assert!(!plan.is_finished());
    }

    #[test]
    fn marking_a_step_that_does_not_exist_says_what_the_range_is() {
        let mut plan = sample();
        let err = plan.mark(9, true, None).expect_err("there is no step 9");
        assert!(err.contains("2 steps"), "{err}");
        assert!(err.contains("1 to 2"), "{err}");
        assert!(plan.mark(0, true, None).is_err(), "steps are 1-indexed");
    }

    #[test]
    fn next_step_skips_what_is_already_done() {
        let mut plan = sample();
        plan.mark(1, true, None).unwrap();
        let (n, step) = plan.next_step().expect("one left");
        assert_eq!(n, 2);
        assert!(step.description.contains("router"));

        plan.mark(2, true, None).unwrap();
        assert_eq!(plan.next_step(), None);
    }

    /// These files are meant to be edited by hand. Someone ticking a box in
    /// their editor, renumbering, or reflowing a step must not produce a file
    /// boxcode then refuses to read.
    #[test]
    fn a_hand_edited_file_still_parses() {
        let text = "\
---
title: Hand written
created: 2026-08-01
---

# Hand written

Some prose about the approach.

## Steps

- [X] 1. First thing
- [ ] 7. Misnumbered but still the second step
- [ ] Not numbered at all

## Not doing

- Anything clever
";
        let plan = Plan::parse(text, "/tmp/p.md").expect("should parse");
        assert_eq!(plan.title, "Hand written");
        assert_eq!(plan.summary, "Some prose about the approach.");
        assert_eq!(plan.steps.len(), 3);
        assert!(plan.steps[0].done, "an uppercase X is still ticked");
        assert_eq!(plan.steps[1].description, "Misnumbered but still the second step");
        assert_eq!(plan.steps[2].description, "Not numbered at all");
        assert_eq!(plan.not_doing, vec!["Anything clever"]);
        assert_eq!(plan.status(), Status::InProgress);
    }

    #[test]
    fn a_file_that_is_not_a_plan_is_rejected_rather_than_half_read() {
        assert!(Plan::parse("just some notes\n", "/tmp/notes.md").is_err());
    }

    #[test]
    fn a_title_with_a_colon_survives_the_frontmatter() {
        let mut plan = sample();
        plan.title = "Auth: refresh tokens".to_string();
        let parsed = Plan::parse(&plan.render(), &plan.path).unwrap();
        assert_eq!(parsed.title, "Auth: refresh tokens");
    }

    /// The presence of the file is the whole state model: no file, no plan.
    #[test]
    fn a_project_with_no_plan_file_simply_has_no_plan() {
        let dir = tempfile::tempdir().unwrap();
        assert!(open(dir.path()).is_none());
    }

    #[test]
    fn the_projects_plan_is_read_back_from_its_fixed_name() {
        let dir = tempfile::tempdir().unwrap();
        let mut plan = sample();
        plan.path = path(dir.path());
        plan.mark(1, true, None).unwrap();
        plan.save().expect("should save");

        assert!(dir.path().join("plan.md").is_file(), "a plain, obvious name");

        let back = open(dir.path()).expect("there is a file").expect("it parses");
        assert_eq!(back.title, plan.title);
        assert_eq!(back.progress(), (1, 2));
    }

    /// A `plan.md` that is not a plan must be reported, not ignored: the user
    /// is entitled to assume a file by that name is being used.
    #[test]
    fn a_plan_file_that_cannot_be_read_is_reported_rather_than_skipped() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(path(dir.path()), "just some notes\n").unwrap();

        let result = open(dir.path()).expect("the file is there");
        assert!(result.is_err(), "it is not a plan, and saying nothing would mislead");
    }

    /// Warned about, never refused -- most of a plan is usually still right
    /// after the project moves.
    #[test]
    fn staleness_compares_the_recorded_commit_with_the_current_one() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(root)
                .output()
                .expect("git should run");
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "T"]);
        std::fs::write(root.join("a.txt"), "one").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "first"]);
        let head = head_commit(root).expect("a repo with a commit");

        let mut plan = sample();
        plan.base_commit = Some("0000000".to_string());
        let (base, now) = plan.stale_against(root).expect("the ground has moved");
        assert_eq!(base, "0000000");
        assert_eq!(now, head);

        plan.base_commit = Some(head);
        assert_eq!(plan.stale_against(root), None, "nothing has moved");

        // A plan with no recorded commit, or a project that is not a repo, has
        // nothing to compare and must not invent a warning.
        plan.base_commit = None;
        assert_eq!(plan.stale_against(root), None);
    }
}

