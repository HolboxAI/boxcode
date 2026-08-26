//! Sending a backend to be hosted.
//!
//! Mirrors [`crate::artifacts`]: collect a tree, check it against limits the
//! server also enforces, send it, and confirm what happened rather than
//! assuming. The differences are the ones the substrate forces.
//!
//! **The source goes in one request.** `artifacts` asks a signer for presigned
//! URLs and uploads each file straight to S3, which is right for a static site
//! served from S3. A backend is extracted onto one box, so a second endpoint
//! would only add a second thing to authenticate -- and an unauthenticated
//! upload endpoint beside an authenticated deploy endpoint is a hole with extra
//! steps. One gated request carries everything.
//!
//! **Deploying is asynchronous.** Installing dependencies takes minutes, and
//! CloudFront gives an origin sixty seconds to answer. So the deploy is accepted
//! and then polled. Reporting "deployed" the moment the request returns would be
//! reporting that the work was *queued*.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rand::RngCore;
use serde::{Deserialize, Serialize};

/// Never sent. `node_modules` is reinstalled inside the build microVM against
/// the runtime the guest actually has -- sending a tree built on macOS would
/// upload hundreds of megabytes of the wrong architecture's binaries.
const SKIP_DIRS: &[&str] = &[
    "node_modules",
    ".git",
    ".venv",
    "venv",
    "__pycache__",
    ".pytest_cache",
    "dist",
    "build",
    ".next",
    ".nuxt",
    "target",
    ".terraform",
    "coverage",
];

/// Never sent either: secrets a project keeps locally, which have no business
/// on a shared host and would otherwise be baked into a disk image.
const SKIP_FILES: &[&str] = &[".env", ".env.local", ".env.production", ".DS_Store"];

const MAX_FILES: usize = 400;
const MAX_TOTAL_BYTES: u64 = 8 * 1024 * 1024;
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;

/// How long to wait for a deploy after it is accepted. The build VM is capped at
/// five minutes and the rest is seconds, so this is generous rather than tight.
const DEPLOY_TIMEOUT: Duration = Duration::from_secs(600);

/// How long to keep asking the URL after the deploy says `running`.
///
/// These are two different events and the gap between them is real. `running`
/// means the microVM booted; it does not mean the process inside has bound its
/// port, and until it does nginx answers 502. A single probe fired the moment
/// the state flips loses that race and reports `verified: false` for a deploy
/// that is seconds from being fine -- which is exactly what it did on the first
/// live run, on a site that was serving correctly by the time anyone looked.
const PROBE_WINDOW: Duration = Duration::from_secs(90);
const POLL_EVERY: Duration = Duration::from_secs(3);

#[derive(Debug, Clone)]
pub struct Deployed {
    pub id: String,
    pub url: String,
    pub expires_in_hours: u32,
    /// Whether a request to the live URL came back healthy. Like
    /// `Published::verified`, this is a fact this code checked, not a claim
    /// relayed from the server.
    pub verified: bool,
}

#[derive(Debug)]
struct Candidate {
    key: String,
    source: PathBuf,
    size: u64,
}

#[derive(Serialize)]
struct FileEntry {
    path: String,
    content: String,
}

#[derive(Deserialize)]
struct AcceptResponse {
    id: String,
    url: String,
    #[serde(default)]
    expires_in_hours: u32,
}

#[derive(Deserialize)]
struct StatusResponse {
    #[serde(default)]
    state: String,
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Deserialize)]
struct ErrorResponse {
    #[serde(default)]
    error: String,
}

