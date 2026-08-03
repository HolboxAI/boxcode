//! What the model can actually do. Mirrors the `providers.rs` shape: a static
//! registry plus a dispatch function, rather than trait objects -- there is a
//! fixed, small set of tools and no reason to pay for dynamic dispatch.

pub mod fs;
pub mod search;
pub mod shell;

use crate::llm::ToolDef;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

/// Drives the permission gate. Not decoration: `ReadOnly` tools run unattended,
/// everything else has to be approved.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SideEffect {
    ReadOnly,
    Write,
    Execute,
}

pub struct ToolSpec {
    pub name: &'static str,
    pub description: &'static str,
    /// JSON Schema for the arguments object, built on demand. A function rather
    /// than a value because `serde_json::Value` cannot be built in a `const`.
    pub parameters: fn() -> serde_json::Value,
    pub side_effect: SideEffect,
}

pub const TOOLS: &[ToolSpec] = &[
    ToolSpec {
        name: "read_file",
        description: "Read a file from the workspace. Returns the contents with line numbers. \
                      Use offset/limit to page through a large file rather than reading it whole.",
        parameters: fs::read_file_schema,
        side_effect: SideEffect::ReadOnly,
    },
    ToolSpec {
        name: "list_dir",
        description: "List the entries of a directory in the workspace. Directories are suffixed with '/'.",
        parameters: fs::list_dir_schema,
        side_effect: SideEffect::ReadOnly,
    },
    ToolSpec {
        name: "glob",
        description: "Find files by glob pattern, e.g. 'src/**/*.rs'. Returns paths relative to the workspace.",
        parameters: search::glob_schema,
        side_effect: SideEffect::ReadOnly,
    },
    ToolSpec {
        name: "grep",
        description: "Search file contents with a regular expression. Returns matching lines as path:line:text.",
        parameters: search::grep_schema,
        side_effect: SideEffect::ReadOnly,
    },
    ToolSpec {
        name: "write_file",
        description: "Write a file, creating it and any missing parent directories, or overwriting it if it exists. \
                      Prefer edit_file for changing part of an existing file.",
        parameters: fs::write_file_schema,
        side_effect: SideEffect::Write,
    },
    ToolSpec {
        name: "edit_file",
        description: "Replace an exact string in a file. old_string must match the file byte for byte and must be \
                      unique unless replace_all is true. Include surrounding context to make it unique.",
        parameters: fs::edit_file_schema,
        side_effect: SideEffect::Write,
    },
    ToolSpec {
        name: "run_shell",
        description: "Run a shell command in the workspace root and return its output and exit status. \
                      Use this for builds, tests, git and gh.",
        parameters: shell::run_shell_schema,
        side_effect: SideEffect::Execute,
    },
];

pub fn find(name: &str) -> Option<&'static ToolSpec> {
    TOOLS.iter().find(|t| t.name == name)
}

/// The subset of the registry an agent is allowed to use, in the wire format the
/// model expects. Unknown names are skipped rather than panicking so a typo in an
/// agent definition degrades gracefully.
pub fn defs(allowed: &[&str]) -> Vec<ToolDef> {
    allowed
        .iter()
        .filter_map(|name| find(name))
        .map(|spec| ToolDef::function(spec.name, spec.description, (spec.parameters)()))
        .collect()
}

pub struct ToolCtx {
    /// Canonicalized workspace root. Every path a tool touches must resolve
    /// inside this.
    pub workspace: PathBuf,
    pub shell_timeout: Duration,
}

impl ToolCtx {
    pub fn new(workspace: PathBuf, shell_timeout: Duration) -> Self {
        // Canonicalize once so `resolve` can do a cheap prefix check. Falling
        // back to the raw path keeps a workspace that vanished mid-session from
        // panicking; `resolve` will simply reject everything.
        let workspace = workspace.canonicalize().unwrap_or(workspace);
        Self {
            workspace,
            shell_timeout,
        }
    }

    /// Display form of a path: relative to the workspace when it is inside,
    /// absolute otherwise.
    pub fn display(&self, path: &Path) -> String {
        path.strip_prefix(&self.workspace)
            .unwrap_or(path)
            .display()
            .to_string()
    }
}

