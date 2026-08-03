//! Filesystem tools. Every one of these resolves its path through
//! `tools::resolve`, so nothing here can touch a file outside the workspace.

use super::{arg_str, cap, opt_bool, opt_usize, resolve, ToolCtx, ToolOutcome};
use serde_json::{json, Value};

const DEFAULT_READ_LIMIT: usize = 2000;

pub fn read_file_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "path": { "type": "string", "description": "Path relative to the workspace root." },
            "offset": { "type": "integer", "description": "1-based line number to start from. Defaults to 1." },
            "limit": { "type": "integer", "description": "Maximum lines to return. Defaults to 2000." }
        },
        "required": ["path"]
    })
}

pub fn write_file_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "path": { "type": "string", "description": "Path relative to the workspace root." },
            "content": { "type": "string", "description": "Full contents to write. This replaces the file entirely." }
        },
        "required": ["path", "content"]
    })
}

pub fn edit_file_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "path": { "type": "string", "description": "Path relative to the workspace root." },
            "old_string": { "type": "string", "description": "Exact text to replace, including indentation." },
            "new_string": { "type": "string", "description": "Text to put in its place." },
            "replace_all": { "type": "boolean", "description": "Replace every occurrence instead of requiring a unique match." }
        },
        "required": ["path", "old_string", "new_string"]
    })
}

pub fn list_dir_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "path": { "type": "string", "description": "Directory relative to the workspace root. Defaults to the root." }
        }
    })
}

pub fn read_file(args: &Value, ctx: &ToolCtx) -> ToolOutcome {
    let raw = match arg_str(args, "path") {
        Ok(p) => p,
        Err(e) => return ToolOutcome::Err(e),
    };
    let path = match resolve(ctx, raw) {
        Ok(p) => p,
        Err(e) => return ToolOutcome::Err(e),
    };

    if path.is_dir() {
        return ToolOutcome::Err(format!("'{raw}' is a directory; use list_dir instead"));
    }

    let contents = match std::fs::read(&path) {
        Ok(bytes) => match String::from_utf8(bytes) {
            Ok(text) => text,
            // Reading a binary as lossy UTF-8 floods the context with garbage
            // that the model then tries to reason about.
            Err(_) => return ToolOutcome::Err(format!("'{raw}' is not a UTF-8 text file")),
        },
        Err(e) => return ToolOutcome::Err(format!("could not read '{raw}': {e}")),
    };

    if contents.is_empty() {
        return ToolOutcome::Ok(format!("'{raw}' exists but is empty."));
    }

    let lines: Vec<&str> = contents.lines().collect();
    let offset = opt_usize(args, "offset").unwrap_or(1).max(1);
    let limit = opt_usize(args, "limit").unwrap_or(DEFAULT_READ_LIMIT).max(1);

    if offset > lines.len() {
        return ToolOutcome::Err(format!(
            "offset {offset} is past the end of '{raw}' ({} lines)",
            lines.len()
        ));
    }

    let start = offset - 1;
    let end = (start + limit).min(lines.len());
    let mut out = String::new();
    for (i, line) in lines[start..end].iter().enumerate() {
        out.push_str(&format!("{:>6}\t{line}\n", start + i + 1));
    }
    if end < lines.len() {
        out.push_str(&format!(
            "\n[{} more lines; re-read with offset={}]",
            lines.len() - end,
            end + 1
        ));
    }

    ToolOutcome::Ok(cap(&out))
}