// ---------------------------------------------------------------------------
// base64
// ---------------------------------------------------------------------------

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Written here rather than pulled in as a dependency.
///
/// This is twenty lines of table lookup with an exhaustive test below, against a
/// crate that would be a new name in the supply chain of a binary people install
/// and run. That trade only goes this way for something small and easy to get
/// demonstrably right -- it is not an argument for hand-rolling anything with
/// more than one correct answer.
fn base64_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(B64[(n >> 18 & 63) as usize] as char);
        out.push(B64[(n >> 12 & 63) as usize] as char);
        // Padding is not decoration: without it a decoder cannot tell three
        // bytes from one, because both produce the same leading characters.
        out.push(if chunk.len() > 1 { B64[(n >> 6 & 63) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { B64[(n & 63) as usize] as char } else { '=' });
    }
    out
}

// ---------------------------------------------------------------------------
// The deploy token
// ---------------------------------------------------------------------------

fn registry_path() -> Option<PathBuf> {
    std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    Some(crate::config::Config::config_dir().join("deploy.json"))
}

fn load_registry() -> HashMap<String, String> {
    let Some(path) = registry_path() else { return HashMap::new() };
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_registry(map: &HashMap<String, String>) {
    let Some(path) = registry_path() else { return };
    let Some(parent) = path.parent() else { return };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    if let Ok(s) = serde_json::to_string_pretty(map) {
        let _ = std::fs::write(path, s);
    }
}

fn generate_token() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Where this machine's own token is filed, alongside the per-project entries
/// that predate it. A project id cannot collide with it: ids match
/// `^[a-z2-9]{4,16}$`, and this contains characters that pattern excludes.
const MACHINE_TOKEN_KEY: &str = "$machine";

/// The token this machine deploys with.
///
/// One token for the machine, not one per project, and the difference is a
/// control rather than a detail. The server's A2 refuses a third live project
/// from a token that already holds two -- it is what stops one person quietly
/// occupying every slot on a shared platform. An earlier version of this minted
/// a fresh token per project id, which meant the count the server did was
/// always of a token holding exactly one project. The check ran, passed, and
/// could not have done anything else. A4's three-new-projects-a-day was left
/// doing all the work alone, which is not what it was sized for.
///
/// Trust on first use either way: the first token to claim an id owns it, and
/// regenerating would lock this machine out of a project it deployed, since the
/// server has no other way to recognise the owner. Kept out of the model's
/// reach for the same reason the database key is -- it must never end up in a
/// file the model can read back and put in a page.
pub fn token_for(project_id: &str) -> String {
    let mut registry = load_registry();

    // A project deployed before this change owns its id under a token of its
    // own, and the server will not accept any other. Honoured rather than
    // migrated: the alternative is a 403 on the next deploy of a project that
    // was working, to tidy up a handful of entries that expire in 48 hours
    // anyway.
    if let Some(token) = registry.get(project_id) {
        return token.clone();
    }

    let token = match registry.get(MACHINE_TOKEN_KEY) {
        Some(existing) => existing.clone(),
        None => {
            let fresh = generate_token();
            registry.insert(MACHINE_TOKEN_KEY.to_string(), fresh.clone());
            fresh
        }
    };

    // The id is recorded too, pointing at the same token.
    //
    // Not redundant, and leaving it out was a bug caught by the first test
    // written against it: with one token for the machine there is otherwise
    // nothing on disk naming the projects this machine deployed. Returning the
    // shared token without noting the id made the whole registry a single
    // entry, so `/hosted` had nothing to list and the machine had quietly
    // stopped keeping track of its own work.
    registry.insert(project_id.to_string(), token.clone());
    save_registry(&registry);
    token
}

// ---------------------------------------------------------------------------
// What this machine has deployed
// ---------------------------------------------------------------------------

/// How many projects one machine may have live at once.
///
/// A mirror of the server's `MAX_APPS_PER_TOKEN`, and only a mirror: the server
/// enforces it and this copy exists so `/hosted` can say where you stand
/// before you hit the refusal rather than after. If the two ever disagree the
/// server is right, and the worst this can do is quote a number that is out of
/// date -- which is why nothing here refuses anything on its own.
pub const MAX_LIVE_PER_MACHINE: usize = 2;

/// Where a project is served, given the configured deploy endpoint.
///
/// Derived from the endpoint rather than assembled from a hard-coded host, so
/// pointing `backend_endpoint` at a different install moves the links with it.
/// The endpoint is `<origin>/api/deploy` and a project is `<origin>/api/<id>/`,
/// so the shared part is everything before the last segment.
pub fn project_url(endpoint: &str, id: &str) -> String {
    let base = endpoint.trim_end_matches('/');
    let root = base.rsplit_once('/').map(|(head, _)| head).unwrap_or(base);
    format!("{root}/{id}/")
}

/// One hosted project this machine owns, as `/hosted` shows it./// One hosted project this machine owns, as `/hosted` shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mine {
    pub id: String,
    /// Where it was published from, when the artifact registry still
    /// remembers. `None` for a project whose publish has aged out of that
    /// registry while the deploy token for it is still on disk -- the id is
    /// still ours and still worth listing, we just cannot say where it came
    /// from any more.
    pub path: Option<String>,
    /// What the server says now: "running", "building", "failed", or `None`
    /// when it could not be reached or does not know the id.
    pub state: Option<String>,
}

