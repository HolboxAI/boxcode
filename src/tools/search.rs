//! Finding things: `glob` by filename, `grep` by content.

use super::{arg_str, cap, is_skipped, opt_str, resolve, ToolCtx, ToolOutcome};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

const MAX_GLOB_RESULTS: usize = 500;
const MAX_GREP_MATCHES: usize = 200;
const MAX_LINE_CHARS: usize = 300;

pub fn glob_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "pattern": {
                "type": "string",
                "description": "Glob relative to the workspace root, e.g. 'src/**/*.rs' or '**/Cargo.toml'."
            }
        },
        "required": ["pattern"]
    })
}

pub fn grep_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "pattern": { "type": "string", "description": "Regular expression to search for." },
            "path": { "type": "string", "description": "File or directory to search. Defaults to the workspace root." },
            "glob": { "type": "string", "description": "Restrict to files matching this glob, e.g. '**/*.rs'." }
        },
        "required": ["pattern"]
    })
}

pub fn glob(args: &Value, ctx: &ToolCtx) -> ToolOutcome {
    let pattern = match arg_str(args, "pattern") {
        Ok(p) => p,
        Err(e) => return ToolOutcome::Err(e),
    };

    let files = match collect(ctx, &ctx.workspace.clone(), pattern) {
        Ok(f) => f,
        Err(e) => return ToolOutcome::Err(e),
    };

    if files.is_empty() {
        return ToolOutcome::Ok(format!("No files match '{pattern}'."));
    }

    let total = files.len();
    let shown: Vec<String> = files
        .iter()
        .take(MAX_GLOB_RESULTS)
        .map(|p| ctx.display(p))
        .collect();

    let mut out = shown.join("\n");
    if total > MAX_GLOB_RESULTS {
        out.push_str(&format!(
            "\n[{} more matches; narrow the pattern]",
            total - MAX_GLOB_RESULTS
        ));
    }
    ToolOutcome::Ok(cap(&out))
}

pub fn grep(args: &Value, ctx: &ToolCtx) -> ToolOutcome {
    let pattern = match arg_str(args, "pattern") {
        Ok(p) => p,
        Err(e) => return ToolOutcome::Err(e),
    };
    let regex = match regex::Regex::new(pattern) {
        Ok(r) => r,
        Err(e) => return ToolOutcome::Err(format!("'{pattern}' is not a valid regular expression: {e}")),
    };

    let root = match opt_str(args, "path") {
        Some(raw) => match resolve(ctx, raw) {
            Ok(p) => p,
            Err(e) => return ToolOutcome::Err(e),
        },
        None => ctx.workspace.clone(),
    };

    let files = if root.is_file() {
        vec![root.clone()]
    } else {
        let pattern = opt_str(args, "glob").unwrap_or("**/*");
        match collect(ctx, &root, pattern) {
            Ok(f) => f,
            Err(e) => return ToolOutcome::Err(e),
        }
    };

    let mut hits: Vec<String> = Vec::new();
    let mut truncated = false;
    'files: for file in &files {
        // A non-UTF-8 file is not a text file; skip rather than fail the search.
        let Ok(contents) = std::fs::read_to_string(file) else {
            continue;
        };
        for (i, line) in contents.lines().enumerate() {
            if regex.is_match(line) {
                if hits.len() >= MAX_GREP_MATCHES {
                    truncated = true;
                    break 'files;
                }
                hits.push(format!(
                    "{}:{}:{}",
                    ctx.display(file),
                    i + 1,
                    clip(line.trim_end())
                ));
            }
        }
    }

    if hits.is_empty() {
        return ToolOutcome::Ok(format!(
            "No matches for '{pattern}' in {} file{}.",
            files.len(),
            if files.len() == 1 { "" } else { "s" }
        ));
    }

    let mut out = hits.join("\n");
    if truncated {
        out.push_str(&format!(
            "\n[stopped at {MAX_GREP_MATCHES} matches; narrow the pattern or pass a path]"
        ));
    }
    ToolOutcome::Ok(cap(&out))
}

/// Expand `pattern` under `root`, keeping only files inside the workspace and
/// outside the noisy directories.
fn collect(ctx: &ToolCtx, root: &Path, pattern: &str) -> Result<Vec<PathBuf>, String> {
    if pattern.trim().is_empty() {
        return Err("pattern must not be empty".to_string());
    }
    let full = root.join(pattern);
    let full = full.to_str().ok_or("pattern is not valid UTF-8")?;

    let paths = glob::glob(full).map_err(|e| format!("'{pattern}' is not a valid glob: {e}"))?;

    let mut files: Vec<PathBuf> = paths
        .flatten()
        .filter(|p| p.is_file())
        // Canonicalize *before* the containment check. `starts_with` is lexical,
        // so `<workspace>/../sibling/x.rs` would otherwise pass it -- a glob is a
        // path expression like any other and must not read its way out.
        .filter_map(|p| p.canonicalize().ok())
        .filter(|p| p.starts_with(&ctx.workspace))
        .filter(|p| !is_skipped(p, &ctx.workspace))
        .collect();
    files.sort();
    Ok(files)
}

