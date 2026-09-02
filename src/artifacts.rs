//! Publishing a preview: put static files somewhere the user can open in a
//! browser, and hand back a link that expires.
//!
//! The shape is deliberately dull. This process never holds AWS credentials --
//! shipping them in a binary would hand the bucket to anyone who downloaded
//! boxcode -- so it POSTs a manifest to a signing endpoint, receives one
//! short-lived presigned `PUT` per file, and uploads directly. The endpoint's
//! own role can do nothing but write under one prefix, and everything written
//! there is deleted by a bucket lifecycle rule after two days. So the worst
//! outcome of the endpoint being abused is rubbish that expires on its own.
//!
//! What arrives at the browser is decided by the *signer*, not by this
//! module: it picks the `Content-Type` for each extension and signs it, so a
//! client that lies about a file's type simply fails the upload. That is why
//! there is no content-type guessing here.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Files the preview host will accept, and nothing else.
///
/// An allowlist rather than a blocklist, and it has to agree with the signer's
/// own table -- anything missing there is rejected at signing time anyway, so
/// checking here only means a clearer message and no wasted round trip.
const PUBLISHABLE: &[&str] = &[
    "html", "htm", "css", "js", "mjs", "json", "map", "webmanifest", "csv", "tsv", "txt", "md",
    "log", "xml", "yml", "yaml", "svg", "png", "jpg", "jpeg", "gif", "webp", "ico", "woff",
    "woff2", "ttf", "pdf", "wasm",
];

/// Ceilings, matching the signer's. Checked here as well so an oversized
/// directory is reported as one clear message rather than as an HTTP 413 the
/// model has to interpret.
const MAX_FILES: usize = 60;
const MAX_TOTAL_BYTES: u64 = 25 * 1024 * 1024;
const MAX_FILE_BYTES: u64 = 10 * 1024 * 1024;

/// How long a published link lives. Stated everywhere it is shown, because a
/// link that has quietly stopped working is worse than one nobody was
/// promised.
pub const EXPIRY_HOURS: u32 = 48;

/// How many of the page's own assets the post-publish check will fetch before
/// it stops. A publish must not get slower in proportion to how many files it
/// has: the first few references are what break together when they break at
/// all, since they share one wrong base path.
const MAX_VERIFIED_ASSETS: usize = 10;

#[derive(Debug)]
pub struct Published {
    pub url: String,
    pub files: usize,
    pub bytes: u64,
    pub expires_in_hours: u32,
    /// Whether a live GET against `url`, right after every upload reported
    /// success, actually served what was just written -- not just that S3
    /// accepted the PUTs. The presigned upload succeeding proves the bytes
    /// reached the bucket; it says nothing about whether the developer-
    /// and visitor-facing URL serves them, or whether a diffing signer even
    /// re-sent `index.html` this call. `false` means the check failed or
    /// was inconclusive (network hiccup, no fresh `index.html` to compare
    /// against, ...), not that the publish itself failed -- see
    /// `verify_live`. The caller decides how much to make of it; this
    /// module only refuses to claim more certainty than it actually has.
    pub verified: bool,
}

#[derive(serde::Serialize)]
struct ManifestEntry {
    path: String,
    size: u64,
}

#[derive(serde::Deserialize)]
struct Upload {
    path: String,
    content_type: String,
    url: String,
}

#[derive(serde::Deserialize)]
struct SignResponse {
    id: String,
    url: String,
    #[serde(default = "default_expiry")]
    expires_in_hours: u32,
    uploads: Vec<Upload>,
}

fn default_expiry() -> u32 {
    EXPIRY_HOURS
}

/// One file to publish: where it is on disk, and what it will be called.
#[derive(Debug)]
struct Candidate {
    source: PathBuf,
    /// Forward-slashed and relative, because it becomes part of a URL.
    key: String,
    size: u64,
}

/// Gather what should be published from a file or a directory.
///
/// A single file becomes `index.html` when it is HTML, so the link opens the
/// page rather than a directory listing that does not exist. Everything else
/// keeps its own name and gains a generated `index.html` beside it, because
/// the link handed to the user points at the directory: without one, a lone
/// CSV publishes successfully and then 404s when opened, which is the most
/// confusing possible outcome.
fn collect(root: &Path) -> Result<Vec<Candidate>, String> {
    if root.is_file() {
        let size = file_size(root)?;
        let name = root
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| format!("{} has no usable name", root.display()))?;
        check_extension(name)?;
        if is_html(name) {
            return Ok(vec![Candidate {
                source: root.to_path_buf(),
                key: "index.html".to_string(),
                size,
            }]);
        }
        let wrapper = wrap_single(name)?;
        let wrapper_size = file_size(&wrapper)?;
        return Ok(vec![
            Candidate { source: root.to_path_buf(), key: name.to_string(), size },
            Candidate { source: wrapper, key: "index.html".to_string(), size: wrapper_size },
        ]);
    }

    // A JS framework's project root also has an index.html -- Vite's
    // template -- so collecting from there "succeeds" and previews an
    // unstyled scaffold. When a real build output sits beside it, that is
    // what a first publish should put online.
    let built = prefer_built_output(root);
    let root = built.as_path();

    let mut out = Vec::new();
    walk(root, root, &mut out)?;
    if out.is_empty() {
        return Err(format!("{} has nothing publishable in it", root.display()));
    }
    if !out.iter().any(|c| c.key == "index.html") {
        return Err(format!(
            "{} has no index.html, so there would be nothing to open. Point at the built \
             output directory (dist/, build/, out/, public/) rather than the project root.",
            root.display()
        ));
    }
    Ok(out)
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<Candidate>) -> Result<(), String> {
    let entries = std::fs::read_dir(dir).map_err(|e| format!("could not read {}: {e}", dir.display()))?;
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        // Dotfiles are configuration, keys and VCS metadata, never page
        // content. Publishing a build directory must not publish `.env`
        // because it happened to be sitting in it.
        if name.starts_with('.') {
            continue;
        }
        if path.is_dir() {
            if matches!(name.as_str(), "node_modules" | "target" | "__pycache__") {
                continue;
            }
            walk(root, &path, out)?;
            continue;
        }
        // Unknown types are skipped rather than fatal: one stray `.DS_Store`
        // or `.map.gz` in a build output should not stop the preview.
        if check_extension(&name).is_err() {
            continue;
        }
        let size = file_size(&path)?;
        let key = path
            .strip_prefix(root)
            .map_err(|_| "path escaped the directory being published".to_string())?
            .components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        out.push(Candidate { source: path, key, size });
    }
    Ok(())
}

/// Directories a JS framework's build actually writes to. `public/` is not
/// among them: it is source input for Vite and Next, not output, and picking
/// it would publish the unbuilt tree under a different name.
const BUILT_OUTPUT_DIRS: &[&str] = &["dist", "build", "out"];

/// If `root` is a JS project that has already been built, return the build
/// output directory; otherwise return `root` unchanged.
fn prefer_built_output(root: &Path) -> PathBuf {
    if !root.is_dir() || !root.join("package.json").is_file() {
        return root.to_path_buf();
    }
    if let Some(name) = root.file_name().and_then(|n| n.to_str()) {
        if BUILT_OUTPUT_DIRS.contains(&name) {
            return root.to_path_buf();
        }
    }
    for dir in BUILT_OUTPUT_DIRS {
        if root.join(dir).join("index.html").is_file() {
            return root.join(dir);
        }
    }
    root.to_path_buf()
}

/// Write a tiny `index.html` that displays `name`, so the artifact link opens
/// something rather than 404ing.
///
/// An `<iframe>` rather than parsing the file: the browser already knows how
/// to render every type the signer will serve -- text and CSV as plain text,
/// images as images, PDFs in the viewer -- and it is same-origin, so nothing
/// is fetched or interpreted here. A parser would be more work and would have
/// to be right about more formats than a browser already is.
fn wrap_single(name: &str) -> Result<PathBuf, String> {
    let escaped = name
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;");
    let html = format!(
        "<!doctype html>\n<meta charset=utf-8>\n<title>{escaped}</title>\n\
         <style>html,body{{margin:0;height:100%;font:14px/1.5 ui-monospace,SFMono-Regular,Menlo,monospace}}\
         header{{padding:.6rem .9rem;border-bottom:1px solid #8884;display:flex;gap:.75rem;align-items:baseline}}\
         a{{color:inherit}}iframe{{border:0;width:100%;height:calc(100% - 2.9rem)}}</style>\n\
         <header><strong>{escaped}</strong><a href=\"{escaped}\" download>download</a></header>\n\
         <iframe src=\"{escaped}\"></iframe>\n"
    );
    let path = std::env::temp_dir().join(format!("boxcode-artifact-index-{}.html", std::process::id()));
    std::fs::write(&path, html).map_err(|e| format!("could not prepare a viewer page: {e}"))?;
    Ok(path)
}

