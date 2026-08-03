//! The directory commands run in.
//!
//! This used to enforce confinement -- resolving paths, rejecting `..`, refusing
//! symlinks out, denying `.env` and key material. None of that survives a shell
//! tool: `cd /` walks straight past it, and no amount of inspecting a command
//! string can tell you which files it will touch. Keeping the checks would have
//! implied a boundary that is not there.
//!
//! So this is now exactly what it says: a working directory, and nothing is
//! claimed about where a command can reach from it. The real control is the user
//! approving each command (see `app.rs`).

use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct Workspace {
    root: PathBuf,
}

impl Workspace {
    /// `root` must exist and be a directory.
    pub fn new(root: impl AsRef<Path>) -> Result<Self, String> {
        let given = root.as_ref();
        let root = given
            .canonicalize()
            .map_err(|e| format!("{} is unusable as a working directory: {e}", given.display()))?;
        if !root.is_dir() {
            return Err(format!("{} is not a directory", root.display()));
        }
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// True when the working directory is somewhere broad enough to be worth
    /// warning about. Not a refusal -- with shell access it would be theatre,
    /// since a command can `cd` anywhere regardless -- but running the model's
    /// commands straight into `$HOME` or `/` is worth seeing on screen first.
    pub fn is_broad(&self) -> bool {
        if self.root.parent().is_none() {
            return true;
        }
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .and_then(|home| home.canonicalize().ok())
            .is_some_and(|home| home == self.root)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opens_an_existing_directory_and_canonicalizes_it() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::new(dir.path()).expect("should open");
        assert!(ws.root().is_absolute());
        assert!(ws.root().is_dir());
    }

    #[test]
    fn a_missing_directory_is_refused() {
        assert!(Workspace::new("/definitely/not/here/at/all").is_err());
    }

    #[test]
    fn a_file_is_not_a_working_directory() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "x").unwrap();
        assert!(Workspace::new(&file).is_err());
    }

    #[test]
    fn the_filesystem_root_counts_as_broad() {
        assert!(Workspace::new("/").expect("root exists").is_broad());
    }

    #[test]
    fn an_ordinary_project_directory_does_not() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!Workspace::new(dir.path()).unwrap().is_broad());
    }
}