/// The ids this machine holds a deploy token for, newest publish first.
///
/// Local only, and deliberately so -- there is no endpoint that lists a
/// caller's projects, and adding one would mean the server keeping a map from
/// token to projects that it currently has no reason to hold.
///
/// The token registry is the source of truth for ownership because it is the
/// thing the server actually checks. The artifact registry is joined onto it
/// only to recover a human-readable path.
pub fn mine() -> Vec<Mine> {
    let owned = load_registry();

    // Read once. `all_local` re-reads and re-sorts a file on every call, and
    // an earlier version of this called it from inside the sort comparator.
    let published = crate::artifacts::all_local();
    let path_of: std::collections::HashMap<&str, &str> =
        published.iter().map(|(path, id)| (id.as_str(), path.as_str())).collect();
    let rank_of: std::collections::HashMap<&str, usize> =
        published.iter().enumerate().map(|(i, (_, id))| (id.as_str(), i)).collect();

    let mut out: Vec<Mine> = owned
        .keys()
        .filter(|id| id.as_str() != MACHINE_TOKEN_KEY)
        .map(|id| Mine {
            id: id.clone(),
            path: path_of.get(id.as_str()).map(|p| (*p).to_string()),
            state: None,
        })
        .collect();

    // Newest publish first, then the ones the artifact registry has forgotten,
    // and ties broken by id so two runs of the same command never disagree --
    // the keys come from a HashMap, whose order is not stable between runs.
    out.sort_by(|a, b| {
        let ra = rank_of.get(a.id.as_str()).copied().unwrap_or(usize::MAX);
        let rb = rank_of.get(b.id.as_str()).copied().unwrap_or(usize::MAX);
        ra.cmp(&rb).then_with(|| a.id.cmp(&b.id))
    });
    out
}

/// Ask the control plane what each of these is doing now.
///
/// Concurrent, because this runs while someone is looking at a blank list, and
/// serialising four requests behind each other's timeouts is the difference
/// between a command that feels instant and one that feels broken.
///
/// A project the server does not know about comes back `None` rather than as an
/// error: the ordinary reason is that it expired or was taken down, which is
/// information, not a failure.
pub async fn statuses(endpoint: &str, mut projects: Vec<Mine>) -> Vec<Mine> {
    let base = endpoint.trim_end_matches('/').to_string();
    let client = match reqwest::Client::builder().timeout(Duration::from_secs(10)).build() {
        Ok(c) => c,
        Err(_) => return projects,
    };
    let lookups = projects.iter().map(|m| {
        let url = format!("{base}/status/{}", m.id);
        let client = client.clone();
        async move {
            let text = client.get(&url).send().await.ok()?.text().await.ok()?;
            serde_json::from_str::<StatusResponse>(&text).ok().map(|s| s.state)
        }
    });
    let states = futures::future::join_all(lookups).await;
    for (m, state) in projects.iter_mut().zip(states) {
        m.state = state;
    }
    projects
}

