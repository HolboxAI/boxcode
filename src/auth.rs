//! Turning an already-published artifact into one with sign-up/sign-in.
//!
//! A project's identity to the rest of boxcode is the artifact id it
//! already published under (`artifacts::remembered_id`) -- there is no
//! second, auth-specific id to keep in sync with the first. `provision`
//! sends that id to the auth control-plane (`infra/auth/` in this repo,
//! running on its own EC2 box, not this endpoint's concern), which
//! idempotently stands up a per-project GoTrue container and returns the
//! base URL it answers on. What runs behind that URL, and why it is a
//! separate box rather than a Lambda, is `infra/auth/README.md`'s story;
//! this module only has to know the one request/response shape.

use std::path::Path;
use std::time::Duration;

#[derive(Debug)]
pub struct Provisioned {
    pub auth_url: String,
}

#[derive(serde::Deserialize)]
struct ProvisionResponse {
    auth_url: String,
}

/// Provision (or recover) the auth endpoint for the project published at
/// `path`. `endpoint` is the control-plane's `/provision` URL.
///
/// Requires `path` to have already been published: auth is something you
/// add to a project, not a way to create one, and the project id this needs
/// only exists once `publish_artifact` has minted it.
pub async fn provision(path: &Path, endpoint: &str) -> Result<Provisioned, String> {
    if endpoint.trim().is_empty() {
        return Err(
            "no auth endpoint is configured. Set `auth_endpoint` under [tools] in \
             ~/.boxcode/config.toml."
                .to_string(),
        );
    }
    let Some(project_id) = crate::artifacts::remembered_id(path) else {
        return Err(
            "this has not been published yet. Call publish_artifact on it first -- auth is \
             added to a project, not a substitute for one."
                .to_string(),
        );
    };

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent(concat!("boxcode/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| format!("could not build an HTTP client: {e}"))?;

    let body = serde_json::json!({ "project_id": project_id }).to_string();
    let response = client
        .post(endpoint)
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .map_err(|e| format!("could not reach the auth service: {e}"))?;

    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!("the auth service refused this ({status}): {}", text.trim()));
    }
    let parsed: ProvisionResponse = serde_json::from_str(&text)
        .map_err(|e| format!("the auth service returned something unexpected ({e})"))?;

    Ok(Provisioned { auth_url: parsed.auth_url })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn an_unconfigured_endpoint_explains_itself() {
        let error = provision(Path::new("/tmp/does-not-matter"), "  ").await.expect_err("should refuse");
        assert!(error.contains("[tools]"), "{error}");
    }

    #[tokio::test]
    async fn an_unpublished_path_is_refused_before_any_network_call() {
        crate::config::test_support::with_isolated_home(|| {});
        let dir = std::env::temp_dir().join(format!("boxcode-auth-unpublished-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        let target = dir.join("index.html");
        std::fs::write(&target, "hi").expect("write");

        // A bogus endpoint would fail this differently (a connection error)
        // if the code got as far as trying to reach it -- the assertion on
        // the message is what proves it was refused *before* that, for the
        // right reason.
        let error = provision(&target, "http://127.0.0.1:1").await.expect_err("should refuse");
        assert!(error.contains("publish_artifact"), "{error}");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