pub fn write_file(args: &Value, ctx: &ToolCtx) -> ToolOutcome {
    let raw = match arg_str(args, "path") {
        Ok(p) => p,
        Err(e) => return ToolOutcome::Err(e),
    };
    let content = match arg_str(args, "content") {
        Ok(c) => c,
        Err(e) => return ToolOutcome::Err(e),
    };
    let path = match resolve(ctx, raw) {
        Ok(p) => p,
        Err(e) => return ToolOutcome::Err(e),
    };

    if path.is_dir() {
        return ToolOutcome::Err(format!("'{raw}' is a directory"));
    }

    let existed = path.exists();
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return ToolOutcome::Err(format!("could not create '{}': {e}", ctx.display(parent)));
        }
    }
    if let Err(e) = std::fs::write(&path, content) {
        return ToolOutcome::Err(format!("could not write '{raw}': {e}"));
    }

    ToolOutcome::Ok(format!(
        "{} {} ({} bytes, {} lines).",
        if existed { "Overwrote" } else { "Created" },
        ctx.display(&path),
        content.len(),
        content.lines().count()
    ))
}

pub fn edit_file(args: &Value, ctx: &ToolCtx) -> ToolOutcome {
    let raw = match arg_str(args, "path") {
        Ok(p) => p,
        Err(e) => return ToolOutcome::Err(e),
    };
    let old = match arg_str(args, "old_string") {
        Ok(s) => s,
        Err(e) => return ToolOutcome::Err(e),
    };
    let new = match arg_str(args, "new_string") {
        Ok(s) => s,
        Err(e) => return ToolOutcome::Err(e),
    };
    if old == new {
        return ToolOutcome::Err("old_string and new_string are identical".to_string());
    }
    if old.is_empty() {
        return ToolOutcome::Err(
            "old_string must not be empty; use write_file to create a file".to_string(),
        );
    }

    let path = match resolve(ctx, raw) {
        Ok(p) => p,
        Err(e) => return ToolOutcome::Err(e),
    };
    let contents = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) => return ToolOutcome::Err(format!("could not read '{raw}': {e}")),
    };

    let matches = contents.matches(old).count();
    let replace_all = opt_bool(args, "replace_all");
    match matches {
        0 => {
            return ToolOutcome::Err(format!(
                "old_string was not found in '{raw}'. It must match byte for byte, including indentation."
            ))
        }
        // Silently editing the wrong one of several identical matches is the
        // single most damaging thing this tool could do.
        n if n > 1 && !replace_all => {
            return ToolOutcome::Err(format!(
                "old_string appears {n} times in '{raw}'. Add surrounding context to make it unique, \
                 or pass replace_all: true."
            ))
        }
        _ => {}
    }

    let updated = if replace_all {
        contents.replace(old, new)
    } else {
        contents.replacen(old, new, 1)
    };
    if let Err(e) = std::fs::write(&path, &updated) {
        return ToolOutcome::Err(format!("could not write '{raw}': {e}"));
    }

    ToolOutcome::Ok(format!(
        "Replaced {} occurrence{} in {}.",
        matches.min(if replace_all { matches } else { 1 }),
        if replace_all && matches > 1 { "s" } else { "" },
        ctx.display(&path)
    ))
}

