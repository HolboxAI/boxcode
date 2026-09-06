//! Reported runtime errors for a published boxcode artifact.
//!
//! Same identity story as `requests.rs`/`auth.rs`/`db.rs`: a project's id to
//! the rest of boxcode *is* the artifact id it already published under
//! (`artifacts::remembered_id`), never a second id invented here.
//!
//! The other half of this feature is not in this file at all: a small,
//! dependency-free JS beacon (`infra/errors/control-plane`'s
//! `GET /errors-beacon.js`) that a developer adds to their own published
//! page's HTML with `edit_file` -- one `<script>` tag, no code in this repo
//! generates or owns it -- so a `window.onerror`/`unhandledrejection` in a
//! real visitor's browser gets reported without anyone running boxcode. The
//! beacon only ever *submits*; it holds no key and cannot resolve anything,
//! so a reported error sitting in the mailbox is not itself an edit.
//! Someone still has to read it, fix it with the ordinary agent loop, and
//! republish -- this module is only how boxcode fetches what is waiting and
//! marks it handled once it has been.
//!
//! Deliberately not a separate pair of tools. `list_change_requests`/
//! `resolve_change_request` already spend two of the fifteen tool slots this
//! codebase caps itself at (see the reasoning at `tools.rs` near
//! `LIST_CHANGE_REQUESTS`) -- a runtime error and a change request are both
//! "something a human or the deployed app itself flagged that needs the
//! developer's attention," so they share the same two tools rather than
//! adding two more. `ReportedError`'s ids are prefixed `err_` by the
//! control-plane specifically so `resolve_change_request` can route a
//! resolve call to the right backend by looking at the id, without a
//! separate `kind` argument the model would have to remember to pass.

use std::path::Path;
use std::time::Duration;

pub const ID_PREFIX: &str = "err_";

#[derive(Debug, Clone, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct ReportedError {
    pub id: String,
    pub message: String,
    pub file: String,
    pub line: u64,
    pub col: u64,
    /// How many times this exact `(project_id, message, file, line, col)`
    /// has been reported. The control-plane dedupes on that tuple rather
    /// than storing one row per occurrence -- a loop that throws on every
    /// render should not turn into thousands of identical mailbox entries.
    pub count: u64,
    pub first_seen_at: String,
    pub last_seen_at: String,
}

fn http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent(concat!("boxcode/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| format!("could not build an HTTP client: {e}"))
}

fn require_endpoint(endpoint: &str) -> Result<(), String> {
    if endpoint.trim().is_empty() {
        return Err(
            "no errors endpoint is configured. Set `errors_endpoint` under [tools] in \
             ~/.boxcode/config.toml."
                .to_string(),
        );
    }
    Ok(())
}

fn require_published(path: &Path) -> Result<String, String> {
    crate::artifacts::remembered_id(path).ok_or_else(|| {
        "this has not been published yet. Call publish_artifact on it first -- the \
         reported-error mailbox belongs to a project, not a substitute for one."
            .to_string()
    })
}

/// The pending reported errors waiting for the project published at `path`.
/// `endpoint` is the control-plane's `/errors` URL.
pub async fn list_pending(path: &Path, endpoint: &str) -> Result<Vec<ReportedError>, String> {
    require_endpoint(endpoint)?;
    let project_id = require_published(path)?;

    let client = http_client()?;
    let response = client
        .get(endpoint)
        .query(&[("project_id", project_id.as_str())])
        .send()
        .await
        .map_err(|e| format!("could not reach the errors service: {e}"))?;

    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!("the errors service refused this ({status}): {}", text.trim()));
    }
    serde_json::from_str(&text)
        .map_err(|e| format!("the errors service returned something unexpected ({e})"))
}

/// Mark reported error `id` for the project published at `path` as handled,
/// so it stops showing up as pending. `endpoint` is the control-plane's
/// `/errors` URL (the same one `list_pending` uses); the resolve call goes
/// to `{endpoint}/{id}/resolve`.
pub async fn resolve(path: &Path, endpoint: &str, id: &str) -> Result<(), String> {
    require_endpoint(endpoint)?;
    let project_id = require_published(path)?;

    let client = http_client()?;
    let resolve_url = format!("{}/{id}/resolve", endpoint.trim_end_matches('/'));
    let body = serde_json::json!({ "project_id": project_id }).to_string();
    let response = client
        .post(resolve_url)
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .map_err(|e| format!("could not reach the errors service: {e}"))?;

    let status = response.status();
    if !status.is_success() {
        let text = response.text().await.unwrap_or_default();
        return Err(format!("the errors service refused this ({status}): {}", text.trim()));
    }
    Ok(())
}