/// Where `publish` last put a given path, so publishing it again updates the
/// same link instead of minting a new one -- the same file, the same slot,
/// same shape as `session.rs`'s `~/.boxcode/sessions/`: a plain file the
/// user can delete to forget, not a database.
fn registry_path() -> Option<PathBuf> {
    std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    Some(crate::config::Config::config_dir().join("artifacts.json"))
}

/// One publish, as recorded locally. `published_at` is unix seconds -- kept
/// so `all_local` can drop entries once the link itself has expired (see
/// `EXPIRY_HOURS`) instead of listing dead links next to live ones forever.
#[derive(serde::Serialize, serde::Deserialize, Clone)]
struct RegistryEntry {
    id: String,
    published_at: u64,
}

fn load_registry() -> HashMap<String, RegistryEntry> {
    registry_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// The id `path` was last published under, keyed by its canonicalized form
/// so `./foo.html` and `foo.html` from different cwds still match.
///
/// `pub(crate)`, not private: `enable_auth` (see `tools.rs`) needs the same
/// id to provision a project's auth against -- a project's identity to the
/// rest of boxcode *is* the artifact id it already published under, not a
/// second id invented for this. Deliberately not filtered by `EXPIRY_HOURS`
/// like `all_local` is: republishing a path whose old link expired should
/// still reuse that id (see `publish`'s comment on `remembered_id`), it just
/// won't show up in the `/pull` list until it is published again.
pub(crate) fn remembered_id(path: &Path) -> Option<String> {
    let key = path.canonicalize().ok()?.to_string_lossy().into_owned();
    load_registry().get(&key).map(|e| e.id.clone())
}

/// Whether anything at or under `root` has ever been published -- unlike
/// `remembered_id`, not an exact-key lookup. `root` here is a *workspace*
/// root (see `agent.rs`'s `schemas` call), and `Workspace::new` resolves a
/// published single file to its containing project directory, not the file
/// itself (see `workspace.rs`) -- so the registry key for a project
/// published as one file (`project/todo.html`) is never equal to that
/// project's own workspace root (`project/`), only nested under it. An
/// exact match here would report "never published" for exactly the
/// projects tonight's workspace-resolution fix exists to handle correctly.
/// Deliberately not filtered by `EXPIRY_HOURS` like `all_local` is, same
/// reasoning as `remembered_id`: an expired preview link does not make the
/// tools that manage it stop being relevant to a workspace that has
/// genuinely published from before.
pub(crate) fn any_published_under(root: &Path) -> bool {
    let Ok(root) = root.canonicalize() else { return false };
    load_registry().keys().any(|key| Path::new(key).starts_with(&root))
}

/// Projects this machine has published within the last `EXPIRY_HOURS` --
/// path (already canonicalized, since that is how the registry keys it)
/// paired with its artifact id, newest first. Bounded to the link's own
/// lifetime because a `/pull` entry for a link that has already expired on
/// the server is just noise: nothing left for the id to identify. Used by
/// `/pull` (see `app.rs`) to let a developer switch to a different local
/// project without needing to remember or retype its path; nothing here
/// reaches the network or the control-plane, it only reads this machine's
/// own registry file.
pub(crate) fn all_local() -> Vec<(String, String)> {
    let cutoff = now_secs().saturating_sub(EXPIRY_HOURS as u64 * 3600);
    let mut all: Vec<(String, RegistryEntry)> = load_registry()
        .into_iter()
        .filter(|(_, e)| e.published_at >= cutoff)
        .collect();
    // Newest first (what a developer picking up recent work wants); ties
    // broken by path so the order is still deterministic, not by whichever
    // way the HashMap happened to iterate.
    all.sort_by(|a, b| b.1.published_at.cmp(&a.1.published_at).then_with(|| a.0.cmp(&b.0)));
    all.into_iter().map(|(path, e)| (path, e.id)).collect()
}

/// A registry write is an amenity, not a correctness requirement: losing it
/// just means the next publish of this path starts a new artifact instead of
/// updating the old one, so every failure here is swallowed rather than
/// surfaced.
fn remember(path: &Path, id: &str) {
    let Some(key) = path.canonicalize().ok().map(|p| p.to_string_lossy().into_owned()) else {
        return;
    };
    let Some(reg_path) = registry_path() else { return };
    let Some(parent) = reg_path.parent() else { return };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    let mut map = load_registry();
    map.insert(key, RegistryEntry { id: id.to_string(), published_at: now_secs() });
    if let Ok(s) = serde_json::to_string_pretty(&map) {
        let _ = std::fs::write(reg_path, s);
    }
}

fn file_size(path: &Path) -> Result<u64, String> {
    std::fs::metadata(path)
        .map(|m| m.len())
        .map_err(|e| format!("could not read {}: {e}", path.display()))
}

fn is_html(name: &str) -> bool {
    matches!(extension(name).as_deref(), Some("html") | Some("htm"))
}

/// A small self-polling script, appended to every published HTML page, so a
/// tab already open on the artifact's URL picks up the *next* publish
/// automatically instead of the developer refreshing by hand.
///
/// Needs no new backend endpoint: every upload here already carries
/// `cache-control: no-cache` (see the comment on it a few lines up in
/// `publish`), which is what makes CloudFront revalidate instead of serving
/// a stale edge copy -- confirmed live when that header was added for the
/// same reason. S3/CloudFront already put a fresh ETag on every response, so
/// polling the page's own URL with `HEAD` and comparing ETags is enough; no
/// websocket, no control-plane, nothing else to operate.
///
/// Only ever changes the bytes sent to S3 -- the developer's own file on
/// disk is untouched, so nothing accumulates across repeated publishes and
/// there is no marker to strip back out.
const LIVE_RELOAD_SCRIPT: &str = r#"<script>(function(){var u=location.href.split('#')[0].split('?')[0];var t=null;setInterval(function(){fetch(u,{method:'HEAD',cache:'no-store'}).then(function(r){var e=r.headers.get('etag');if(!e)return;if(t===null){t=e;return;}if(e!==t){location.reload();}}).catch(function(){});},2000);})();</script>"#;

/// Inserts [`LIVE_RELOAD_SCRIPT`] just before `</body>` (or `</html>`, or at
/// the very end, whichever is found first) -- case-insensitively, since
/// hand-written HTML is not guaranteed to use lowercase tags.
///
/// Non-UTF-8 content is returned unchanged rather than corrupted: HTML this
/// old signer already accepts is `.html`/`.htm` by extension, which is
/// always meant to be text, but a byte-for-byte guarantee is worth more here
/// than a script tag on the one file that would not decode.
fn inject_live_reload(bytes: Vec<u8>) -> Vec<u8> {
    let Ok(html) = String::from_utf8(bytes.clone()) else {
        return bytes;
    };
    let lower = html.to_ascii_lowercase();
    let insert_at = lower.rfind("</body>").or_else(|| lower.rfind("</html>"));
    let Some(idx) = insert_at else {
        return format!("{html}{LIVE_RELOAD_SCRIPT}").into_bytes();
    };
    let mut out = String::with_capacity(html.len() + LIVE_RELOAD_SCRIPT.len());
    out.push_str(&html[..idx]);
    out.push_str(LIVE_RELOAD_SCRIPT);
    out.push_str(&html[idx..]);
    out.into_bytes()
}

/// The path an artifact is served under, taken from the URL the signer
/// returned -- `/artifacts/k9depef6` for `https://boxcode.sh/artifacts/k9depef6`.
///
/// Read off the response rather than built from the id, so a fork or a
/// self-hosted signer that lays its prefixes out differently keeps working
/// without this having to know its scheme.
fn served_under(url: &str) -> String {
    let after_scheme = url.split_once("://").map_or(url, |(_, rest)| rest);
    match after_scheme.find('/') {
        Some(i) => after_scheme[i..].trim_end_matches('/').to_string(),
        None => String::new(),
    }
}

/// Rewrites asset URLs in HTML so they resolve under the artifact's own
/// prefix. Two shapes, one failure:
///
/// - A Vite build emits `src="/assets/app.js"` (domain root).
/// - A handmade page emits `href="style.css"` (relative).
///
/// The artifact URL has no trailing slash and is not redirected to one, so a
/// browser treats the last segment as a file: `./style.css` on
/// `/artifacts/k9depef6` asks for `/artifacts/style.css`. Either way the
/// upload succeeds, S3 holds every byte, and the page is unstyled -- the
/// single most confusing way this can fail, because nothing anywhere
/// reports an error until the visitor opens it.
///
/// Rewriting to an absolute prefix rather than a relative `./` is
/// deliberate: an absolute prefix does not depend on how the URL was
/// written. `//host/path` is protocol-relative and already points at
/// another origin, so it is left alone; so is everything with a scheme.
///
/// `file_key` is the artifact-relative path of this HTML file, so a nested
/// page's `../style.css` lands at the prefix root rather than being
/// naively prefixed.
fn rebase_page_urls(html: &[u8], prefix: &str, file_key: &str) -> Vec<u8> {
    if prefix.is_empty() {
        return html.to_vec();
    }
    let Ok(text) = std::str::from_utf8(html) else {
        // Surrounding markup may be any encoding at all. The byte-wise
        // root-absolute pass still applies; relative URLs are left alone
        // rather than corrupting a page this cannot parse.
        return rebase_root_absolute_bytes(html, prefix);
    };
    let with_attrs = rebase_html_attr_urls(text, prefix, file_key);
    rebase_css_urls(with_attrs.as_bytes(), prefix)
}

/// Tests and the Vite fixture call this with an implicit `index.html`.
#[cfg(test)]
fn rebase_absolute_urls(html: &[u8], prefix: &str) -> Vec<u8> {
    rebase_page_urls(html, prefix, "index.html")
}

fn rebase_root_absolute_bytes(html: &[u8], prefix: &str) -> Vec<u8> {
    let prefix = prefix.as_bytes();
    let mut out = Vec::with_capacity(html.len() + 128);
    let mut i = 0;
    while i < html.len() {
        if html[i] == b'='
            && i + 3 < html.len()
            && (html[i + 1] == b'"' || html[i + 1] == b'\'')
            && html[i + 2] == b'/'
            && html[i + 3] != b'/'
        {
            out.push(b'=');
            out.push(html[i + 1]);
            out.extend_from_slice(prefix);
            i += 2;
            continue;
        }
        out.push(html[i]);
        i += 1;
    }
    out
}

fn rebase_html_attr_urls(text: &str, prefix: &str, file_key: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len() + 128);
    let mut i = 0;
    while i < bytes.len() {
        if is_attr_name(bytes, i, b"src") || is_attr_name(bytes, i, b"href") {
            if let Some((consumed, rewritten)) = rewrite_quoted_attr(text, i, prefix, file_key) {
                out.push_str(&rewritten);
                i += consumed;
                continue;
            }
        }
        let ch = text[i..].chars().next().expect("index is in-bounds");
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn is_attr_name(bytes: &[u8], i: usize, name: &[u8]) -> bool {
    let end = i + name.len();
    if end > bytes.len() || !bytes[i..end].eq_ignore_ascii_case(name) {
        return false;
    }
    if i > 0 {
        let prev = bytes[i - 1];
        if !prev.is_ascii_whitespace() && prev != b'<' {
            return false;
        }
    }
    matches!(
        bytes.get(end),
        Some(b'=') | Some(b' ') | Some(b'\t') | Some(b'\n') | Some(b'\r')
    )
}

/// From the start of an attribute name, copy through the closing quote,
/// rewriting the URL. `None` when the attribute is not a quoted value, so
/// the caller copies bytes through unchanged.
fn rewrite_quoted_attr(
    text: &str,
    i: usize,
    prefix: &str,
    file_key: &str,
) -> Option<(usize, String)> {
    let bytes = text.as_bytes();
    let mut j = i;
    while j < bytes.len() && bytes[j].is_ascii_alphabetic() {
        j += 1;
    }
    while j < bytes.len() && bytes[j].is_ascii_whitespace() {
        j += 1;
    }
    if j >= bytes.len() || bytes[j] != b'=' {
        return None;
    }
    j += 1;
    while j < bytes.len() && bytes[j].is_ascii_whitespace() {
        j += 1;
    }
    let quote = *bytes.get(j)?;
    if quote != b'"' && quote != b'\'' {
        return None;
    }
    j += 1;
    let value_start = j;
    while j < bytes.len() && bytes[j] != quote {
        j += 1;
    }
    if j >= bytes.len() {
        return None;
    }
    let value = &text[value_start..j];
    let mut out = String::from(&text[i..value_start]);
    out.push_str(&rebase_url_value(value, prefix, file_key));
    out.push(quote as char);
    Some((j + 1 - i, out))
}

fn rebase_url_value(value: &str, prefix: &str, file_key: &str) -> String {
    let trimmed = value.trim();
    if skip_external_url(trimmed) {
        return value.to_string();
    }
    let prefix = prefix.trim_end_matches('/');
    if trimmed.starts_with('/') {
        if trimmed == prefix || trimmed.starts_with(&format!("{prefix}/")) {
            return value.to_string();
        }
        return format!("{prefix}{trimmed}");
    }
    let resolved = resolve_relative(file_key, trimmed);
    if resolved.is_empty() {
        return value.to_string();
    }
    format!("{prefix}/{resolved}")
}

fn skip_external_url(value: &str) -> bool {
    value.is_empty()
        || value.starts_with('#')
        || value.starts_with('?')
        || value.starts_with("data:")
        || value.starts_with("mailto:")
        || value.starts_with("tel:")
        || value.starts_with("javascript:")
        || value.starts_with("//")
        || value.contains("://")
}

/// Resolves `relative` against the directory of `file_key` the way a
/// browser would if the page URL *did* have a trailing slash -- which is
/// the location the file actually occupies in the artifact.
fn resolve_relative(file_key: &str, relative: &str) -> String {
    let (path, suffix) = match relative.find(['?', '#']) {
        Some(i) => (&relative[..i], &relative[i..]),
        None => (relative, ""),
    };
    let dir = match file_key.rsplit_once('/') {
        Some((dir, _)) => dir,
        None => "",
    };
    let mut stack: Vec<&str> = dir.split('/').filter(|s| !s.is_empty()).collect();
    for seg in path.split('/') {
        if seg.is_empty() || seg == "." {
            continue;
        }
        if seg == ".." {
            stack.pop();
            continue;
        }
        stack.push(seg);
    }
    let mut out = stack.join("/");
    out.push_str(suffix);
    out
}

/// Rewrites root-absolute `url(/...)` in a stylesheet. Relative `url(x)`
/// is left alone: a CSS file is served with a filename, so the browser
/// already resolves those against the file's directory, which is correct.
fn rebase_css_urls(input: &[u8], prefix: &str) -> Vec<u8> {
    if prefix.is_empty() {
        return input.to_vec();
    }
    let Ok(text) = std::str::from_utf8(input) else {
        return input.to_vec();
    };
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len() + 64);
    let mut i = 0;
    while i < bytes.len() {
        if i + 4 <= bytes.len() && bytes[i..i + 4].eq_ignore_ascii_case(b"url(") {
            out.push_str(&text[i..i + 4]);
            i += 4;
            let ws_start = i;
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            out.push_str(&text[ws_start..i]);
            let quote = bytes.get(i).copied().filter(|b| *b == b'"' || *b == b'\'');
            if let Some(q) = quote {
                out.push(q as char);
                i += 1;
            }
            let val_start = i;
            let val_end = match quote {
                Some(q) => text[i..].find(q as char).map(|n| i + n).unwrap_or(text.len()),
                None => {
                    let mut k = i;
                    while k < bytes.len() && bytes[k] != b')' && !bytes[k].is_ascii_whitespace() {
                        k += 1;
                    }
                    k
                }
            };
            let value = &text[val_start..val_end];
            if value.starts_with('/') && !value.starts_with("//") {
                let p = prefix.trim_end_matches('/');
                if value == p || value.starts_with(&format!("{p}/")) {
                    out.push_str(value);
                } else {
                    out.push_str(p);
                    out.push_str(value);
                }
            } else {
                out.push_str(value);
            }
            i = val_end;
            continue;
        }
        let ch = text[i..].chars().next().expect("index is in-bounds");
        out.push(ch);
        i += ch.len_utf8();
    }
    out.into_bytes()
}

fn extension(name: &str) -> Option<String> {
    name.rsplit_once('.').map(|(_, ext)| ext.to_ascii_lowercase())
}

fn check_extension(name: &str) -> Result<(), String> {
    match extension(name) {
        Some(ext) if PUBLISHABLE.contains(&ext.as_str()) => Ok(()),
        Some(ext) => Err(format!(".{ext} cannot be published")),
        None => Err(format!("{name} has no extension")),
    }
}

/// Reject a set that cannot be published before any of it is uploaded.
fn check_limits(files: &[Candidate]) -> Result<u64, String> {
    if files.len() > MAX_FILES {
        return Err(format!(
            "{} files; at most {MAX_FILES} can be published at once",
            files.len()
        ));
    }
    let mut total = 0u64;
    for file in files {
        if file.size > MAX_FILE_BYTES {
            return Err(format!(
                "{} is {:.1} MB; the per-file limit is {} MB",
                file.key,
                file.size as f64 / 1_048_576.0,
                MAX_FILE_BYTES / 1_048_576
            ));
        }
        total += file.size;
    }
    if total > MAX_TOTAL_BYTES {
        return Err(format!(
            "{:.1} MB in total; the limit is {} MB",
            total as f64 / 1_048_576.0,
            MAX_TOTAL_BYTES / 1_048_576
        ));
    }
    Ok(total)
}

/// Publish `path` and return the link.
///
/// `endpoint` is the signing URL. Configurable rather than compiled in so a
/// fork, an internal mirror, or a self-hosted bucket can be pointed at without
/// rebuilding -- and so this is testable against a local server.
pub async fn publish(path: &Path, endpoint: &str) -> Result<Published, String> {
    if endpoint.trim().is_empty() {
        return Err(
            "no artifact endpoint is configured. Set `endpoint` under [artifacts] in \
             ~/.boxcode/config.toml."
                .to_string(),
        );
    }
    let files = collect(path)?;
    let total = check_limits(&files)?;

    let manifest: Vec<ManifestEntry> = files
        .iter()
        .map(|f| ManifestEntry { path: f.key.clone(), size: f.size })
        .collect();
    // Republishing the same path sends back the id it got last time, so the
    // signer reuses that S3 prefix instead of minting a new one -- an unknown
    // or missing id is treated by the signer as a fresh publish, so there is
    // nothing here to validate before sending it.
    let mut request = serde_json::json!({ "files": manifest });
    if let Some(id) = remembered_id(path) {
        request["id"] = serde_json::Value::String(id);
    }
    let body = request.to_string();

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .user_agent(concat!("boxcode/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| format!("could not build an HTTP client: {e}"))?;

    let response = client
        .post(endpoint)
        .header("content-type", "application/json")
        // Required when the endpoint sits behind CloudFront with origin access
        // control: OAC signs the request including a hash of the body, and
        // without this header the signature cannot be reproduced and every
        // request is rejected as a mismatch. Harmless anywhere else.
        .header("x-amz-content-sha256", sha256_hex(body.as_bytes()))
        .body(body)
        .send()
        .await
        .map_err(|e| format!("could not reach the artifact service: {e}"))?;

    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!("the artifact service refused this ({status}): {}", text.trim()));
    }
    let signed: SignResponse = serde_json::from_str(&text)
        .map_err(|e| format!("the artifact service returned something unexpected ({e})"))?;

    // Captured so the post-upload check below has something concrete to
    // compare the live URL against -- `None` if this call's upload batch
    // did not include `index.html` (a diffing signer may only have asked
    // for the files that actually changed), in which case there is nothing
    // fresh to verify against and `verify_live` degrades to a reachability
    // check.
    let mut index_len = None;
    let prefix = served_under(&signed.url);
    for upload in &signed.uploads {
        let candidate = files
            .iter()
            .find(|f| f.key == upload.path)
            .ok_or_else(|| format!("the service asked for a file that was not offered: {}", upload.path))?;
        let bytes = std::fs::read(&candidate.source)
            .map_err(|e| format!("could not read {}: {e}", candidate.source.display()))?;
        let bytes = if is_html(&candidate.key) {
            // Rebase before injecting, so the reload script is never itself a
            // candidate for rewriting.
            inject_live_reload(rebase_page_urls(&bytes, &prefix, &candidate.key))
        } else if extension(&candidate.key).as_deref() == Some("css") {
            rebase_css_urls(&bytes, &prefix)
        } else {
            bytes
        };
        if candidate.key == "index.html" {
            index_len = Some(bytes.len());
        }
        let put = client
            .put(&upload.url)
            .header("content-type", &upload.content_type)
            // Not part of the presigned signature (the signer never required
            // it, so old and new clients both still upload fine either way),
            // but S3 stores it as the object's metadata regardless, and
            // CloudFront honors it from there. Without it, a same-URL update
            // publishes correctly to S3 but can sit behind a day-old cached
            // copy at the edge -- confirmed live: CloudFront serves the new
            // bytes on the very next request once this header is set.
            .header("cache-control", "no-cache")
            .body(bytes)
            .send()
            .await
            .map_err(|e| format!("could not upload {}: {e}", upload.path))?;
        if !put.status().is_success() {
            return Err(format!("uploading {} failed ({})", upload.path, put.status()));
        }
    }

    let verified = verify_live(&client, &signed.url, index_len).await;

    remember(path, &signed.id);
    Ok(Published {
        url: signed.url,
        files: files.len(),
        bytes: total,
        expires_in_hours: signed.expires_in_hours,
        verified,
    })
}

/// Confirms the just-published URL actually serves what was just uploaded,
/// instead of trusting a successful presigned `PUT` as proof on its own --
/// that only shows S3 accepted the bytes, not that `url` (what a developer
/// or visitor actually opens) serves them. Best-effort and deliberately
/// never turns into an `Err`: a flaky read-back (a transient network error,
/// a CDN edge that has not yet propagated) must not make a real publish
/// come back as a reported failure. Same "no-cache" header set on every
/// upload above is what makes this check meaningful the moment it runs,
/// not just eventually -- see that header's own comment.
async fn verify_live(client: &reqwest::Client, url: &str, expected_index_len: Option<usize>) -> bool {
    // Bounded well under the client's own 60s request timeout: this check
    // exists to make a publish more trustworthy, not to let a slow CDN edge
    // make every publish take up to a minute longer. A timeout here reports
    // as unverified, same as any other failure -- never as an error the
    // whole publish has to fail over.
    let Ok(response) = client.get(url).timeout(Duration::from_secs(10)).send().await else {
        return false;
    };
    if !response.status().is_success() {
        return false;
    }
    let Ok(body) = response.bytes().await else {
        return false;
    };
    if expected_index_len.is_some_and(|expected| body.len() != expected) {
        return false;
    }
    // Not text: there is nothing to read references out of, and the fetch
    // above is all that can honestly be claimed.
    let Ok(html) = std::str::from_utf8(&body) else {
        return true;
    };

    // The page loading is not the page working. A build that assumes it owns
    // the domain root serves a perfectly good index.html whose every asset
    // 404s -- a blank screen that reported as confirmed, which is the exact
    // failure this walk exists to catch. Asking for what the served markup
    // asks for is the only way to know.
    for asset in referenced_assets(html, url).into_iter().take(MAX_VERIFIED_ASSETS) {
        let ok = client
            .get(&asset)
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false);
        if !ok {
            return false;
        }
    }
    true
}