/// A tool's result. `Err` is a message the *model* reads and reacts to -- a
/// missing file or a failing build is normal control flow, not a run failure.
#[derive(Clone, Debug, PartialEq)]
pub enum ToolOutcome {
    Ok(String),
    Err(String),
}

impl ToolOutcome {
    pub fn is_ok(&self) -> bool {
        matches!(self, ToolOutcome::Ok(_))
    }

    pub fn text(&self) -> &str {
        match self {
            ToolOutcome::Ok(s) | ToolOutcome::Err(s) => s,
        }
    }
}

pub async fn dispatch(name: &str, args: &serde_json::Value, ctx: &ToolCtx) -> ToolOutcome {
    match name {
        "read_file" => fs::read_file(args, ctx),
        "list_dir" => fs::list_dir(args, ctx),
        "write_file" => fs::write_file(args, ctx),
        "edit_file" => fs::edit_file(args, ctx),
        "glob" => search::glob(args, ctx),
        "grep" => search::grep(args, ctx),
        "run_shell" => shell::run_shell(args, ctx).await,
        other => ToolOutcome::Err(format!(
            "Unknown tool '{other}'. Available: {}",
            TOOLS
                .iter()
                .map(|t| t.name)
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

// ---- argument helpers ----------------------------------------------------------
// Models get argument types wrong often enough that every accessor has to fail
// with a sentence the model can act on, never with a panic.

pub fn arg_str<'a>(args: &'a serde_json::Value, key: &str) -> Result<&'a str, String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .ok_or_else(|| format!("missing required string argument '{key}'"))
}

pub fn opt_str<'a>(args: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

pub fn opt_usize(args: &serde_json::Value, key: &str) -> Option<usize> {
    let value = args.get(key)?;
    // Some models send numbers as strings.
    value
        .as_u64()
        .or_else(|| value.as_str()?.trim().parse().ok())
        .map(|n| n as usize)
}

pub fn opt_bool(args: &serde_json::Value, key: &str) -> bool {
    match args.get(key) {
        Some(serde_json::Value::Bool(b)) => *b,
        Some(serde_json::Value::String(s)) => s.eq_ignore_ascii_case("true"),
        _ => false,
    }
}

/// One-line rendering of a call, e.g. `read_file(src/app.rs)`. Used both in the
/// transcript and in the permission prompt, so the thing the user approves reads
/// exactly like the thing they later see in the timeline.
pub fn summarize(name: &str, args: &serde_json::Value) -> String {
    let field = |key: &str| opt_str(args, key).unwrap_or("?").to_string();
    let inner = match name {
        "read_file" | "write_file" | "edit_file" => field("path"),
        "list_dir" => opt_str(args, "path").unwrap_or(".").to_string(),
        "glob" => field("pattern"),
        "grep" => match opt_str(args, "path") {
            Some(path) => format!("{} in {path}", field("pattern")),
            None => field("pattern"),
        },
        "run_shell" => field("command"),
        _ => args.to_string(),
    };
    format!("{name}({})", clip(&inner, 120))
}

fn clip(s: &str, max: usize) -> String {
    let s = s.replace('\n', " ");
    if s.chars().count() <= max {
        return s;
    }
    let cut: String = s.chars().take(max).collect();
    format!("{cut}…")
}

// ---- path containment ----------------------------------------------------------

/// Resolve a model-supplied path against the workspace and refuse anything that
/// escapes it.
///
/// Lexical normalization alone is not enough (a symlink inside the workspace can
/// point outside it) and canonicalization alone is not enough (it fails on a file
/// that does not exist yet, which `write_file` needs). So: normalize lexically,
/// canonicalize the deepest ancestor that exists, then re-attach the missing tail
/// and check containment on the result.
pub fn resolve(ctx: &ToolCtx, raw: &str) -> Result<PathBuf, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("path must not be empty".to_string());
    }

    let joined = if Path::new(raw).is_absolute() {
        PathBuf::from(raw)
    } else {
        ctx.workspace.join(raw)
    };
    let lexical = normalize(&joined);

    let mut probe = lexical.as_path();
    let mut tail: Vec<&std::ffi::OsStr> = Vec::new();
    let real = loop {
        match probe.canonicalize() {
            Ok(real) => break real,
            Err(_) => match (probe.file_name(), probe.parent()) {
                (Some(name), Some(parent)) => {
                    tail.push(name);
                    probe = parent;
                }
                _ => return Err(format!("cannot resolve path '{raw}'")),
            },
        }
    };

    let mut full = real;
    for part in tail.iter().rev() {
        full.push(part);
    }

    if !full.starts_with(&ctx.workspace) {
        return Err(format!(
            "'{raw}' resolves outside the workspace ({}); refusing to touch it",
            ctx.workspace.display()
        ));
    }
    Ok(full)
}