/// Renders one `ReportedError` the way `execute_list_change_requests` folds
/// it into the same text-based list a `ChangeRequest` renders as, so the
/// model can tell the two kinds apart by reading the text without a new
/// field on the wire.
pub fn describe(error: &ReportedError) -> String {
    let times = if error.count == 1 {
        String::new()
    } else {
        format!(" (×{})", error.count)
    };
    format!(
        "[runtime error{times}] {} — {}:{}:{}",
        error.message, error.file, error.line, error.col
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn an_unconfigured_endpoint_explains_itself() {
        let error = list_pending(Path::new("/tmp/does-not-matter"), "  ")
            .await
            .expect_err("should refuse");
        assert!(error.contains("[tools]"), "{error}");

        let error = resolve(Path::new("/tmp/does-not-matter"), "  ", "err_abc123")
            .await
            .expect_err("should refuse");
        assert!(error.contains("[tools]"), "{error}");
    }

    #[tokio::test]
    async fn an_unpublished_path_is_refused_before_any_network_call() {
        let dir =
            std::env::temp_dir().join(format!("boxcode-errors-unpublished-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        let target = dir.join("index.html");
        std::fs::write(&target, "hi").expect("write");

        // A bogus endpoint would fail this differently (a connection error)
        // if the code got as far as trying to reach it -- the assertion on
        // the message is what proves it was refused *before* that, for the
        // right reason.
        let error = list_pending(&target, "http://127.0.0.1:1").await.expect_err("should refuse");
        assert!(error.contains("publish_artifact"), "{error}");

        let error =
            resolve(&target, "http://127.0.0.1:1", "err_abc123").await.expect_err("should refuse");
        assert!(error.contains("publish_artifact"), "{error}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A minimal HTTP/1.1 server on a real socket, same pattern as
    /// `db.rs`'s `serve_once_and_capture_body` -- proves `list_pending`
    /// actually parses a real response and `resolve` actually POSTs the
    /// right body, not just that both functions refuse the cases above.
    async fn serve_once_and_capture_body(
        response_json: &'static str,
    ) -> (String, tokio::task::JoinHandle<String>) {
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
                    // A GET (`list_pending`) has no body and so no
                    // Content-Length header -- `None` here means "no body
                    // expected", not "wait for more." Without this the loop
                    // would wait forever for a body that never arrives and
                    // the client would time out (which is exactly what
                    // happened before this fix).
                    let body_so_far = buf.len() - (header_end + 4);
                    let body_complete = content_length.map(|cl| body_so_far >= cl).unwrap_or(true);
                    if body_complete {
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
    /// private to that module -- same helper `db.rs`'s tests use.
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

    /// Same reasoning as `db.rs`'s `an_access_token_is_forwarded...`: this
    /// test needs `list_pending`/`resolve`'s own `.await`, so it locks
    /// `HOME_LOCK` directly rather than going through `with_isolated_home`.
    #[tokio::test]
    async fn list_pending_parses_a_real_response_and_resolve_posts_the_project_id() {
        let _guard = crate::config::test_support::HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let fake_home = tempfile::tempdir().expect("temp home");
        let prev_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", fake_home.path());

        let project = tempfile::tempdir().expect("project dir");
        std::fs::write(project.path().join("index.html"), "hi").expect("write");
        fake_publish(fake_home.path(), project.path(), "proj-with-errors");

        let response = r#"[{"id":"err_1","message":"TypeError: x is undefined","file":"/checkout.js","line":12,"col":4,"count":3,"first_seen_at":"t0","last_seen_at":"t1"}]"#;
        let (endpoint, handle) = serve_once_and_capture_body(response).await;
        let list_result = list_pending(project.path(), &endpoint).await;

        match prev_home.clone() {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }
        let _ = handle.await.expect("server task");

        let errors = list_result.expect("should parse");
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].id, "err_1");
        assert_eq!(errors[0].count, 3);
        assert_eq!(describe(&errors[0]), "[runtime error (×3)] TypeError: x is undefined — /checkout.js:12:4");

        // Second round: resolve, and check the request body actually names
        // the right project -- same "sent the right thing on the wire"
        // proof as db.rs's access-token tests.
        std::env::set_var("HOME", fake_home.path());
        let (endpoint, handle) = serve_once_and_capture_body("{}").await;
        let resolve_result = resolve(project.path(), &endpoint, "err_1").await;

        match prev_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }

        assert!(resolve_result.is_ok(), "{resolve_result:?}");
        let body = handle.await.expect("server task");
        assert!(body.contains("\"project_id\":\"proj-with-errors\""), "{body}");
    }

    #[test]
    fn describe_shows_the_count_only_when_it_is_more_than_one() {
        let once = ReportedError {
            id: "err_1".into(),
            message: "TypeError: x is undefined".into(),
            file: "/checkout.js".into(),
            line: 12,
            col: 4,
            count: 1,
            first_seen_at: "t0".into(),
            last_seen_at: "t0".into(),
        };
        assert_eq!(
            describe(&once),
            "[runtime error] TypeError: x is undefined — /checkout.js:12:4"
        );

        let repeated = ReportedError { count: 7, ..once };
        assert_eq!(
            describe(&repeated),
            "[runtime error (×7)] TypeError: x is undefined — /checkout.js:12:4"
        );
    }

    #[test]
    fn ids_are_prefixed_so_resolve_change_request_can_route_on_them() {
        assert!("err_abc123".starts_with(ID_PREFIX));
        assert!(!"1".starts_with(ID_PREFIX));
    }
}