// ---------------------------------------------------------------------------
// Collecting
// ---------------------------------------------------------------------------

fn collect(root: &Path) -> Result<Vec<Candidate>, String> {
    if !root.is_dir() {
        return Err(format!(
            "{} is not a directory. A backend is deployed as a whole project, not a single file.",
            root.display()
        ));
    }
    let mut out = Vec::new();
    walk(root, root, &mut out, 0)?;
    if out.is_empty() {
        return Err(format!("{} has no files to deploy.", root.display()));
    }
    out.sort_by(|a, b| a.key.cmp(&b.key));
    Ok(out)
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<Candidate>, depth: usize) -> Result<(), String> {
    // A symlink loop would otherwise walk forever. Depth is the cheap guard;
    // symlinked directories are skipped outright below.
    if depth > 20 {
        return Ok(());
    }
    let entries = std::fs::read_dir(dir).map_err(|e| format!("could not read {}: {e}", dir.display()))?;
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        // Not followed. A symlink out of the project would quietly pull in
        // whatever it points at -- an ssh key, a home directory -- and package
        // it into an image on a shared host.
        if meta.file_type().is_symlink() {
            continue;
        }
        if meta.is_dir() {
            if SKIP_DIRS.contains(&name.as_str()) {
                continue;
            }
            walk(root, &path, out, depth + 1)?;
            continue;
        }
        if SKIP_FILES.contains(&name.as_str()) || name.starts_with(".env.") {
            continue;
        }
        let key = path
            .strip_prefix(root)
            .map_err(|_| "a file resolved outside the project".to_string())?
            .components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        out.push(Candidate { key, source: path, size: meta.len() });
    }
    Ok(())
}

fn check_limits(files: &[Candidate]) -> Result<u64, String> {
    if files.len() > MAX_FILES {
        return Err(format!(
            "this project has {} files and the limit is {MAX_FILES}. Dependencies are installed on \
             the server, so build output and vendored packages do not need sending -- check for a \
             directory that should be ignored.",
            files.len()
        ));
    }
    if let Some(big) = files.iter().find(|f| f.size > MAX_FILE_BYTES) {
        return Err(format!(
            "{} is {:.1} MB and the per-file limit is {} MB.",
            big.key,
            big.size as f64 / 1_048_576.0,
            MAX_FILE_BYTES / 1_048_576
        ));
    }
    let total: u64 = files.iter().map(|f| f.size).sum();
    if total > MAX_TOTAL_BYTES {
        return Err(format!(
            "this project is {:.1} MB of source and the limit is {} MB.",
            total as f64 / 1_048_576.0,
            MAX_TOTAL_BYTES / 1_048_576
        ));
    }
    Ok(total)
}

// ---------------------------------------------------------------------------
// Deploying
// ---------------------------------------------------------------------------

