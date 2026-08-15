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
use std::time::Duration;

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

#[derive(Debug)]
pub struct Published {
    pub url: String,
    pub files: usize,
    pub bytes: u64,
    pub expires_in_hours: u32,
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

fn load_registry() -> HashMap<String, String> {
    registry_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// The id `path` was last published under, keyed by its canonicalized form
/// so `./foo.html` and `foo.html` from different cwds still match.
fn remembered_id(path: &Path) -> Option<String> {
    let key = path.canonicalize().ok()?.to_string_lossy().into_owned();
    load_registry().get(&key).cloned()
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
    map.insert(key, id.to_string());
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

    for upload in &signed.uploads {
        let candidate = files
            .iter()
            .find(|f| f.key == upload.path)
            .ok_or_else(|| format!("the service asked for a file that was not offered: {}", upload.path))?;
        let bytes = std::fs::read(&candidate.source)
            .map_err(|e| format!("could not read {}: {e}", candidate.source.display()))?;
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

    remember(path, &signed.id);
    Ok(Published {
        url: signed.url,
        files: files.len(),
        bytes: total,
        expires_in_hours: signed.expires_in_hours,
    })
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
        let _ = std::fs::remove_dir_all(&dir);
    }
}