pub fn list_dir(args: &Value, ctx: &ToolCtx) -> ToolOutcome {
    let raw = super::opt_str(args, "path").unwrap_or(".");
    let path = match resolve(ctx, raw) {
        Ok(p) => p,
        Err(e) => return ToolOutcome::Err(e),
    };

    let entries = match std::fs::read_dir(&path) {
        Ok(e) => e,
        Err(e) => return ToolOutcome::Err(format!("could not list '{raw}': {e}")),
    };

    let mut names: Vec<String> = Vec::new();
    for entry in entries.flatten() {
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let name = entry.file_name().to_string_lossy().to_string();
        names.push(if is_dir { format!("{name}/") } else { name });
    }
    if names.is_empty() {
        return ToolOutcome::Ok(format!("{} is empty.", ctx.display(&path)));
    }
    // Directories first, then alphabetical -- a stable order keeps repeated
    // listings from looking like the tree changed.
    names.sort_by(|a, b| {
        let dir = |s: &String| !s.ends_with('/');
        dir(a).cmp(&dir(b)).then_with(|| a.cmp(b))
    });

    ToolOutcome::Ok(cap(&format!(
        "{}:\n{}",
        ctx.display(&path),
        names.join("\n")
    )))
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::*;

    #[test]
    fn read_file_numbers_lines() {
        let (_dir, ctx) = ctx();
        write(&ctx, "a.txt", "one\ntwo\n");

        let out = read_file(&json!({"path": "a.txt"}), &ctx);
        assert!(out.is_ok());
        assert_eq!(out.text(), "     1\tone\n     2\ttwo\n");
    }

    #[test]
    fn read_file_pages_with_offset_and_limit() {
        let (_dir, ctx) = ctx();
        write(&ctx, "a.txt", "1\n2\n3\n4\n5\n");

        let out = read_file(&json!({"path": "a.txt", "offset": 2, "limit": 2}), &ctx);
        assert!(out.text().contains("     2\t2"));
        assert!(out.text().contains("     3\t3"));
        assert!(!out.text().contains("\t4"));
        // The model must know there is more, and where to resume.
        assert!(out.text().contains("[2 more lines; re-read with offset=4]"));
    }

    #[test]
    fn read_file_reports_missing_files_and_directories_distinctly() {
        let (_dir, ctx) = ctx();
        std::fs::create_dir(ctx.workspace.join("sub")).unwrap();

        let missing = read_file(&json!({"path": "nope.txt"}), &ctx);
        assert!(!missing.is_ok());
        assert!(missing.text().contains("could not read"));

        let dir = read_file(&json!({"path": "sub"}), &ctx);
        assert!(!dir.is_ok());
        assert!(dir.text().contains("list_dir"));
    }

    #[test]
    fn read_file_refuses_binary_content() {
        let (_dir, ctx) = ctx();
        std::fs::write(ctx.workspace.join("blob.bin"), [0xff, 0xfe, 0x00]).unwrap();

        let out = read_file(&json!({"path": "blob.bin"}), &ctx);
        assert!(!out.is_ok());
        assert!(out.text().contains("not a UTF-8 text file"));
    }

    #[test]
    fn read_file_past_the_end_says_how_long_the_file_is() {
        let (_dir, ctx) = ctx();
        write(&ctx, "a.txt", "one\n");

        let out = read_file(&json!({"path": "a.txt", "offset": 99}), &ctx);
        assert!(!out.is_ok());
        assert!(out.text().contains("1 lines"), "{}", out.text());
    }

    #[test]
    fn write_file_creates_missing_parent_directories() {
        let (_dir, ctx) = ctx();

        let out = write_file(&json!({"path": "a/b/c.txt", "content": "hi"}), &ctx);
        assert!(out.is_ok(), "{}", out.text());
        assert!(out.text().starts_with("Created"));
        assert_eq!(
            std::fs::read_to_string(ctx.workspace.join("a/b/c.txt")).unwrap(),
            "hi"
        );
    }

    #[test]
    fn write_file_distinguishes_creating_from_overwriting() {
        let (_dir, ctx) = ctx();
        write(&ctx, "a.txt", "old");

        let out = write_file(&json!({"path": "a.txt", "content": "new"}), &ctx);
        assert!(out.text().starts_with("Overwrote"), "{}", out.text());
        assert_eq!(
            std::fs::read_to_string(ctx.workspace.join("a.txt")).unwrap(),
            "new"
        );
    }

    #[test]
    fn write_file_cannot_escape_the_workspace() {
        let (_dir, ctx) = ctx();
        let out = write_file(&json!({"path": "../pwned.txt", "content": "x"}), &ctx);
        assert!(!out.is_ok());
        assert!(out.text().contains("outside the workspace"));
    }

    #[test]
    fn edit_file_replaces_a_unique_match() {
        let (_dir, ctx) = ctx();
        write(&ctx, "a.rs", "fn one() {}\nfn two() {}\n");

        let out = edit_file(
            &json!({"path": "a.rs", "old_string": "fn two() {}", "new_string": "fn three() {}"}),
            &ctx,
        );
        assert!(out.is_ok(), "{}", out.text());
        assert_eq!(
            std::fs::read_to_string(ctx.workspace.join("a.rs")).unwrap(),
            "fn one() {}\nfn three() {}\n"
        );
    }

    #[test]
    fn edit_file_refuses_an_ambiguous_match_and_leaves_the_file_alone() {
        let (_dir, ctx) = ctx();
        let original = "x = 1;\nx = 1;\n";
        write(&ctx, "a.rs", original);

        let out = edit_file(
            &json!({"path": "a.rs", "old_string": "x = 1;", "new_string": "x = 2;"}),
            &ctx,
        );
        assert!(!out.is_ok());
        assert!(out.text().contains("appears 2 times"), "{}", out.text());
        assert_eq!(
            std::fs::read_to_string(ctx.workspace.join("a.rs")).unwrap(),
            original,
            "an ambiguous edit must not modify the file"
        );
    }

    #[test]
    fn edit_file_replaces_every_match_when_asked() {
        let (_dir, ctx) = ctx();
        write(&ctx, "a.rs", "x = 1;\nx = 1;\n");

        let out = edit_file(
            &json!({"path": "a.rs", "old_string": "x = 1;", "new_string": "x = 2;", "replace_all": true}),
            &ctx,
        );
        assert!(out.is_ok(), "{}", out.text());
        assert_eq!(
            std::fs::read_to_string(ctx.workspace.join("a.rs")).unwrap(),
            "x = 2;\nx = 2;\n"
        );
    }

    #[test]
    fn edit_file_reports_a_missing_match_without_writing() {
        let (_dir, ctx) = ctx();
        write(&ctx, "a.rs", "hello\n");

        let out = edit_file(
            &json!({"path": "a.rs", "old_string": "goodbye", "new_string": "hi"}),
            &ctx,
        );
        assert!(!out.is_ok());
        assert!(out.text().contains("not found"));
        assert_eq!(
            std::fs::read_to_string(ctx.workspace.join("a.rs")).unwrap(),
            "hello\n"
        );
    }

    #[test]
    fn edit_file_rejects_a_no_op_and_an_empty_needle() {
        let (_dir, ctx) = ctx();
        write(&ctx, "a.rs", "hello\n");

        let same = edit_file(
            &json!({"path": "a.rs", "old_string": "a", "new_string": "a"}),
            &ctx,
        );
        assert!(same.text().contains("identical"));

        let empty = edit_file(
            &json!({"path": "a.rs", "old_string": "", "new_string": "x"}),
            &ctx,
        );
        assert!(empty.text().contains("must not be empty"));
    }

    #[test]
    fn list_dir_marks_directories_and_sorts_them_first() {
        let (_dir, ctx) = ctx();
        write(&ctx, "zebra.txt", "");
        write(&ctx, "src/app.rs", "");
        write(&ctx, "alpha.txt", "");

        let out = list_dir(&json!({}), &ctx);
        assert!(out.is_ok(), "{}", out.text());
        let listed: Vec<&str> = out.text().lines().skip(1).collect();
        assert_eq!(listed, vec!["src/", "alpha.txt", "zebra.txt"]);
    }

    #[test]
    fn list_dir_defaults_to_the_workspace_root() {
        let (_dir, ctx) = ctx();
        write(&ctx, "only.txt", "");
        assert!(list_dir(&json!({}), &ctx).text().contains("only.txt"));
    }

    #[test]
    fn missing_required_arguments_are_reported_not_panicked() {
        let (_dir, ctx) = ctx();
        assert!(read_file(&json!({}), &ctx).text().contains("'path'"));
        assert!(write_file(&json!({"path": "a"}), &ctx)
            .text()
            .contains("'content'"));
    }
}