/// Send a backend and wait for it to come up.
pub async fn deploy(
    path: &Path,
    id: &str,
    profile: &crate::deploy::backend::BackendProfile,
    endpoint: &str,
) -> Result<Deployed, String> {
    if endpoint.trim().is_empty() {
        return Err(
            "no backend endpoint is configured. Set `backend_endpoint` under [tools] in \
             ~/.boxcode/config.toml."
                .to_string(),
        );
    }
    let entrypoint = profile
        .start_command()
        .ok_or_else(|| {
            format!(
                "could not work out how to start this {}. Name the file that starts the server \
                 -- for Node that is usually the `main` field in package.json or a top-level \
                 index.js/server.js.",
                profile.framework.label()
            )
        })?;

    let files = collect(path)?;
    let total = check_limits(&files)?;

    let payload: Vec<FileEntry> = files
        .iter()
        .map(|f| {
            let bytes = std::fs::read(&f.source)
                .map_err(|e| format!("could not read {}: {e}", f.source.display()))?;
            Ok(FileEntry { path: f.key.clone(), content: base64_encode(&bytes) })
        })
        .collect::<Result<_, String>>()?;

    let body = serde_json::json!({
        "id": id,
        "token": token_for(id),
        "runtime": profile.runtime.wire_name(),
        "entrypoint": entrypoint,
        "files": payload,
    })
    .to_string();

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .user_agent(concat!("boxcode/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| format!("could not build an HTTP client: {e}"))?;

    let response = client
        .post(endpoint)
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .map_err(|e| format!("could not reach the hosting service: {e}"))?;

    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        // The server's refusals are written for the person deploying -- "this
        // token already has 2 projects running (aaaa, bbbb)" -- so they are
        // relayed rather than replaced with something generic.
        let detail = serde_json::from_str::<ErrorResponse>(&text)
            .map(|e| e.error)
            .unwrap_or_else(|_| text.trim().to_string());
        return Err(format!("the hosting service refused this ({status}): {detail}"));
    }

    let accepted: AcceptResponse = serde_json::from_str(&text)
        .map_err(|e| format!("the hosting service returned something unexpected ({e})"))?;

    let _ = total;
    let state = await_ready(&client, endpoint, &accepted.id).await?;
    let verified = state == "running" && probe(&client, &accepted.url, &accepted.id).await;

    Ok(Deployed {
        id: accepted.id,
        url: accepted.url,
        expires_in_hours: if accepted.expires_in_hours == 0 { 48 } else { accepted.expires_in_hours },
        verified,
    })
}

/// Poll until the deploy finishes, fails, or runs out of patience.
async fn await_ready(client: &reqwest::Client, endpoint: &str, id: &str) -> Result<String, String> {
    let status_url = format!("{}/status/{id}", endpoint.trim_end_matches('/'));
    let deadline = std::time::Instant::now() + DEPLOY_TIMEOUT;
    let mut last = String::from("queued");

    while std::time::Instant::now() < deadline {
        tokio::time::sleep(POLL_EVERY).await;
        let Ok(response) = client.get(&status_url).send().await else {
            // A single failed poll is not a failed deploy -- the build VM is
            // taking a whole CPU and nginx is on the same box.
            continue;
        };
        let Ok(text) = response.text().await else { continue };
        let Ok(status) = serde_json::from_str::<StatusResponse>(&text) else { continue };

        last = status.state.clone();
        match status.state.as_str() {
            "running" => return Ok(last),
            "failed" => {
                return Err(format!(
                    "the deploy failed: {}",
                    status.reason.unwrap_or_else(|| "no reason given".into())
                ))
            }
            _ => {}
        }
    }
    Err(format!(
        "the deploy did not finish within {} minutes (last state: {last}). It may still be \
         building -- check the URL shortly rather than deploying again.",
        DEPLOY_TIMEOUT.as_secs() / 60
    ))
}

/// One real request to the live URL.
///
/// A backend is entitled to answer 404 at its root, or 401, or a redirect, so
/// the status alone cannot say whether it worked. What it also cannot say is
/// whether the request reached the backend at all: an unrouted path returns the
/// edge's own 404, which is equally "less than 500".
///
/// That is not hypothetical. The first live deploy reported itself verified
/// while nginx had never reloaded, so every request fell through to
/// "no app deployed at this path" -- a 404 from the front door, counted as
/// success. The per-project route carries `X-Boxcode-Project` for exactly this
/// reason: it is present only when the route is loaded, whatever the app then
/// decides to answer.
async fn probe(client: &reqwest::Client, url: &str, id: &str) -> bool {
    let deadline = std::time::Instant::now() + PROBE_WINDOW;
    loop {
        if probe_once(client, url, id).await {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(POLL_EVERY).await;
    }
}

/// One attempt.
///
/// The header is the whole point. Without it a 404 from the edge's catch-all --
/// which is what an unrouted project gets -- reads as a live site, and the
/// deploy reports a URL that serves nothing. Checking the status alone is not
/// enough: the edge answers, it just does not answer for this project.
async fn probe_once(client: &reqwest::Client, url: &str, id: &str) -> bool {
    let Ok(r) = client.get(url).timeout(Duration::from_secs(20)).send().await else {
        return false;
    };
    let routed = r
        .headers()
        .get("x-boxcode-project")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v == id);
    routed && r.status().as_u16() < 500
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_rfc_test_vectors() {
        // RFC 4648 section 10, which exists precisely so a hand-written encoder
        // can be checked against something authoritative.
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn base64_handles_bytes_that_are_not_text() {
        // A backend source tree can contain a small binary, and every byte
        // value has to survive the trip.
        let all: Vec<u8> = (0u8..=255).collect();
        let encoded = base64_encode(&all);
        assert_eq!(encoded.len(), 344, "256 bytes is 344 base64 characters");
        assert!(encoded.chars().all(|c| c.is_ascii_alphanumeric() || "+/=".contains(c)));
        assert_eq!(base64_encode(&[0, 0, 0]), "AAAA");
        assert_eq!(base64_encode(&[255, 255, 255]), "////");
    }

    #[test]
    fn base64_pads_so_length_is_never_ambiguous() {
        // Without padding a decoder cannot tell three bytes from one.
        for len in 0..32usize {
            let encoded = base64_encode(&vec![b'x'; len]);
            assert_eq!(encoded.len() % 4, 0, "len {len} produced {}", encoded.len());
        }
    }

    fn tree(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        for (path, body) in files {
            let full = dir.path().join(path);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(full, body).unwrap();
        }
        dir
    }

    #[test]
    fn dependencies_and_build_output_are_never_sent() {
        // node_modules is reinstalled in the build microVM against the runtime
        // the guest actually has; sending a macOS-built tree would upload
        // hundreds of megabytes of the wrong architecture.
        let dir = tree(&[
            ("server.js", "run"),
            ("node_modules/left-pad/index.js", "x"),
            ("dist/bundle.js", "x"),
            (".git/config", "x"),
            ("__pycache__/a.pyc", "x"),
            ("src/app.js", "y"),
        ]);
        let files = collect(dir.path()).unwrap();
        let keys: Vec<_> = files.iter().map(|f| f.key.as_str()).collect();
        assert_eq!(keys, vec!["server.js", "src/app.js"]);
    }

    #[test]
    fn local_secrets_are_never_sent() {
        // They have no business on a shared host, and would be baked into a
        // disk image that outlives the deploy.
        let dir = tree(&[
            ("server.js", "run"),
            (".env", "SECRET=1"),
            (".env.local", "SECRET=2"),
            (".env.production", "SECRET=3"),
        ]);
        let files = collect(dir.path()).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].key, "server.js");
    }

    #[test]
    fn a_file_is_not_a_project() {
        let dir = tree(&[("server.js", "run")]);
        let err = collect(&dir.path().join("server.js")).unwrap_err();
        assert!(err.contains("not a directory"), "{err}");
    }

    #[test]
    fn an_empty_project_is_refused_rather_than_deployed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let err = collect(dir.path()).unwrap_err();
        assert!(err.contains("no files"), "{err}");
    }

    #[test]
    fn limits_name_the_file_and_suggest_the_cause() {
        let files = vec![Candidate {
            key: "big.bin".into(),
            source: PathBuf::from("big.bin"),
            size: MAX_FILE_BYTES + 1,
        }];
        let err = check_limits(&files).unwrap_err();
        assert!(err.contains("big.bin"), "{err}");

        let many: Vec<Candidate> = (0..MAX_FILES + 1)
            .map(|i| Candidate { key: format!("f{i}"), source: PathBuf::from("f"), size: 1 })
            .collect();
        let err = check_limits(&many).unwrap_err();
        // A tree over the file limit is nearly always a directory that should
        // have been skipped, so the message says so rather than just refusing.
        assert!(err.contains("do not need sending"), "{err}");
    }

    #[test]
    fn paths_are_relative_and_use_forward_slashes() {
        // They become paths inside a Linux guest, whatever platform packaged
        // them.
        let dir = tree(&[("src/routes/index.js", "x")]);
        let files = collect(dir.path()).unwrap();
        assert_eq!(files[0].key, "src/routes/index.js");
        assert!(!files[0].key.starts_with('/'));
    }

    #[test]
    fn files_are_sent_in_a_stable_order() {
        // So an unchanged project produces an identical request, which makes a
        // redeploy diffable in a log.
        let dir = tree(&[("b.js", "x"), ("a.js", "x"), ("c/d.js", "x")]);
        let files = collect(dir.path()).unwrap();
        let keys: Vec<_> = files.iter().map(|f| f.key.as_str()).collect();
        assert_eq!(keys, vec!["a.js", "b.js", "c/d.js"]);
    }

    /// The real thing, end to end, against the live service.
    ///
    /// Ignored by default because it needs the network and it costs a slot on a
    /// box with ten. Run it deliberately:
    ///
    ///   cargo test --  --ignored --nocapture live_deploy
    ///
    /// Follows the two network-gated tests in artifacts.rs, for the same reason
    /// they exist: everything else here proves the client builds the right
    /// request, and only this proves the request is one the server accepts.
    #[tokio::test]
    #[ignore]
    async fn live_deploy_of_a_real_backend() {
        let endpoint = std::env::var("BOXCODE_BACKEND_ENDPOINT")
            .unwrap_or_else(|_| "https://boxcode.sh/api/deploy".to_string());
        let id = std::env::var("BOXCODE_TEST_ID").unwrap_or_else(|_| "e2etest99".to_string());

        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"name":"e2e","version":"1.0.0","main":"server.js","dependencies":{"express":"^4.19.2"}}"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("server.js"),
            r#"const express = require("express");
