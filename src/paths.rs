//! Where this app keeps its state, and how it moved.
//!
//! v1.0.0 renamed the binary from `tuisample-code` to `boxcode`, which renames
//! the directory everything lives in: `~/.tuisample-code` → `~/.boxcode`. Two
//! things had to be true through that change, and this module exists to make
//! both of them true in one place rather than six:
//!
//! - **Nothing is lost.** Config, the usage log, today's quota counters, the
//!   deployment history, the anonymous device id and any embedded Python all
//!   come across on first run, without the user doing anything.
//! - **Nothing is mixed.** After the move there is exactly one directory being
//!   written to. Two live state directories would mean a usage log split in
//!   half, a quota that under-counts, and a second device id inflating the
//!   install count — all of which look like data corruption rather than a
//!   rename.
//!
//! The move is a `rename`, which on one filesystem is atomic: either the whole
//! directory is at the new path or it is still at the old one, never half. A
//! cross-device fallback copies instead and leaves the original alone, on the
//! principle that a duplicate is recoverable and a deletion is not.
//!
//! Environment variables get the same treatment from the other direction:
//! `BOXCODE_*` is the name now, `TUISAMPLE_*` still works, and [`env_var`] is
//! the only thing that needs to know that.

use std::path::PathBuf;

/// The directory state lives in, under `$HOME`.
pub const APP_DIR: &str = ".boxcode";
/// What it was called before v1.0.0.
pub const LEGACY_APP_DIR: &str = ".tuisample-code";

/// The current environment-variable prefix.
pub const ENV_PREFIX: &str = "BOXCODE";
/// The pre-1.0 prefix. Still honoured, deprecated.
pub const LEGACY_ENV_PREFIX: &str = "TUISAMPLE";

/// `$HOME`, or `%USERPROFILE%` on Windows.
pub fn home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

/// `~/.boxcode`. `None` only when there is no home directory to put it in.
pub fn state_dir() -> Option<PathBuf> {
    home().map(|home| home.join(APP_DIR))
}

/// `~/.tuisample-code`, for the migration and nothing else.
pub fn legacy_state_dir() -> Option<PathBuf> {
    home().map(|home| home.join(LEGACY_APP_DIR))
}

/// A file inside the state directory.
pub fn state_file(name: &str) -> Option<PathBuf> {
    state_dir().map(|dir| dir.join(name))
}

/// What happened when the app started up.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Migration {
    /// Everything was moved across in one go.
    Moved,
    /// The old directory could not be moved (a different filesystem), so it was
    /// copied. The original is deliberately left in place: a duplicate can be
    /// deleted later, a wrong deletion cannot be undone.
    Copied,
    /// A copy that did not fully succeed. Named separately because the right
    /// response is "go and look", not "carry on".
    Partial(String),
}

impl Migration {
    /// One line for the welcome screen. A silent migration is one nobody can
    /// verify, and this only ever appears once.
    pub fn notice(&self) -> String {
        match self {
            Migration::Moved => format!(
                "Renamed to boxcode: your settings and history moved from ~/{LEGACY_APP_DIR} to ~/{APP_DIR}."
            ),
            Migration::Copied => format!(
                "Renamed to boxcode: your settings and history were copied to ~/{APP_DIR}. \
                 The old ~/{LEGACY_APP_DIR} is untouched and can be deleted once you are happy."
            ),
            Migration::Partial(detail) => format!(
                "Renamed to boxcode, but copying ~/{LEGACY_APP_DIR} to ~/{APP_DIR} did not fully \
                 succeed: {detail}. Nothing was deleted — check both directories."
            ),
        }
    }
}

/// Bring pre-1.0 state across, once.
///
/// Returns `None` when there was nothing to do, which is every run after the
/// first and every run of a fresh install.
///
/// The guard is `new directory does not exist`: once `~/.boxcode` is there,
/// this never touches anything again. That is what stops a second copy from
/// merging a stale `usage.jsonl` back over a live one, or resurrecting an old
/// `device_id` alongside the current one.
pub fn migrate_legacy_state() -> Option<Migration> {
    let new = state_dir()?;
    let old = legacy_state_dir()?;

    if new.exists() || !old.is_dir() {
        return None;
    }
    // Not a directory we should be moving onto itself.
    if new == old {
        return None;
    }

    match std::fs::rename(&old, &new) {
        Ok(()) => Some(Migration::Moved),
        Err(_) => {
            // Almost always a cross-device link error. Copy instead, and leave
            // the original where it is.
            match copy_dir(&old, &new) {
                Ok(()) => Some(Migration::Copied),
                Err(e) => Some(Migration::Partial(e)),
            }
        }
    }
}

