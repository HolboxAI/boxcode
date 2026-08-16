//! The change-request mailbox for a published boxcode artifact.
//!
//! Same identity story as `auth.rs`/`db.rs`: a project's id to the rest of
//! boxcode *is* the artifact id it already published under
//! (`artifacts::remembered_id`), never a second id invented here.
//!
//! The other half of this feature is not in this file at all: a small,
//! generic, dependency-free JS widget (`infra/requests/control-plane`'s
//! `GET /requests-widget.js`) that a developer adds to their own published
//! page's HTML with `edit_file` -- one `<script>` tag, no code in this
//! repo generates or owns it -- so a visitor can leave a plain-English
//! request without running boxcode themselves. That widget only ever
//! *submits*; it holds no key and cannot resolve anything, so a request
//! sitting in the mailbox is not itself an edit. Someone still has to
//! read it, make the change with the ordinary agent loop, and republish --
//! this module is only how boxcode fetches what is waiting and marks it
//! handled once it has been.

use std::path::Path;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct ChangeRequest {
    pub id: String,
    pub text: String,
    pub created_at: String,
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
            "no requests endpoint is configured. Set `requests_endpoint` under [tools] in \
             ~/.boxcode/config.toml."
                .to_string(),
        );
    }
    Ok(())
}

fn require_published(path: &Path) -> Result<String, String> {
    crate::artifacts::remembered_id(path).ok_or_else(|| {
        "this has not been published yet. Call publish_artifact on it first -- the change-\
         request mailbox belongs to a project, not a substitute for one."
            .to_string()
    })
}

/// The pending change requests waiting for the project published at `path`.
/// `endpoint` is the control-plane's `/requests` URL.
pub async fn list_pending(path: &Path, endpoint: &str) -> Result<Vec<ChangeRequest>, String> {
    require_endpoint(endpoint)?;
    let project_id = require_published(path)?;

    let client = http_client()?;
    let response = client
        .get(endpoint)
        .query(&[("project_id", project_id.as_str())])
        .send()
        .await
        .map_err(|e| format!("could not reach the requests service: {e}"))?;

    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!("the requests service refused this ({status}): {}", text.trim()));
    }
    serde_json::from_str(&text)
        .map_err(|e| format!("the requests service returned something unexpected ({e})"))
}

/// Mark change request `id` for the project published at `path` as handled,
/// so it stops showing up as pending. `endpoint` is the control-plane's
/// `/requests` URL (the same one `list_pending` uses); the resolve call goes
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
        .map_err(|e| format!("could not reach the requests service: {e}"))?;

    let status = response.status();
    if !status.is_success() {
        let text = response.text().await.unwrap_or_default();
        return Err(format!("the requests service refused this ({status}): {}", text.trim()));
    }
    Ok(())
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

        let error = resolve(Path::new("/tmp/does-not-matter"), "  ", "abc123")
            .await
            .expect_err("should refuse");
        assert!(error.contains("[tools]"), "{error}");
    }

    #[tokio::test]
    async fn an_unpublished_path_is_refused_before_any_network_call() {
        let dir =
            std::env::temp_dir().join(format!("boxcode-requests-unpublished-{}", std::process::id()));
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

        let error = resolve(&target, "http://127.0.0.1:1", "abc123").await.expect_err("should refuse");
        assert!(error.contains("publish_artifact"), "{error}");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