const app = express();
app.get("/", (_q, r) => r.json({ ok: true, from: "a microVM" }));
app.get("/env", (_q, r) => r.json({ hasDatabaseUrl: Boolean(process.env.DATABASE_URL) }));
app.listen(process.env.PORT || 3000, "0.0.0.0");
"#,
        )
        .unwrap();

        let profile = crate::deploy::backend::detect_backend(dir.path()).expect("detect");
        eprintln!("detected: {} on {}", profile.framework.label(), profile.runtime.label());
        eprintln!("start:    {:?}", profile.start_command());

        match deploy(dir.path(), &id, &profile, &endpoint).await {
            Ok(d) => {
                eprintln!("url:      {}", d.url);
                eprintln!("verified: {}", d.verified);
                eprintln!("expires:  {}h", d.expires_in_hours);
                assert!(d.verified, "deployed but the URL did not answer");
            }
            Err(e) => panic!("deploy failed: {e}"),
        }
    }

    #[test]
    fn a_project_url_is_derived_from_the_endpoint_not_hard_coded() {
        assert_eq!(
            project_url("https://boxcode.sh/api/deploy", "jqtqx9zf"),
            "https://boxcode.sh/api/jqtqx9zf/"
        );
        // A different install must move the links with it, or /deploys prints
        // URLs pointing at somebody else's platform.
        assert_eq!(
            project_url("https://hosting.example.com/api/deploy", "abcd2345"),
            "https://hosting.example.com/api/abcd2345/"
        );
        // A trailing slash on the configured endpoint must not produce a
        // doubled one in the link.
        assert_eq!(
            project_url("https://boxcode.sh/api/deploy/", "abcd2345"),
            "https://boxcode.sh/api/abcd2345/"
        );
    }

    #[test]
    fn mine_lists_this_machines_projects_and_never_the_machine_key() {
        crate::config::test_support::with_isolated_home(|| {
            // Three projects, so the machine key has company and cannot be
            // mistaken for the only odd entry.
            for id in ["aaaa2345", "bbbb2345", "cccc2345"] {
                let _ = token_for(id);
            }
            let listed = mine();
            assert_eq!(listed.len(), 3, "{listed:?}");
            assert!(
                !listed.iter().any(|m| m.id == MACHINE_TOKEN_KEY),
                "the machine's own token is not a project: {listed:?}"
            );
            assert!(listed.iter().all(|m| m.state.is_none()), "state is asked for separately");
        });
    }

    #[test]
    fn mine_is_ordered_the_same_way_twice() {
        // The ids come from a HashMap, whose iteration order differs between
        // runs. A list that reshuffles every time it is shown is one nobody
        // can read down twice.
        crate::config::test_support::with_isolated_home(|| {
            for id in ["zzzz2345", "aaaa2345", "mmmm2345", "bbbb2345"] {
                let _ = token_for(id);
            }
            let first: Vec<String> = mine().into_iter().map(|m| m.id).collect();
            let again: Vec<String> = mine().into_iter().map(|m| m.id).collect();
            assert_eq!(first, again);
            // Nothing has been published here, so every entry ranks equally and
            // the id is the tie-break -- which must be a real ordering.
            let mut sorted = first.clone();
            sorted.sort();
            assert_eq!(first, sorted, "ties break by id");
        });
    }

    #[test]
    fn a_token_is_minted_once_and_then_reused() {
        // with_isolated_home rather than setting HOME here: these tests run in
        // parallel and the variable is process-global, so a hand-rolled
        // save/restore races with every other test that touches it. The first
        // version of this did exactly that and the token appeared to rotate.
        crate::config::test_support::with_isolated_home(|| {
            let first = token_for("k9depef6");
            assert_eq!(first.len(), 64, "32 bytes hex-encoded");
            assert!(first.chars().all(|c| c.is_ascii_hexdigit()));
            assert_eq!(token_for("k9depef6"), first, "the same project must keep its token");
        });
    }

    #[test]
    fn one_token_covers_every_project_on_this_machine() {
        // This assertion used to run the other way -- "different projects must
        // differ" -- which is what a per-project token does and reads as
        // perfectly sensible on its own. It also made the server's A2 check
        // dead: A2 refuses a third live project from a token already holding
        // two, and a token that holds exactly one project by construction can
        // never reach two. The test passed, the control was inert, and nothing
        // anywhere said so.
        crate::config::test_support::with_isolated_home(|| {
            let a = token_for("k9depef6");
            let b = token_for("aaaabbbb");
            let c = token_for("zzzz2345");
            assert_eq!(a, b, "one machine deploys with one token");
            assert_eq!(b, c, "and that does not drift as projects are added");
        });
    }

    #[test]
    fn a_project_that_already_owns_a_token_keeps_it() {
        // Projects deployed before the change own their id under a token of
        // their own, and the server accepts no other. Migrating them would mean
        // a 403 on the next deploy of something that was working.
        crate::config::test_support::with_isolated_home(|| {
            let legacy = "f".repeat(64);
            let mut registry = load_registry();
            registry.insert("oldproj12".to_string(), legacy.clone());
            save_registry(&registry);

            assert_eq!(token_for("oldproj12"), legacy, "the old token still owns the old id");
            assert_ne!(token_for("newproj34"), legacy, "a new project uses the machine token");
            assert_eq!(
                token_for("newproj34"),
                token_for("othernew5"),
                "and every new project shares it"
            );
        });
    }

    #[test]
    fn the_machine_key_cannot_collide_with_a_project_id() {
        // Ids are ^[a-z2-9]{4,16}$ server-side. If the key ever became a legal
        // id, a project could claim the machine's own token.
        let id_re = regex::Regex::new("^[a-z2-9]{4,16}$").unwrap();
        assert!(!id_re.is_match(MACHINE_TOKEN_KEY), "{MACHINE_TOKEN_KEY} must not be a valid id");
    }
}