/// Recursive copy, used only by the cross-device fallback above.
fn copy_dir(from: &std::path::Path, to: &std::path::Path) -> Result<(), String> {
    std::fs::create_dir_all(to).map_err(|e| format!("could not create {}: {e}", to.display()))?;
    let entries = std::fs::read_dir(from)
        .map_err(|e| format!("could not read {}: {e}", from.display()))?;

    for entry in entries.flatten() {
        let source = entry.path();
        let target = to.join(entry.file_name());
        // `file_type` rather than `is_dir`, so a symlink is copied as a file
        // rather than followed into somewhere unexpected.
        let kind = entry
            .file_type()
            .map_err(|e| format!("could not inspect {}: {e}", source.display()))?;
        if kind.is_dir() {
            copy_dir(&source, &target)?;
        } else {
            std::fs::copy(&source, &target)
                .map_err(|e| format!("could not copy {}: {e}", source.display()))?;
        }
    }
    Ok(())
}

/// Read `BOXCODE_<suffix>`, falling back to the deprecated
/// `TUISAMPLE_<suffix>`.
///
/// Empty and whitespace-only values read as unset, matching what `config.rs`
/// already did: `export KEY=` is how people turn a setting off, and treating it
/// as a value produces an invalid header rather than a default.
///
/// The new name wins when both are set. Anyone who has exported both has
/// migrated one of them and forgotten the other, and the one they edited more
/// recently is overwhelmingly the new one.
pub fn env_var(suffix: &str) -> Option<String> {
    for prefix in [ENV_PREFIX, LEGACY_ENV_PREFIX] {
        if let Ok(value) = std::env::var(format!("{prefix}_{suffix}")) {
            if !value.trim().is_empty() {
                return Some(value);
            }
        }
    }
    None
}

