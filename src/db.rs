//! A basic per-project database for boxcode-published sites.
//!
//! Same identity story as `auth.rs`: a project's id to the rest of boxcode
//! *is* the artifact id it already published under
//! (`artifacts::remembered_id`), never a second id invented here. `query`
//! sends one SQL statement to the db control-plane (`infra/db/` in this
//! repo, on the same box as the auth control-plane, not this module's
//! concern) and gets back rows or a change count.
//!
//! The one thing this module owns that `auth.rs` does not: a per-project
//! secret key, generated and kept entirely on the developer's own machine
//! (`~/.boxcode/db.json`), never sent to the model. That is deliberate --
//! see `infra/db/README.md` for the full reasoning, but the short version
//! is that a key the model never holds is a key that can never end up
//! embedded in the published page's client-side JS by accident.
//!
//! `query`'s optional `access_token` closes the other half of a gap: the
//! project key above proves *which project* a request belongs to, not
//! *which of the project's own users* is asking. Without this, the only
//! way to scope a row to a signed-in user is SQL the model writes trusting
//! a client-supplied id -- nothing stops a page's own JS from sending
//! someone else's. Passing the access_token `enable_auth`'s sign-in
//! response handed back lets the control-plane verify it against that
//! project's own GoTrue instance and bind the resulting, verified user id
//! as `:current_user_id`, so `WHERE user_id = :current_user_id` is backed
//! by the token, not by whatever the page claims.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rand::RngCore;

#[derive(Debug)]
pub enum QueryResult {
    Rows { rows: Vec<serde_json::Value>, truncated: bool },
    Write { changes: i64, last_insert_rowid: i64 },
}

#[derive(serde::Deserialize)]
#[serde(untagged)]
enum RawResult {
    Rows { rows: Vec<serde_json::Value>, truncated: bool },
    Write { changes: i64, last_insert_rowid: i64 },
}

/// Where this developer's per-project db keys live -- same file-based-
/// registry shape as `artifacts.rs`'s `artifacts.json` and `session.rs`'s
/// sessions, a plain file that can be deleted to forget everything.
fn registry_path() -> Option<PathBuf> {
    std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    Some(crate::config::Config::config_dir().join("db.json"))
}