/// Collapse `.` and `..` without consulting the filesystem.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

// ---- output caps ---------------------------------------------------------------

pub const MAX_OUTPUT_LINES: usize = 2000;
pub const MAX_OUTPUT_BYTES: usize = 100 * 1024;

/// Truncate tool output so one `read_file` on a generated file cannot swallow the
/// whole context window. The marker matters as much as the cut: without it the
/// model reasons confidently about a file it only half saw.
pub fn cap(text: &str) -> String {
    let mut out = text.to_string();

    let total_lines = out.lines().count();
    if total_lines > MAX_OUTPUT_LINES {
        let kept: Vec<&str> = out.lines().take(MAX_OUTPUT_LINES).collect();
        out = format!(
            "{}\n[truncated: {} more lines]",
            kept.join("\n"),
            total_lines - MAX_OUTPUT_LINES
        );
    }

    if out.len() > MAX_OUTPUT_BYTES {
        // Cut on a char boundary; the byte limit is approximate by design.
        let mut end = MAX_OUTPUT_BYTES;
        while end > 0 && !out.is_char_boundary(end) {
            end -= 1;
        }
        let dropped = out.len() - end;
        out.truncate(end);
        out.push_str(&format!("\n[truncated: {dropped} more bytes]"));
    }

    out
}

/// Directories never worth walking into. Skipping these is the difference between
/// a `glob` that answers instantly and one that walks a 40k-file `target/`.
pub const SKIP_DIRS: &[&str] = &[".git", "target", "node_modules", ".venv", "dist", "build"];