/// The asset URLs a browser would fetch for `html` served at `page_url`,
/// resolved the way a browser resolves them.
///
/// `src` is always an asset. `href` is usually a link, so it counts only when
/// it names a stylesheet -- checking every `<a href>` would turn the app's own
/// routes into evidence about this publish, and a single-page app's routes do
/// not exist as files at all.
///
/// Relative references are resolved against the page's *directory*, dropping
/// the last segment exactly as a browser does. That is not a detail: the
/// artifact URL carries no trailing slash, so `./assets/app.js` on
/// `/artifacts/k9depef6` really does resolve to `/artifacts/assets/app.js`,
/// and a check that quietly resolved it the convenient way would confirm a
/// page that does not work.
fn referenced_assets(html: &str, page_url: &str) -> Vec<String> {
    let (origin, dir) = split_origin_and_dir(page_url);
    let mut out = Vec::new();
    for (attr, quote, stylesheets_only) in [
        ("src=\"", '"', false),
        ("src='", '\'', false),
        ("href=\"", '"', true),
        ("href='", '\'', true),
    ] {
        for piece in html.split(attr).skip(1) {
            let Some(raw) = piece.split(quote).next().map(str::trim) else {
                continue;
            };
            if stylesheets_only && !raw.split('?').next().unwrap_or(raw).ends_with(".css") {
                continue;
            }
            // Another origin, an inline payload, or an in-page anchor: none of
            // them say anything about whether this publish is serving.
            if raw.is_empty()
                || raw.starts_with("//")
                || raw.starts_with('#')
                || raw.contains("://")
                || raw.starts_with("data:")
                || raw.starts_with("mailto:")
            {
                continue;
            }
            let resolved = if let Some(path) = raw.strip_prefix('/') {
                format!("{origin}/{path}")
            } else {
                format!("{origin}{dir}{}", raw.trim_start_matches("./"))
            };
            if !out.contains(&resolved) {
                out.push(resolved);
            }
        }
    }
    out
}