/// Whether a deprecated `TUISAMPLE_*` variable is being relied on, so the app
/// can say so once rather than silently honouring a name that is going away.
pub fn legacy_env_vars_in_use() -> Vec<String> {
    let mut found: Vec<String> = std::env::vars()
        .map(|(key, _)| key)
        .filter(|key| key.starts_with(&format!("{LEGACY_ENV_PREFIX}_")))
        .filter(|key| {
            // Only the ones actually doing something: if the BOXCODE_ name is
            // also set, the old one is inert and not worth a warning.
            let suffix = key.trim_start_matches(&format!("{LEGACY_ENV_PREFIX}_"));
            std::env::var(format!("{ENV_PREFIX}_{suffix}"))
                .map(|v| v.trim().is_empty())
                .unwrap_or(true)
        })
        .collect();
    found.sort();
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::test_support::with_isolated_home;

    fn write(path: &std::path::Path, name: &str, contents: &str) {
        std::fs::create_dir_all(path).unwrap();
        std::fs::write(path.join(name), contents).unwrap();
    }

    /// The property the whole module exists for: upgrading must not lose a
    /// single file.
    #[test]
    fn a_first_run_brings_every_pre_1_0_file_across() {
        with_isolated_home(|| {
            let old = legacy_state_dir().unwrap();
            write(&old, "config.toml", "[llm]\nmodel = \"kept\"\n");
            write(&old, "usage.jsonl", "{\"date\":\"2026-01-01\"}\n");
            write(&old, "quota.json", "{\"requests\":7}");
            write(&old, "deployments.jsonl", "{\"project\":\"old-app\"}\n");
            write(&old, "device_id", "abc123");
            write(&old.join("python").join("bin"), "python3", "#!/bin/sh\n");

            assert_eq!(migrate_legacy_state(), Some(Migration::Moved));

            let new = state_dir().unwrap();
            for name in [
                "config.toml",
                "usage.jsonl",
                "quota.json",
                "deployments.jsonl",
                "device_id",
            ] {
                assert!(new.join(name).exists(), "{name} did not come across");
            }
            assert!(new.join("python/bin/python3").exists(), "nested files too");
            assert_eq!(
                std::fs::read_to_string(new.join("device_id")).unwrap(),
                "abc123",
                "contents must survive, not just the filenames"
            );
        });
    }

    /// The other half: after the move there is exactly one live directory.
    /// Two would split the usage log and double-count the install.
    #[test]
    fn nothing_is_left_behind_to_be_written_to_afterwards() {
        with_isolated_home(|| {
            write(&legacy_state_dir().unwrap(), "config.toml", "x");
            migrate_legacy_state();
            assert!(!legacy_state_dir().unwrap().exists(), "the old directory must be gone");
            assert!(state_dir().unwrap().exists());
        });
    }

    /// Running again must be a no-op. A second migration could merge a stale
    /// file back over a live one.
    #[test]
    fn migrating_is_done_exactly_once() {
        with_isolated_home(|| {
            write(&legacy_state_dir().unwrap(), "usage.jsonl", "old\n");
            assert!(migrate_legacy_state().is_some());

            // Simulate a later run that has written new data, plus a stale old
            // directory reappearing (a restored backup, a downgrade).
            std::fs::write(state_dir().unwrap().join("usage.jsonl"), "new\n").unwrap();
            write(&legacy_state_dir().unwrap(), "usage.jsonl", "stale\n");

            assert_eq!(migrate_legacy_state(), None, "must not run twice");
            assert_eq!(
                std::fs::read_to_string(state_dir().unwrap().join("usage.jsonl")).unwrap(),
                "new\n",
                "live data must not be overwritten by a stale copy"
            );
        });
    }

    #[test]
    fn a_fresh_install_has_nothing_to_migrate() {
        with_isolated_home(|| {
            assert_eq!(migrate_legacy_state(), None);
            assert!(!state_dir().unwrap().exists(), "and nothing is created eagerly");
        });
    }

    #[test]
    fn every_outcome_explains_itself_without_naming_a_file_path_wrongly() {
        for migration in [
            Migration::Moved,
            Migration::Copied,
            Migration::Partial("disk full".to_string()),
        ] {
            let notice = migration.notice();
            assert!(notice.contains(APP_DIR), "{notice}");
            assert!(notice.contains(LEGACY_APP_DIR), "{notice}");
        }
        assert!(Migration::Partial("disk full".to_string()).notice().contains("disk full"));
        // The two that leave the original in place must say so, or the user
        // deletes something they still need.
        assert!(Migration::Copied.notice().contains("untouched"));
        assert!(Migration::Partial("x".into()).notice().contains("Nothing was deleted"));
    }

    // ---- environment variables ---------------------------------------------

    /// Serialised against every other test that touches process-wide env.
    fn with_env<R>(pairs: &[(&str, Option<&str>)], f: impl FnOnce() -> R) -> R {
        let _guard = crate::config::test_support::HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let saved: Vec<(String, Option<String>)> = pairs
            .iter()
            .map(|(key, _)| ((*key).to_string(), std::env::var(key).ok()))
            .collect();
        for (key, value) in pairs {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
        let result = f();
        for (key, value) in saved {
            match value {
                Some(value) => std::env::set_var(&key, value),
                None => std::env::remove_var(&key),
            }
        }
        result
    }

    #[test]
    fn the_deprecated_prefix_still_works() {
        with_env(
            &[("BOXCODE_MODEL", None), ("TUISAMPLE_MODEL", Some("old-name"))],
            || assert_eq!(env_var("MODEL").as_deref(), Some("old-name")),
        );
    }

    #[test]
    fn the_new_prefix_wins_when_both_are_set() {
        with_env(
            &[
                ("BOXCODE_MODEL", Some("new-name")),
                ("TUISAMPLE_MODEL", Some("old-name")),
            ],
            || assert_eq!(env_var("MODEL").as_deref(), Some("new-name")),
        );
    }

    #[test]
    fn a_blank_value_reads_as_unset_under_either_prefix() {
        with_env(
            &[("BOXCODE_MODEL", Some("   ")), ("TUISAMPLE_MODEL", Some("fallback"))],
            || assert_eq!(env_var("MODEL").as_deref(), Some("fallback")),
        );
        with_env(
            &[("BOXCODE_MODEL", Some("")), ("TUISAMPLE_MODEL", Some(""))],
            || assert_eq!(env_var("MODEL"), None),
        );
    }

    /// Only a deprecated name that is actually doing something is worth
    /// mentioning; one shadowed by its replacement is inert.
    #[test]
    fn only_load_bearing_deprecated_names_are_reported() {
        with_env(
            &[
                ("TUISAMPLE_ENDPOINT", Some("https://x")),
                ("BOXCODE_ENDPOINT", None),
                ("TUISAMPLE_MODEL", Some("m")),
                ("BOXCODE_MODEL", Some("m")),
            ],
            || {
                let reported = legacy_env_vars_in_use();
                assert!(reported.contains(&"TUISAMPLE_ENDPOINT".to_string()), "{reported:?}");
                assert!(
                    !reported.contains(&"TUISAMPLE_MODEL".to_string()),
                    "a shadowed name is inert: {reported:?}"
                );
            },
        );
    }
}