pub fn is_skipped(path: &Path, workspace: &Path) -> bool {
    path.strip_prefix(workspace)
        .unwrap_or(path)
        .components()
        .any(|c| SKIP_DIRS.contains(&c.as_os_str().to_string_lossy().as_ref()))
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    /// A `ToolCtx` rooted at a fresh temp dir. The `TempDir` is returned too --
    /// dropping it deletes the workspace, so tests must hold on to it.
    pub(crate) fn ctx() -> (tempfile::TempDir, ToolCtx) {
        let dir = tempfile::tempdir().expect("failed to create temp workspace");
        let ctx = ToolCtx::new(dir.path().to_path_buf(), Duration::from_secs(30));
        (dir, ctx)
    }

    pub(crate) fn write(ctx: &ToolCtx, rel: &str, contents: &str) {
        let path = ctx.workspace.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, contents).unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;

    #[test]
    fn every_tool_has_a_unique_name_and_an_object_schema() {
        let mut seen = std::collections::HashSet::new();
        for tool in TOOLS {
            assert!(seen.insert(tool.name), "duplicate tool name {}", tool.name);
            let schema = (tool.parameters)();
            assert_eq!(schema["type"], "object", "{} schema", tool.name);
            assert!(
                schema.get("properties").is_some(),
                "{} schema has no properties",
                tool.name
            );
        }
    }

    #[test]
    fn defs_filters_to_the_allowed_subset_and_ignores_unknown_names() {
        let defs = defs(&["read_file", "not_a_tool", "run_shell"]);
        let names: Vec<&str> = defs.iter().map(|d| d.function.name.as_str()).collect();
        assert_eq!(names, vec!["read_file", "run_shell"]);
    }

    #[test]
    fn resolve_accepts_paths_inside_the_workspace() {
        let (_dir, ctx) = ctx();
        write(&ctx, "src/app.rs", "fn main() {}");

        let path = resolve(&ctx, "src/app.rs").unwrap();
        assert!(path.starts_with(&ctx.workspace));
        assert_eq!(ctx.display(&path), "src/app.rs");
    }

    #[test]
    fn resolve_accepts_a_file_that_does_not_exist_yet() {
        let (_dir, ctx) = ctx();
        // write_file depends on this: the target is missing by definition.
        let path = resolve(&ctx, "src/brand/new.rs").unwrap();
        assert!(path.starts_with(&ctx.workspace));
        assert!(!path.exists());
    }

    #[test]
    fn resolve_rejects_traversal_out_of_the_workspace() {
        let (_dir, ctx) = ctx();
        for attempt in ["../escape.txt", "src/../../escape.txt", "/etc/passwd"] {
            let result = resolve(&ctx, attempt);
            assert!(result.is_err(), "{attempt} should have been rejected");
            assert!(result.unwrap_err().contains("outside the workspace"));
        }
    }

    #[test]
    fn resolve_rejects_an_empty_path() {
        let (_dir, ctx) = ctx();
        assert!(resolve(&ctx, "   ").is_err());
    }

    /// A symlink inside the workspace pointing out of it is the case lexical
    /// normalization alone would wave through.
    #[cfg(unix)]
    #[test]
    fn resolve_follows_symlinks_before_deciding_containment() {
        let (_dir, ctx) = ctx();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), "shh").unwrap();
        std::os::unix::fs::symlink(outside.path(), ctx.workspace.join("link")).unwrap();

        let result = resolve(&ctx, "link/secret.txt");
        assert!(result.is_err(), "symlink escape should be rejected");
        assert!(result.unwrap_err().contains("outside the workspace"));
    }

    #[test]
    fn cap_truncates_by_line_count_and_says_how_much_was_dropped() {
        let text = (0..MAX_OUTPUT_LINES + 50)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let capped = cap(&text);
        assert!(capped.contains("[truncated: 50 more lines]"));
        assert!(capped.lines().count() <= MAX_OUTPUT_LINES + 1);
    }

    #[test]
    fn cap_truncates_by_byte_size_on_a_char_boundary() {
        // One long line: under the line cap, far over the byte cap, multi-byte.
        let text = "é".repeat(MAX_OUTPUT_BYTES);
        let capped = cap(&text);
        assert!(capped.contains("more bytes]"));
        assert!(capped.len() < text.len());
    }

    #[test]
    fn cap_leaves_small_output_untouched() {
        assert_eq!(cap("hello\nworld"), "hello\nworld");
    }

    #[test]
    fn is_skipped_matches_noisy_directories_at_any_depth() {
        let root = Path::new("/w");
        assert!(is_skipped(Path::new("/w/target/debug/x"), root));
        assert!(is_skipped(Path::new("/w/a/node_modules/b"), root));
        assert!(!is_skipped(Path::new("/w/src/app.rs"), root));
    }

    #[tokio::test]
    async fn dispatching_an_unknown_tool_lists_the_real_ones() {
        let (_dir, ctx) = ctx();
        let outcome = dispatch("teleport", &serde_json::json!({}), &ctx).await;
        assert!(!outcome.is_ok());
        assert!(outcome.text().contains("read_file"), "{}", outcome.text());
    }

    #[test]
    fn opt_usize_accepts_numbers_and_numeric_strings() {
        let args = serde_json::json!({"a": 12, "b": "34", "c": "nope"});
        assert_eq!(opt_usize(&args, "a"), Some(12));
        assert_eq!(opt_usize(&args, "b"), Some(34));
        assert_eq!(opt_usize(&args, "c"), None);
        assert_eq!(opt_usize(&args, "missing"), None);
    }

    #[test]
    fn opt_bool_accepts_real_bools_and_stringified_ones() {
        let args = serde_json::json!({"a": true, "b": "true", "c": false, "d": "no"});
        assert!(opt_bool(&args, "a"));
        assert!(opt_bool(&args, "b"));
        assert!(!opt_bool(&args, "c"));
        assert!(!opt_bool(&args, "d"));
        assert!(!opt_bool(&args, "missing"));
    }
}
