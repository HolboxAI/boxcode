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
    /// `root` must exist. A directory is used exactly as given -- this
    /// never walks up from a directory a caller already chose, so
    /// `workspace = "."` (the default) and any other directory keep
    /// meaning exactly that directory, nothing more.
    ///
    /// A file resolves to the project it belongs to, not an error: `/pull`
    /// relaunches into whatever local path `publish_artifact` was last
    /// given, and publishing a single file (not a directory) is a
    /// perfectly ordinary way to use it -- so the registry it reads back
    /// from can just as easily hand this a file as a directory. Walking up
    /// from the file for a `.git` boundary and rooting there, the same
    /// algorithm `git rev-parse --show-toplevel` and every CLI that shells
    /// out to it already use, handles the file having been published from
    /// deep inside a real project; a single fixed hop to the immediate
    /// parent would get that case silently wrong instead of loudly wrong.
    /// Falling back to the immediate parent only when no `.git` exists
    /// anywhere in the ancestry is deliberate too: most projects a
    /// developer publishes here never end up git-initialized at all, and
    /// walking all the way to the filesystem root with nothing found would
    /// land somewhere far broader than the file's own directory ever was.
    pub fn new(root: impl AsRef<Path>) -> Result<Self, String> {
        let given = root.as_ref();
        let canonical = given
            .canonicalize()
            .map_err(|e| format!("{} is unusable as a working directory: {e}", given.display()))?;
        let root = if canonical.is_dir() {
            canonical
        } else if canonical.is_file() {
            let parent = canonical
                .parent()
                .ok_or_else(|| format!("{} has no containing directory", canonical.display()))?;
            find_git_root(parent).unwrap_or_else(|| parent.to_path_buf())
        } else {
            return Err(format!("{} is neither a file nor a directory", canonical.display()));
        };
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

/// Walks up from `start` looking for a `.git` entry -- a directory in the
/// ordinary case, but git worktrees and submodules leave a `.git` *file*
/// there instead, pointing at the real one elsewhere, so this checks for
/// either rather than requiring a directory specifically. Returns the
/// first ancestor (inclusive of `start`) that has one; `None` once the
/// filesystem root is reached with nothing found.
fn find_git_root(start: &Path) -> Option<PathBuf> {
    let mut dir = start;
    loop {
        if dir.join(".git").exists() {
            return Some(dir.to_path_buf());
        }
        dir = dir.parent()?;
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

    /// Regression: this is the exact bug a real /pull session hit --
    /// publish_artifact was pointed at a lone file, so the local registry
    /// remembered that file's path, and /pull handed it straight back as
    /// the workspace root. Without a .git anywhere above it, resolving to
    /// its own containing directory is the correct, safe fallback -- not
    /// an error, and not a walk all the way to the filesystem root.
    #[test]
    fn a_file_with_no_git_ancestor_resolves_to_its_own_directory() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("todo.html");
        std::fs::write(&file, "<html></html>").unwrap();

        let ws = Workspace::new(&file).expect("should resolve, not refuse");
        assert_eq!(ws.root(), dir.path().canonicalize().unwrap());
    }

    /// The case a single fixed parent-directory hop would have gotten
    /// silently wrong: a file published from several levels inside a real
    /// git-tracked project must root at the actual project, matching what
    /// `git rev-parse --show-toplevel` would report from the same spot --
    /// not at whichever intermediate folder happens to be its direct
    /// parent (here, a build output directory two levels down).
    #[test]
    fn a_file_inside_a_git_project_resolves_to_the_git_root() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        std::fs::create_dir_all(dir.path().join("dist/assets")).unwrap();
        let file = dir.path().join("dist/assets/index.html");
        std::fs::write(&file, "<html></html>").unwrap();

        let ws = Workspace::new(&file).expect("should resolve");
        assert_eq!(ws.root(), dir.path().canonicalize().unwrap());
    }

    /// git worktrees and submodules leave `.git` as a *file*, not a
    /// directory -- confirming the boundary check does not require one.
    #[test]
    fn a_git_boundary_left_as_a_file_still_counts() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".git"), "gitdir: /elsewhere/.git/worktrees/x").unwrap();
        let file = dir.path().join("index.html");
        std::fs::write(&file, "<html></html>").unwrap();

        let ws = Workspace::new(&file).expect("should resolve");
        assert_eq!(ws.root(), dir.path().canonicalize().unwrap());
    }

    /// A directory is never walked up from, even when a `.git` sits above
    /// it -- only file resolution triggers the walk. Opening `workspace =
    /// "."` (the default) must keep meaning exactly that directory.
    #[test]
    fn a_directory_is_used_as_given_even_under_a_git_root() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        let sub = dir.path().join("subdir");
        std::fs::create_dir_all(&sub).unwrap();

        let ws = Workspace::new(&sub).expect("should open");
        assert_eq!(ws.root(), sub.canonicalize().unwrap());
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