/// Splits `https://host/artifacts/id` into `("https://host", "/artifacts/")`
/// -- the origin, and the directory relative references resolve against.
fn split_origin_and_dir(url: &str) -> (String, String) {
    let (scheme, rest) = url.split_once("://").unwrap_or(("https", url));
    let (host, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let dir = match path.rfind('/') {
        Some(i) => &path[..=i],
        None => "/",
    };
    (format!("{scheme}://{host}"), dir.to_string())
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole fix, end to end, against the real service: publish a dist
    /// shaped exactly like a default `vite build` and confirm a browser
    /// opening the returned URL can actually fetch the assets the served HTML
    /// asks for. This is the check that would have caught the blank page --
    /// `verify_live` fetches index.html and stops there, so a publish whose
    /// every asset 404s still reports as confirmed.
    ///
    /// Ignored by default: it needs the network and publishes a real (tiny,
    /// 48h) artifact, neither of which belongs in an ordinary `cargo test`.
    /// Run it deliberately with
    /// `cargo test --bin boxcode publishing_a_vite_dist -- --ignored --nocapture`.
    #[tokio::test]
    #[ignore]
    async fn publishing_a_vite_dist_serves_assets_a_browser_can_actually_fetch() {
        let dir = temp("vite-e2e");
        write(
            &dir,
            "index.html",
            r#"<!doctype html><html><head>
<script type="module" crossorigin src="/assets/app.js"></script>
<link rel="stylesheet" crossorigin href="/assets/app.css">
</head><body><div id="root"></div></body></html>"#,
        );
        write(&dir, "assets/app.js", "console.log('hello from the bundle');\n");
        write(&dir, "assets/app.css", "body{background:#fff;color:#111}\n");

        let published = publish(&dir, "https://boxcode.sh/api/artifact")
            .await
            .expect("publish should succeed");
        println!("published to {}", published.url);
        assert_eq!(published.files, 3);

        let client = reqwest::Client::new();
        let html = client
            .get(&published.url)
            .send()
            .await
            .expect("fetch index")
            .text()
            .await
            .expect("body");

        // Whatever the served HTML asks for, ask for it the same way a browser
        // would -- resolved against the origin, not against our own idea of
        // where the files went.
        let mut checked = 0;
        for attr in ["src=\"", "href=\""] {
            for piece in html.split(attr).skip(1) {
                let Some(url) = piece.split('"').next() else { continue };
                if !url.starts_with('/') || url.starts_with("//") {
                    continue;
                }
                let absolute = format!("https://boxcode.sh{url}");
                let status = client
                    .get(&absolute)
                    .send()
                    .await
                    .expect("fetch asset")
                    .status();
                println!("  {status}  {absolute}");
                assert!(
                    status.is_success(),
                    "the served page asks for {absolute}, which does not resolve -- \
                     this is the blank page"
                );
                checked += 1;
            }
        }
        assert_eq!(checked, 2, "both the script and the stylesheet were checked");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// The check, against the two real pages this bug produced: one published
    /// before the rebase existed (blank, every asset 404s) and one published
    /// after it (working). The first must come back unverified -- it reported
    /// as "confirmed live" under the old check, which is what let a blank page
    /// ship looking like a success.
    ///
    /// Ignored by default: it needs the network and two fixed artifact ids.
    /// Both expire 48h after they were published, so a failure here long after
    /// the fact means "the links aged out", not "the code regressed".
    /// `cargo test --bin boxcode verify_live_against_the_real -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn verify_live_against_the_real_broken_and_fixed_pages() {
        let client = reqwest::Client::new();

        let broken = "https://boxcode.sh/artifacts/k9depef6";
        let fixed = "https://boxcode.sh/artifacts/25menr6r";

        let broken_ok = verify_live(&client, broken, None).await;
        let fixed_ok = verify_live(&client, fixed, None).await;
        println!("  broken page {broken} -> verified={broken_ok}");
        println!("  fixed  page {fixed} -> verified={fixed_ok}");

        assert!(
            !broken_ok,
            "the page whose assets all 404 must not report as confirmed live"
        );
        assert!(fixed_ok, "the rebased page serves its assets and must confirm");
    }

    // ---- what the post-publish check actually looks at -----------------------

    #[test]
    fn the_origin_and_directory_come_apart_the_way_a_browser_splits_them() {
        assert_eq!(
            split_origin_and_dir("https://boxcode.sh/artifacts/k9depef6"),
            ("https://boxcode.sh".to_string(), "/artifacts/".to_string())
        );
        assert_eq!(
            split_origin_and_dir("https://boxcode.sh/artifacts/k9depef6/"),
            ("https://boxcode.sh".to_string(), "/artifacts/k9depef6/".to_string())
        );
        assert_eq!(
            split_origin_and_dir("https://example.test"),
            ("https://example.test".to_string(), "/".to_string())
        );
    }

    /// The rebased page: every asset sits under the artifact prefix, and that
    /// is what the check goes and asks for.
    #[test]
    fn a_rebased_page_is_checked_at_its_real_locations() {
        let html = r#"<script src="/artifacts/k9depef6/assets/app.js"></script>
<link rel="stylesheet" href="/artifacts/k9depef6/assets/app.css">"#;

        assert_eq!(
            referenced_assets(html, "https://boxcode.sh/artifacts/k9depef6"),
            vec![
                "https://boxcode.sh/artifacts/k9depef6/assets/app.js".to_string(),
                "https://boxcode.sh/artifacts/k9depef6/assets/app.css".to_string(),
            ]
        );
    }

    /// The bug this exists to catch: an unrebased Vite build. The check must
    /// ask for the domain-root path the browser would ask for -- the one that
    /// 404s -- not the path where the file happens to live.
    #[test]
    fn an_unrebased_page_is_checked_where_the_browser_would_actually_look() {
        let html = r#"<script src="/assets/index-BvuHFN-e.js"></script>"#;
        assert_eq!(
            referenced_assets(html, "https://boxcode.sh/artifacts/k9depef6"),
            vec!["https://boxcode.sh/assets/index-BvuHFN-e.js".to_string()],
            "resolving this to where the file really is would confirm a blank page"
        );
    }

    /// A relative reference on a URL with no trailing slash resolves one
    /// directory too high. Resolving it the convenient way instead would hide
    /// exactly the failure worth reporting.
    #[test]
    fn relative_references_resolve_against_the_directory_not_the_page() {
        let html = r#"<script src="./assets/app.js"></script><img src="logo.png">"#;
        assert_eq!(
            referenced_assets(html, "https://boxcode.sh/artifacts/k9depef6"),
            vec![
                "https://boxcode.sh/artifacts/assets/app.js".to_string(),
                "https://boxcode.sh/artifacts/logo.png".to_string(),
            ]
        );
    }

    /// Other origins, inline data and in-page anchors say nothing about
    /// whether this publish is serving, so none of them are fetched.
    #[test]
    fn other_origins_and_non_files_are_not_checked() {
        let html = r##"<link href="//fonts.googleapis.com/css2?family=Figtree" rel="stylesheet">
<link href="https://cdn.test/x.css" rel="stylesheet">
<img src="data:image/png;base64,AAAA">
<a href="#top">top</a>"##;
        assert!(referenced_assets(html, "https://boxcode.sh/artifacts/abc").is_empty());
    }

    /// A single-page app's routes are not files. Treating `<a href="/admin">`
    /// as an asset would report every SPA as broken.
    #[test]
    fn app_routes_are_not_mistaken_for_assets() {
        let html = r#"<a href="/admin/orders">Orders</a><a href="/">Home</a>
<link rel="stylesheet" href="/artifacts/abc/assets/app.css">"#;
        assert_eq!(
            referenced_assets(html, "https://boxcode.sh/artifacts/abc"),
            vec!["https://boxcode.sh/artifacts/abc/assets/app.css".to_string()],
            "only the stylesheet is a file; the routes are the app's business"
        );
    }

    /// Single quotes are as valid as double, and the same asset named twice
    /// is fetched once.
    #[test]
    fn quoting_styles_are_both_read_and_duplicates_collapse() {
        let html = r#"<script src='/artifacts/abc/a.js'></script>
<script src="/artifacts/abc/a.js"></script>"#;
        assert_eq!(
            referenced_assets(html, "https://boxcode.sh/artifacts/abc"),
            vec!["https://boxcode.sh/artifacts/abc/a.js".to_string()]
        );
    }

    /// A cache-busting query must not stop a stylesheet being recognised.
    #[test]
    fn a_stylesheet_with_a_query_string_is_still_a_stylesheet() {
        let html = r#"<link rel="stylesheet" href="/artifacts/abc/a.css?v=2">"#;
        assert_eq!(
            referenced_assets(html, "https://boxcode.sh/artifacts/abc"),
            vec!["https://boxcode.sh/artifacts/abc/a.css?v=2".to_string()]
        );
    }

    // ---- serving under a sub-path ------------------------------------------

    #[test]
    fn the_served_path_is_read_off_the_signed_url() {
        assert_eq!(served_under("https://boxcode.sh/artifacts/k9depef6"), "/artifacts/k9depef6");
        assert_eq!(served_under("https://boxcode.sh/artifacts/k9depef6/"), "/artifacts/k9depef6");
        assert_eq!(served_under("https://example.test/a/b/c"), "/a/b/c");
        // A signer serving from the domain root has no prefix to add, and
        // rebasing must then be a no-op rather than inventing one.
        assert_eq!(served_under("https://example.test"), "");
    }

    /// The exact failure a real publish hit: Vite's default `base: "/"` emits
    /// root-absolute asset URLs, the artifact is served from a sub-path, and
    /// every asset 404s into a blank page while the upload reports success.
    #[test]
    fn a_vite_build_serves_its_assets_from_the_artifact_prefix() {
        let html = br#"<!doctype html>
<html><head>
<script type="module" crossorigin src="/assets/index-BvuHFN-e.js"></script>
<link rel="stylesheet" crossorigin href="/assets/index-8NRPCoXr.css">
</head><body><div id="root"></div></body></html>"#;

        let out = rebase_absolute_urls(html, "/artifacts/k9depef6");
        let text = String::from_utf8(out).expect("still valid utf-8");

        assert!(
            text.contains(r#"src="/artifacts/k9depef6/assets/index-BvuHFN-e.js""#),
            "{text}"
        );
        assert!(
            text.contains(r#"href="/artifacts/k9depef6/assets/index-8NRPCoXr.css""#),
            "{text}"
        );
        assert!(!text.contains(r#"src="/assets/"#), "the broken form must be gone: {text}");
    }

    /// Another origin's URL is not ours to rewrite. Protocol-relative `//host`
    /// is the one that looks like a root-absolute path and is not one --
    /// getting this wrong would break every CDN font and script on the page.
    #[test]
    fn other_origins_are_left_alone() {
        let html = br#"<link href="//fonts.googleapis.com/css2?family=Figtree" rel="stylesheet">
<link href="https://fonts.gstatic.com/x.woff2" rel="preconnect">
<script src="http://example.test/a.js"></script>"#;

        let out = rebase_absolute_urls(html, "/artifacts/abc");
        let text = String::from_utf8(out).unwrap();

        assert!(text.contains(r#"href="//fonts.googleapis.com"#), "{text}");
        assert!(text.contains(r#"href="https://fonts.gstatic.com"#), "{text}");
        assert!(text.contains(r#"src="http://example.test"#), "{text}");
        assert!(!text.contains("/artifacts/abc"), "nothing should have been rewritten: {text}");
    }

    /// Relative URLs do *not* resolve correctly on an artifact URL: there is
    /// no trailing slash, so the browser drops the id. They have to become
    /// prefix-absolute the same way Vite's `/assets/` URLs do. Rewriting
    /// from the file on disk (never from previously uploaded HTML) means a
    /// republish cannot double the prefix.
    #[test]
    fn relative_urls_are_rewritten_to_the_artifact_prefix() {
        let html = br#"<link rel="stylesheet" href="style.css"><img src="./a/b.png"><script src="assets/app.js">"#;
        let out = rebase_absolute_urls(html, "/artifacts/abc");
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains(r#"href="/artifacts/abc/style.css""#), "{text}");
        assert!(text.contains(r#"src="/artifacts/abc/a/b.png""#), "{text}");
        assert!(text.contains(r#"src="/artifacts/abc/assets/app.js""#), "{text}");
        // After rewrite, a browser opening the artifact URL fetches the
        // stylesheet from the prefix -- the path that actually exists --
        // instead of one directory too high.
        assert_eq!(
            referenced_assets(&text, "https://boxcode.sh/artifacts/abc"),
            vec![
                "https://boxcode.sh/artifacts/abc/a/b.png".to_string(),
                "https://boxcode.sh/artifacts/abc/assets/app.js".to_string(),
                "https://boxcode.sh/artifacts/abc/style.css".to_string(),
            ]
        );
    }

    /// A nested page's `../style.css` must land at the artifact root, not
    /// at `/artifacts/id/../style.css` (which is `/artifacts/style.css`).
    #[test]
    fn nested_relative_urls_resolve_against_the_file_not_the_prefix() {
        let html = br#"<link rel="stylesheet" href="../style.css"><img src="./photo.png">"#;
        let out = rebase_page_urls(html, "/artifacts/abc", "pages/about.html");
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains(r#"href="/artifacts/abc/style.css""#), "{text}");
        assert!(text.contains(r#"src="/artifacts/abc/pages/photo.png""#), "{text}");
    }

    /// A stylesheet that assumes it owns the domain root 404s its fonts
    /// and images the same way Vite 404s `/assets/`. Relative `url(x)` is
    /// already correct (the CSS file has a filename, so the directory is
    /// the artifact prefix) and must not be rewritten.
    #[test]
    fn root_absolute_urls_inside_css_are_rebased() {
        let css = br#"@import url("/fonts/figtree.css");
body{background:url(/img/bg.png)}
h1{background:url(logo.png)}"#;
        let out = rebase_css_urls(css, "/artifacts/abc");
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains(r#"url("/artifacts/abc/fonts/figtree.css")"#), "{text}");
        assert!(text.contains("url(/artifacts/abc/img/bg.png)"), "{text}");
        assert!(text.contains("url(logo.png)"), "relative css urls must be left alone: {text}");
    }

    /// Republishing HTML that was already rebased (or a page that linked
    /// with the prefix already in it) must not stack a second copy.
    #[test]
    fn an_already_prefixed_url_is_not_prefixed_again() {
        let html = br#"<link href="/artifacts/abc/style.css" rel="stylesheet">"#;
        let out = rebase_absolute_urls(html, "/artifacts/abc");
        let text = String::from_utf8(out).unwrap();
        assert_eq!(text.matches("/artifacts/abc").count(), 1, "{text}");
    }

    /// Single quotes are as valid as double in HTML, and a bare `href="/"`
    /// (a home link) is a root-absolute URL like any other.
    #[test]
    fn single_quotes_and_bare_roots_are_handled() {
        let html = br#"<script src='/a.js'></script><a href="/">home</a>"#;
        let out = rebase_absolute_urls(html, "/artifacts/abc");
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains(r#"src='/artifacts/abc/a.js'"#), "{text}");
        assert!(text.contains(r#"href="/artifacts/abc/""#), "{text}");
    }

    /// A root-served signer adds no prefix, so the bytes come back identical.
    #[test]
    fn an_empty_prefix_changes_nothing() {
        let html = br#"<script src="/assets/a.js"></script>"#;
        assert_eq!(rebase_absolute_urls(html, ""), html.to_vec());
    }

    /// Bytes that are not text pass through untouched rather than being
    /// corrupted, the same guarantee `inject_live_reload` makes.
    #[test]
    fn non_utf8_bytes_survive_rebasing() {
        let bytes = [0xff, 0xfe, b'=', b'"', b'/', b'a', 0x00];
        let out = rebase_absolute_urls(&bytes, "/p");
        // The rewrite still applies (it is byte-wise), and nothing else moved.
        assert_eq!(out, vec![0xff, 0xfe, b'=', b'"', b'/', b'p', b'/', b'a', 0x00]);
    }

    /// The two transforms compose: assets are rebased and the reload script is
    /// still injected, and the script itself is not rewritten by the rebase.
    #[test]
    fn rebasing_and_live_reload_compose() {
        let html = br#"<html><body><script src="/assets/a.js"></script></body></html>"#;
        let out = inject_live_reload(rebase_absolute_urls(html, "/artifacts/abc"));
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains(r#"src="/artifacts/abc/assets/a.js""#), "{text}");
        assert!(text.contains("location.reload"), "the reload script survived: {text}");
    }

    fn write(dir: &Path, name: &str, contents: &str) {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("mkdir");
        }
        std::fs::write(path, contents).expect("write");
    }

    fn temp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("boxcode-artifact-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    // ---- inject_live_reload -------------------------------------------

    #[test]
    fn inserts_before_a_lowercase_closing_body_tag() {
        let out = inject_live_reload(b"<html><body><h1>hi</h1></body></html>".to_vec());
        let out = String::from_utf8(out).unwrap();
        assert!(out.starts_with("<html><body><h1>hi</h1>"), "{out}");
        assert!(out.ends_with("</body></html>"), "{out}");
        assert!(out.contains(LIVE_RELOAD_SCRIPT), "{out}");
    }

    #[test]
    fn matches_the_closing_body_tag_case_insensitively() {
        let out = inject_live_reload(b"<HTML><BODY>hi</BODY></HTML>".to_vec());
        let out = String::from_utf8(out).unwrap();
        // The original casing of the tag itself is preserved -- only the
        // search for *where* to insert is case-insensitive.
        assert!(out.ends_with("</BODY></HTML>"), "{out}");
        assert!(out.contains(LIVE_RELOAD_SCRIPT), "{out}");
    }

    #[test]
    fn falls_back_to_before_closing_html_when_there_is_no_body_tag() {
        let out = inject_live_reload(b"<html><h1>hi</h1></html>".to_vec());
        let out = String::from_utf8(out).unwrap();
        assert!(out.ends_with("</html>"), "{out}");
        assert!(out.contains(LIVE_RELOAD_SCRIPT), "{out}");
    }

    #[test]
    fn appends_at_the_end_when_neither_closing_tag_is_present() {
        // The real case this hits: a bare fragment, same shape as the live
        // end-to-end test's own fixture (`<h1>live</h1><script ...>`).
        let out = inject_live_reload(b"<h1>fragment, no wrapper tags</h1>".to_vec());
        let out = String::from_utf8(out).unwrap();
        assert!(out.ends_with(LIVE_RELOAD_SCRIPT), "{out}");
    }

    #[test]
    fn non_utf8_bytes_are_returned_unchanged_rather_than_corrupted() {
        let invalid = vec![0x68, 0x69, 0xff, 0xfe]; // "hi" + invalid UTF-8
        let out = inject_live_reload(invalid.clone());
        assert_eq!(out, invalid);
    }

    /// Republishing a path recovers the id it got last time, so `publish`
    /// can send it back and update the same link. A path seen for the first
    /// time has nothing to recover.
    #[test]
    fn a_republished_path_remembers_its_last_id() {
        crate::config::test_support::with_isolated_home(|| {
            let dir = temp("remember");
            let target = dir.join("index.html");
            write(&dir, "index.html", "hi");

            assert!(remembered_id(&target).is_none(), "never published before");

            remember(&target, "abc12345");
            assert_eq!(remembered_id(&target).as_deref(), Some("abc12345"));

            // A later publish's id replaces the old one for this same path.
            remember(&target, "zzz98765");
            assert_eq!(remembered_id(&target).as_deref(), Some("zzz98765"));

            let _ = std::fs::remove_dir_all(&dir);
        });
    }

    /// The case `remembered_id` alone gets wrong: a project published as a
    /// single file registers under that file's path, never equal to the
    /// directory `Workspace::new` resolves the same file to -- only nested
    /// under it. `any_published_under` has to see through that.
    #[test]
    fn any_published_under_finds_a_file_published_inside_the_root() {
        crate::config::test_support::with_isolated_home(|| {
            let dir = temp("published-under");
            let file = dir.join("todo.html");
            write(&dir, "todo.html", "hi");

            assert!(!any_published_under(&dir), "nothing published yet");

            remember(&file, "hs3c6cb7");
            assert!(any_published_under(&dir), "todo.html is published, root should see it");
            assert!(any_published_under(&file), "the exact published path itself still counts");

            let _ = std::fs::remove_dir_all(&dir);
        });
    }

    /// An unrelated sibling directory that merely shares a path prefix
    /// (`.../boxcode-other`) must not be mistaken for a project nested
    /// inside `.../boxcode` -- `Path::starts_with` is component-aware for
    /// exactly this reason, confirmed here rather than assumed.
    #[test]
    fn any_published_under_does_not_match_a_sibling_with_a_shared_prefix() {
        crate::config::test_support::with_isolated_home(|| {
            let dir = temp("published-under-sibling");
            std::fs::create_dir_all(&dir).unwrap();
            let root = dir.join("boxcode");
            let sibling = dir.join("boxcode-other");
            std::fs::create_dir_all(&root).unwrap();
            write(&sibling, "todo.html", "hi");

            remember(&sibling.join("todo.html"), "zzz99999");
            assert!(!any_published_under(&root), "boxcode-other is not under boxcode");

            let _ = std::fs::remove_dir_all(&dir);
        });
    }

    /// `/pull`'s picker needs every locally published project, not just one
    /// looked up by path -- newest first, so the most recent work a
    /// developer switched away from is always the top entry.
    #[test]
    fn all_local_lists_every_published_project_newest_first() {
        crate::config::test_support::with_isolated_home(|| {
            assert!(all_local().is_empty(), "nothing published yet");

            let dir = temp("all-local");
            let b = dir.join("b-project/index.html");
            let a = dir.join("a-project/index.html");
            write(&dir, "b-project/index.html", "hi");
            write(&dir, "a-project/index.html", "hi");

            remember(&b, "bproj123");
            remember(&a, "aproj456");

            let all = all_local();
            assert_eq!(all.len(), 2);
            // Newest first, not path order -- "a-project" was remembered
            // second so it leads even though its path sorts first.
            assert!(all[0].0.ends_with("a-project/index.html"), "{all:?}");
            assert_eq!(all[0].1, "aproj456");
            assert!(all[1].0.ends_with("b-project/index.html"), "{all:?}");
            assert_eq!(all[1].1, "bproj123");

            let _ = std::fs::remove_dir_all(&dir);
        });
    }

    /// The whole point of bounding `/pull` to `EXPIRY_HOURS`: an entry for a
    /// link that has already died on the server must not linger in the list
    /// forever just because the local registry file never forgets on its own.
    #[test]
    fn all_local_drops_entries_older_than_the_link_lifetime() {
        crate::config::test_support::with_isolated_home(|| {
            let dir = temp("all-local-expiry");
            let stale = dir.join("stale/index.html");
            let fresh = dir.join("fresh/index.html");
            write(&dir, "stale/index.html", "hi");
            write(&dir, "fresh/index.html", "hi");

            remember(&fresh, "fresh123");
            // Backdate `stale` past the cutoff directly in the registry file,
            // since `remember` always stamps "now" -- same seam the /pull
            // picker test in app.rs uses to seed the registry.
            let reg_path = crate::config::Config::config_dir().join("artifacts.json");
            let mut map = load_registry();
            let stale_key = stale.canonicalize().expect("canonicalize").to_string_lossy().into_owned();
            map.insert(
                stale_key,
                RegistryEntry {
                    id: "stale999".to_string(),
                    published_at: now_secs() - (EXPIRY_HOURS as u64 * 3600) - 1,
                },
            );
            std::fs::write(&reg_path, serde_json::to_string(&map).expect("serialize")).expect("write");

            let all = all_local();
            assert_eq!(all.len(), 1, "{all:?}");
            assert_eq!(all[0].1, "fresh123");

            let _ = std::fs::remove_dir_all(&dir);
        });
    }

    /// A whole build directory, which is what an SPA actually is.
    #[test]
    fn a_directory_publishes_every_asset_with_forward_slashed_keys() {
        let dir = temp("spa");
        write(&dir, "index.html", "<div id=app></div>");
        write(&dir, "assets/app.js", "console.log(1)");
        write(&dir, "assets/app.css", "body{}");

        let mut keys: Vec<String> = collect(&dir).expect("collect").into_iter().map(|c| c.key).collect();
        keys.sort();
        assert_eq!(keys, vec!["assets/app.css", "assets/app.js", "index.html"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Pointing publish at a Vite/React project root after `npm run build`
    /// must upload `dist/` (or `build/`/`out/`), not the unbuilt template
    /// sitting at the root. Without this, a first preview is an unstyled
    /// scaffold even though the built CSS is on disk.
    #[test]
    fn a_js_project_root_publishes_its_build_output() {
        let dir = temp("js-root");
        write(&dir, "package.json", r#"{"name":"app","scripts":{"build":"vite build"}}"#);
        write(&dir, "index.html", "<script type=module src=/src/main.js></script>");
        write(&dir, "src/main.js", "import './style.css'");
        write(&dir, "dist/index.html", r#"<link rel="stylesheet" href="/assets/app.css">"#);
        write(&dir, "dist/assets/app.css", "body{}");

        let mut keys: Vec<String> = collect(&dir).expect("collect").into_iter().map(|c| c.key).collect();
        keys.sort();
        assert_eq!(
            keys,
            vec!["assets/app.css", "index.html"],
            "the unbuilt src/ tree must not be what is published: {keys:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A handmade HTML site with a package.json but no build output is
    /// still published from the root -- there is nothing else to pick.
    #[test]
    fn a_static_site_with_package_json_is_not_forced_into_a_missing_dist() {
        let dir = temp("static-pkg");
        write(&dir, "package.json", r#"{"name":"site"}"#);
        write(&dir, "index.html", r#"<link rel="stylesheet" href="style.css">"#);
        write(&dir, "style.css", "body{}");

        let mut keys: Vec<String> = collect(&dir).expect("collect").into_iter().map(|c| c.key).collect();
        keys.sort();
        assert_eq!(keys, vec!["index.html", "package.json", "style.css"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Publishing a build directory must not publish the secrets that happen
    /// to be sitting in it. This is the failure that would matter.
    #[test]
    fn dotfiles_and_dependency_directories_are_never_published() {
        let dir = temp("secrets");
        write(&dir, "index.html", "hi");
        write(&dir, ".env", "AWS_SECRET_ACCESS_KEY=hunter2");
        write(&dir, ".git/config", "[core]");
        write(&dir, "node_modules/left-pad/index.js", "module.exports=1");

        let keys: Vec<String> = collect(&dir).expect("collect").into_iter().map(|c| c.key).collect();
        assert_eq!(keys, vec!["index.html"], "got {keys:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A lone HTML file is renamed so the link opens the page itself; a lone
    /// data file keeps its name.
    #[test]
    fn a_single_html_file_becomes_the_index() {
        let dir = temp("single");
        write(&dir, "report.html", "<h1>hi</h1>");
        let files = collect(&dir.join("report.html")).expect("collect");
        assert_eq!(files[0].key, "index.html");

        // A lone data file keeps its name AND gains a viewer page, or the
        // link we hand over opens nothing.
        write(&dir, "data.csv", "a,b\n1,2\n");
        let files = collect(&dir.join("data.csv")).expect("collect");
        let keys: Vec<&str> = files.iter().map(|c| c.key.as_str()).collect();
        assert!(keys.contains(&"data.csv"), "{keys:?}");
        assert!(keys.contains(&"index.html"), "a lone CSV needs a viewer page: {keys:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Pointing at a project root instead of its build output is the common
    /// mistake, and "nothing to open" is a useless thing to discover after
    /// uploading 60 files.
    #[test]
    fn a_directory_with_no_index_is_refused_with_a_useful_message() {
        let dir = temp("noindex");
        write(&dir, "style.css", "body{}");
        let error = collect(&dir).expect_err("should refuse");
        assert!(error.contains("no index.html"), "{error}");
        assert!(error.contains("dist/"), "should say where to point instead: {error}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn oversized_sets_are_refused_before_anything_is_uploaded() {
        let files = vec![Candidate {
            source: PathBuf::from("big.png"),
            key: "big.png".to_string(),
            size: MAX_FILE_BYTES + 1,
        }];
        let error = check_limits(&files).expect_err("should refuse");
        assert!(error.contains("per-file limit"), "{error}");

        let many: Vec<Candidate> = (0..MAX_FILES + 1)
            .map(|i| Candidate { source: PathBuf::from("x"), key: format!("{i}.js"), size: 1 })
            .collect();
        assert!(check_limits(&many).expect_err("should refuse").contains("at most"));
    }

    #[test]
    fn unpublishable_extensions_are_rejected() {
        assert!(check_extension("app.exe").is_err());
        assert!(check_extension("noextension").is_err());
        assert!(check_extension("chart.svg").is_ok());
        assert!(check_extension("DATA.CSV").is_ok(), "extensions are case-insensitive");
    }

    /// Empty rather than a panic, and with a message that says what to set.
    #[tokio::test]
    async fn an_unconfigured_endpoint_explains_itself() {
        let dir = temp("noendpoint");
        write(&dir, "index.html", "hi");
        let error = publish(&dir, "  ").await.expect_err("should refuse");
        assert!(error.contains("[artifacts]"), "{error}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A minimal HTTP/1.1 GET-only server on a real socket: accepts one
    /// connection, ignores whatever request arrives (`verify_live` only
    /// ever sends a bare GET), and replies with the given status/body.
    /// Reused across the `verify_live` cases below rather than mocking the
    /// whole `publish` round trip, since the thing actually under test is
    /// this one read-back, not the upload flow already covered elsewhere.
    async fn serve_once(status_line: &'static str, body: &'static [u8]) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut buf = [0u8; 1024];
            let _ = socket.read(&mut buf).await;
            let response = [
                status_line.as_bytes(),
                format!("\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", body.len()).as_bytes(),
                body,
            ]
            .concat();
            let _ = socket.write_all(&response).await;
        });
        format!("http://{addr}")
    }

    /// The whole point of `verify_live`: a byte-length match against a
    /// freshly-uploaded `index.html` is what "actually serving it" means
    /// here, not just a 200 status.
    #[tokio::test]
    async fn verify_live_confirms_a_matching_index_html() {
        let url = serve_once("HTTP/1.1 200 OK", b"<h1>hi</h1>").await;
        let client = reqwest::Client::new();
        assert!(verify_live(&client, &url, Some(b"<h1>hi</h1>".len())).await);
    }

    /// A live URL that answers but with the wrong length is exactly the
    /// case this exists to catch: something reachable, just not what was
    /// just uploaded (stale cache, wrong prefix, a signer bug).
    #[tokio::test]
    async fn verify_live_flags_a_length_mismatch() {
        let url = serve_once("HTTP/1.1 200 OK", b"<h1>stale</h1>").await;
        let client = reqwest::Client::new();
        assert!(!verify_live(&client, &url, Some(999)).await);
    }

    /// An HTTP error status is not "reachable" for this purpose.
    #[tokio::test]
    async fn verify_live_flags_a_non_success_status() {
        let url = serve_once("HTTP/1.1 404 Not Found", b"").await;
        let client = reqwest::Client::new();
        assert!(!verify_live(&client, &url, None).await);
    }

    /// Nothing listening at all -- the network-error path, distinct from a
    /// server that responds but wrongly.
    #[tokio::test]
    async fn verify_live_flags_an_unreachable_host() {
        let client = reqwest::Client::new();
        assert!(!verify_live(&client, "http://127.0.0.1:1", Some(3)).await);
    }

    /// No fresh `index.html` this call (a diffing signer only re-asked for
    /// other files) -- reachability is all that can honestly be claimed,
    /// so a 200 with any body at all passes.
    #[tokio::test]
    async fn verify_live_without_an_expected_length_only_checks_reachability() {
        let url = serve_once("HTTP/1.1 200 OK", b"whatever is there already").await;
        let client = reqwest::Client::new();
        assert!(verify_live(&client, &url, None).await);
    }
}

#[cfg(test)]
mod live_check {
    use super::*;
    /// Drives the real client against a stand-in signer that presigns with
    /// this machine's own AWS credentials, then fetches the result back
    /// through the real CloudFront distribution. Skipped when there are no
    /// credentials, so it never fails someone else's `cargo test`.
    #[tokio::test]
    #[ignore]
    async fn publishes_to_the_real_bucket_and_serves_through_cloudfront() {
        let dir = std::env::temp_dir().join("boxcode-live-artifact");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("assets")).unwrap();
        std::fs::write(dir.join("index.html"), "<h1>live</h1><script src=assets/app.js></script>").unwrap();
        std::fs::write(dir.join("assets/app.js"), "console.log('spa')").unwrap();
        std::fs::write(dir.join("data.csv"), "a,b\n1,2\n").unwrap();

        let endpoint = std::env::var("BOXCODE_ARTIFACT_ENDPOINT").unwrap_or_default();
        if endpoint.is_empty() {
            eprintln!("skipping: BOXCODE_ARTIFACT_ENDPOINT unset");
            return;
        }
        let published = publish(&dir, &endpoint).await.expect("publish");
        println!("PUBLISHED_URL={}", published.url);
        println!("files={} bytes={} expires={}h", published.files, published.bytes, published.expires_in_hours);
        // publish() already did this exact check against the real bucket and
        // CDN; asserting it here confirms verify_live's own logic (unit-
        // tested above against a fake server) also holds against the real
        // one, not just a stand-in.
        assert!(published.verified, "publish()'s own live read-back failed against the real infra");

        // Confirms the live-reload script actually reaches a real visitor's
        // browser through the real CDN, not just that inject_live_reload's
        // own unit tests are internally consistent.
        let body = reqwest::get(&published.url).await.expect("fetch published page").text().await.expect("body");
        assert!(body.contains(LIVE_RELOAD_SCRIPT), "published page missing live-reload script: {body}");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