/// Keep one absurdly long line (a minified bundle, a base64 blob) from crowding
/// out every other match.
fn clip(line: &str) -> String {
    if line.chars().count() <= MAX_LINE_CHARS {
        return line.to_string();
    }
    let cut: String = line.chars().take(MAX_LINE_CHARS).collect();
    format!("{cut}…")
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::*;

    fn populated() -> (tempfile::TempDir, ToolCtx) {
        let (dir, ctx) = ctx();
        write(&ctx, "src/app.rs", "fn main() {}\nlet needle = 1;\n");
        write(&ctx, "src/ui/render.rs", "// needle here\n");
        write(&ctx, "README.md", "no match\n");
        write(&ctx, "target/debug/build.rs", "let needle = 99;\n");
        (dir, ctx)
    }

    #[test]
    fn glob_matches_recursively_and_returns_relative_paths() {
        let (_dir, ctx) = populated();
        let out = glob(&json!({"pattern": "src/**/*.rs"}), &ctx);
        assert!(out.is_ok(), "{}", out.text());

        let mut found: Vec<&str> = out.text().lines().collect();
        found.sort();
        assert_eq!(found, vec!["src/app.rs", "src/ui/render.rs"]);
    }

    #[test]
    fn glob_skips_build_output_directories() {
        let (_dir, ctx) = populated();
        let out = glob(&json!({"pattern": "**/*.rs"}), &ctx);
        assert!(
            !out.text().contains("target/"),
            "target/ should be skipped: {}",
            out.text()
        );
    }

    #[test]
    fn glob_reports_no_matches_as_success() {
        let (_dir, ctx) = populated();
        // Finding nothing is an answer, not a failure -- an Err would push the
        // model toward retrying instead of concluding.
        let out = glob(&json!({"pattern": "**/*.py"}), &ctx);
        assert!(out.is_ok());
        assert!(out.text().contains("No files match"));
    }

    #[test]
    fn grep_finds_matches_with_path_and_line_number() {
        let (_dir, ctx) = populated();
        let out = grep(&json!({"pattern": "needle"}), &ctx);
        assert!(out.is_ok(), "{}", out.text());
        assert!(out.text().contains("src/app.rs:2:let needle = 1;"));
        assert!(out.text().contains("src/ui/render.rs:1:// needle here"));
        assert!(!out.text().contains("target/"));
    }

    #[test]
    fn grep_can_be_scoped_by_glob_and_by_path() {
        let (_dir, ctx) = populated();

        let by_glob = grep(&json!({"pattern": "needle", "glob": "**/render.rs"}), &ctx);
        assert!(by_glob.text().contains("render.rs"));
        assert!(!by_glob.text().contains("app.rs"));

        let by_path = grep(&json!({"pattern": "needle", "path": "src/app.rs"}), &ctx);
        assert!(by_path.text().contains("app.rs"));
        assert!(!by_path.text().contains("render.rs"));
    }

    #[test]
    fn grep_rejects_an_invalid_regex_with_an_actionable_message() {
        let (_dir, ctx) = populated();
        let out = grep(&json!({"pattern": "unclosed("}), &ctx);
        assert!(!out.is_ok());
        assert!(out.text().contains("not a valid regular expression"));
    }

    #[test]
    fn grep_reports_no_matches_as_success() {
        let (_dir, ctx) = populated();
        let out = grep(&json!({"pattern": "zzz_nothing"}), &ctx);
        assert!(out.is_ok());
        assert!(out.text().contains("No matches"));
    }

    #[test]
    fn grep_skips_binary_files_instead_of_failing() {
        let (_dir, ctx) = populated();
        std::fs::write(ctx.workspace.join("blob.bin"), [0xff, 0xfe, 0x00]).unwrap();

        let out = grep(&json!({"pattern": "needle"}), &ctx);
        assert!(out.is_ok(), "{}", out.text());
        assert!(out.text().contains("src/app.rs"));
    }

    #[test]
    fn grep_clips_absurdly_long_lines() {
        let (_dir, ctx) = ctx();
        write(&ctx, "min.js", &format!("needle{}", "x".repeat(5000)));

        let out = grep(&json!({"pattern": "needle"}), &ctx);
        assert!(out.text().contains('…'));
        assert!(out.text().len() < 1000);
    }

    /// `..` in a glob must not reach a sibling directory. Note that walking out
    /// and back in again is fine -- what matters is that every path returned
    /// resolves inside the workspace.
    #[test]
    fn a_glob_cannot_read_its_way_out_of_the_workspace() {
        let (_dir, ctx) = populated();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.rs"), "fn secret() {}").unwrap();

        let out = glob(&json!({"pattern": "../**/*.rs"}), &ctx);
        assert!(out.is_ok(), "{}", out.text());
        assert!(!out.text().contains("secret.rs"), "{}", out.text());
        for line in out.text().lines() {
            assert!(
                !line.starts_with("..") && !line.starts_with('/'),
                "escaped the workspace: {line}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_glob_does_not_follow_a_symlink_out_of_the_workspace() {
        let (_dir, ctx) = populated();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.rs"), "fn secret() {}").unwrap();
        std::os::unix::fs::symlink(outside.path(), ctx.workspace.join("link")).unwrap();

        let out = glob(&json!({"pattern": "**/*.rs"}), &ctx);
        assert!(!out.text().contains("secret.rs"), "{}", out.text());
    }
}