fn load_registry() -> HashMap<String, String> {
    registry_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_registry(map: &HashMap<String, String>) {
    let Some(reg_path) = registry_path() else { return };
    let Some(parent) = reg_path.parent() else { return };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    if let Ok(s) = serde_json::to_string_pretty(map) {
        let _ = std::fs::write(reg_path, s);
    }
}

fn generate_key() -> String {
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// This project's db key, generating and persisting a new one on first
/// use. Reused on every later call for the same project -- never
/// regenerated -- since the control-plane adopts whichever key it sees
/// first for a given project id (trust-on-first-use) and rejects any
/// later call whose key does not match it.
fn key_for(project_id: &str) -> String {
    let mut registry = load_registry();
    if let Some(key) = registry.get(project_id) {
        return key.clone();
    }
    let key = generate_key();
    registry.insert(project_id.to_string(), key.clone());
    save_registry(&registry);
    key
}

/// Run one SQL statement against the database for the project published at
/// `path`. `endpoint` is the control-plane's `/query` URL.
///
/// Requires `path` to have already been published, same reasoning as
/// `auth::provision`: the project id this needs only exists once
/// `publish_artifact` has minted it.
///
/// `access_token` is optional and orthogonal to the project key: omit it
/// for queries that don't need to know who's asking, pass it (the value
/// `auth::provision`'s sign-in endpoint returned) to have `:current_user_id`
/// available in `sql`, verified server-side rather than trusted from the
/// caller.
pub async fn query(
    path: &Path,
    endpoint: &str,
    sql: &str,
    params: &[serde_json::Value],
    access_token: Option<&str>,
) -> Result<QueryResult, String> {
    if endpoint.trim().is_empty() {
        return Err(
            "no db endpoint is configured. Set `db_endpoint` under [tools] in \
             ~/.boxcode/config.toml."
                .to_string(),
        );
    }
    let Some(project_id) = crate::artifacts::remembered_id(path) else {
        return Err(
            "this has not been published yet. Call publish_artifact on it first -- the \
             database belongs to a project, not a substitute for one."
                .to_string(),
        );
    };
    let key = key_for(&project_id);

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent(concat!("boxcode/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| format!("could not build an HTTP client: {e}"))?;

    let mut body = serde_json::json!({ "project_id": project_id, "key": key, "sql": sql, "params": params });
    if let Some(token) = access_token {
        body["access_token"] = serde_json::Value::String(token.to_string());
    }
    let body = body.to_string();
    let response = client
        .post(endpoint)
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .map_err(|e| format!("could not reach the db service: {e}"))?;

    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!("the db service refused this ({status}): {}", text.trim()));
    }
    let parsed: RawResult = serde_json::from_str(&text)
        .map_err(|e| format!("the db service returned something unexpected ({e})"))?;

    Ok(match parsed {
        RawResult::Rows { rows, truncated } => QueryResult::Rows { rows, truncated },
        RawResult::Write { changes, last_insert_rowid } => {
            QueryResult::Write { changes, last_insert_rowid }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn an_unconfigured_endpoint_explains_itself() {
        let error = query(Path::new("/tmp/does-not-matter"), "  ", "SELECT 1", &[], None)
            .await
            .expect_err("should refuse");
        assert!(error.contains("[tools]"), "{error}");
    }

    #[tokio::test]
    async fn an_unpublished_path_is_refused_before_any_network_call() {
        let dir = std::env::temp_dir().join(format!("boxcode-db-unpublished-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        let target = dir.join("index.html");
        std::fs::write(&target, "hi").expect("write");

        let error = query(&target, "http://127.0.0.1:1", "SELECT 1", &[], None)
            .await
            .expect_err("should refuse");
        assert!(error.contains("publish_artifact"), "{error}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A minimal HTTP/1.1 server on a real socket, not a fake address like
    /// the tests above -- this one has to inspect what actually went out on
    /// the wire, which an unreachable address can't show.
    async fn serve_once_and_capture_body(response_json: &'static str) -> (String, tokio::task::JoinHandle<String>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            let mut content_length = None;
            loop {
                let n = socket.read(&mut chunk).await.expect("read");
                buf.extend_from_slice(&chunk[..n]);
                if let Some(header_end) = find_subslice(&buf, b"\r\n\r\n") {
                    if content_length.is_none() {
                        let headers = String::from_utf8_lossy(&buf[..header_end]);
                        content_length = headers.lines().find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .and_then(|v| v.trim().parse::<usize>().ok())
                        });
                    }
                    let body_so_far = buf.len() - (header_end + 4);
                    if content_length.map(|cl| body_so_far >= cl).unwrap_or(false) {
                        break;
                    }
                }
                if n == 0 {
                    break;
                }
            }
            let header_end = find_subslice(&buf, b"\r\n\r\n").expect("headers");
            let body = String::from_utf8_lossy(&buf[header_end + 4..]).to_string();

            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                response_json.len(),
                response_json
            );
            socket.write_all(response.as_bytes()).await.expect("write");
            socket.shutdown().await.ok();
            body
        });
        (format!("http://{addr}"), handle)
    }

    fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack.windows(needle.len()).position(|w| w == needle)
    }

    /// Writes straight into `artifacts.json` in the same shape
    /// `artifacts::publish` itself would leave, since `remember` there is
    /// private to that module -- this is the one other place (besides
    /// `enable_auth`) that needs a path to already read back as published.
    fn fake_publish(fake_home: &Path, project_dir: &Path, id: &str) {
        let key = project_dir.canonicalize().expect("canonicalize").to_string_lossy().into_owned();
        let registry_path = fake_home.join(".boxcode").join("artifacts.json");
        std::fs::create_dir_all(registry_path.parent().unwrap()).expect("mkdir");
        let published_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_secs();
        let map = serde_json::json!({ key: { "id": id, "published_at": published_at } });
        std::fs::write(registry_path, serde_json::to_string_pretty(&map).unwrap()).expect("write registry");
    }

    /// Locks `HOME_LOCK` directly rather than going through
    /// `with_isolated_home`, same reasoning as `tools.rs`'s
    /// `web_search_falls_back_to_the_embedded_python...`: this test needs
    /// `query`'s own `.await`, and `with_isolated_home`'s closure runs
    /// synchronously already inside `#[tokio::test]`'s runtime.
    #[tokio::test]
    async fn an_access_token_is_forwarded_in_the_request_body() {
        let _guard = crate::config::test_support::HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let fake_home = tempfile::tempdir().expect("temp home");
        let prev_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", fake_home.path());

        let project = tempfile::tempdir().expect("project dir");
        std::fs::write(project.path().join("index.html"), "hi").expect("write");
        fake_publish(fake_home.path(), project.path(), "proj-with-token");
        let (endpoint, handle) = serve_once_and_capture_body(r#"{"rows":[],"truncated":false}"#).await;

        let result = query(project.path(), &endpoint, "SELECT 1", &[], Some("the-access-token")).await;

        match prev_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }

        assert!(result.is_ok(), "{result:?}");
        let body = handle.await.expect("server task");
        assert!(body.contains("\"access_token\":\"the-access-token\""), "{body}");
    }

    #[tokio::test]
    async fn no_access_token_means_no_access_token_field_at_all() {
        let _guard = crate::config::test_support::HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let fake_home = tempfile::tempdir().expect("temp home");
        let prev_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", fake_home.path());

        let project = tempfile::tempdir().expect("project dir");
        std::fs::write(project.path().join("index.html"), "hi").expect("write");
        fake_publish(fake_home.path(), project.path(), "proj-without-token");
        let (endpoint, handle) = serve_once_and_capture_body(r#"{"rows":[],"truncated":false}"#).await;

        let result = query(project.path(), &endpoint, "SELECT 1", &[], None).await;

        match prev_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }

        assert!(result.is_ok(), "{result:?}");
        let body = handle.await.expect("server task");
        assert!(!body.contains("access_token"), "{body}");
    }

    #[test]
    fn a_projects_key_is_generated_once_and_then_reused() {
        crate::config::test_support::with_isolated_home(|| {
            let first = key_for("proj1234");
            let second = key_for("proj1234");
            assert_eq!(first, second, "the same project must keep the same key");
            assert_eq!(first.len(), 32, "expected 16 bytes hex-encoded");

            let other = key_for("otherproj");
            assert_ne!(first, other, "different projects must get different keys");
        });
    }
}
