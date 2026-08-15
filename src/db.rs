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
pub async fn query(
    path: &Path,
    endpoint: &str,
    sql: &str,
    params: &[serde_json::Value],
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

    let body = serde_json::json!({ "project_id": project_id, "key": key, "sql": sql, "params": params })
        .to_string();
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
        let error = query(Path::new("/tmp/does-not-matter"), "  ", "SELECT 1", &[])
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

        let error = query(&target, "http://127.0.0.1:1", "SELECT 1", &[])
            .await
            .expect_err("should refuse");
        assert!(error.contains("publish_artifact"), "{error}");

        let _ = std::fs::remove_dir_all(&dir);
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
