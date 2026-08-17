//! `boxcode --upgrade` — pull the latest build without having to
//! remember (or re-paste) the curl one-liner.
//!
//! The install itself is delegated to `install.sh` (or, on Windows,
//! `install.ps1`) fetched from the repo, not reimplemented here. Those
//! scripts already know how to pick an install directory, escalate
//! privileges, sweep stale copies off `PATH`, and verify what the shell
//! actually resolves to. A second install path written in Rust would only
//! drift away from them.
//!
//! Which platform's installer to fetch and how to run it is threaded through
//! as a `windows: bool` parameter (`run` fixes it to `cfg!(windows)`; tests
//! pass either value directly) rather than branching on `cfg!(windows)`
//! inline -- a compile-time `cfg!` would make the Windows branch of this
//! logic permanently untestable on any machine that isn't Windows.

use std::error::Error;
use std::process::Command;
use std::time::Duration;

const REPO: &str = "HolboxAI/boxcode";
const BRANCH: &str = "main";
const CURRENT: &str = env!("CARGO_PKG_VERSION");

/// Where to fetch `Cargo.toml` and `install.sh` from. Overridable so a fork or
/// an internal mirror (this tool is often run somewhere with no route to
/// github.com) can serve its own builds.
const URL_BASE_ENV: &str = "BOXCODE_UPGRADE_URL_BASE";

fn join_url(base: &str, path: &str) -> String {
    format!("{}/{}", base.trim_end_matches('/'), path)
}

fn base_url() -> String {
    std::env::var(URL_BASE_ENV)
        .ok()
        .filter(|base| !base.trim().is_empty())
        .unwrap_or_else(|| format!("https://raw.githubusercontent.com/{REPO}/{BRANCH}"))
}

fn raw_url(path: &str) -> String {
    join_url(&base_url(), path)
}

/// Turns the startup check off from the environment, for the cases config
/// cannot reach: a CI job, a container, a scripted run.
const NO_CHECK_ENV: &str = "BOXCODE_NO_UPDATE_CHECK";

/// How long the startup check may take before it is abandoned.
///
/// Far shorter than the explicit `--upgrade` timeouts below, and deliberately:
/// there the user asked for an upgrade and will wait for it, whereas here they
/// asked to start the app and a slow network must not be allowed to hold that
/// up. Two seconds is enough for a healthy request and short enough that a
/// dead one is not felt.
const CHECK_TIMEOUT: Duration = Duration::from_secs(2);

fn last_check_path() -> Option<std::path::PathBuf> {
    Some(crate::config::Config::config_dir().join("last_update_check"))
}

/// Whether the once-a-day gate has already been spent.
///
/// Split out so the gate can be tested against a plain file, rather than by
/// standing up an isolated `$HOME` around an async call. A missing or
/// unreadable stamp reads as "not yet today", which is the right way round:
/// the cost of an extra check is one request, the cost of skipping one is a
/// user sitting on a stale build.
fn already_checked_today(path: &std::path::Path, today: &str) -> bool {
    std::fs::read_to_string(path)
        .map(|stamp| stamp.trim() == today)
        .unwrap_or(false)
}

/// Whether a newer release exists, when that can be answered cheaply.
///
/// `None` for every reason other than "there is a newer version": checked
/// already today, turned off, offline, timed out, a malformed manifest. None
/// of those are worth a word on screen -- someone starting a coding assistant
/// wants the assistant, not a report on an update check that failed.
///
/// Once a day rather than every launch, on the same reasoning (and with the
/// same file-stamp trick) as `telemetry::ping_active_if_new_day`: a prompt
/// that appears every single time is one people learn to dismiss without
/// reading, which is worse than not asking.
pub async fn check_on_start(enabled: bool) -> Option<String> {
    if !enabled {
        return None;
    }
    if std::env::var_os(NO_CHECK_ENV).is_some() {
        return None;
    }

    let path = last_check_path()?;
    check_against(&base_url(), &path, &crate::dateutil::today_string()).await
}

/// The check itself, with everything it depends on passed in.
///
/// Where it looks, where it stamps and what day it is are all arguments
/// rather than globals, so the tests drive it against a local server and a
/// temporary file. The alternative -- reaching for `$HOME` and
/// `BOXCODE_UPGRADE_URL_BASE` from inside a test -- mutates process-wide state
/// that every other test in this file shares, and duly broke two of them.
async fn check_against(base: &str, stamp: &std::path::Path, today: &str) -> Option<String> {
    if already_checked_today(stamp, today) {
        return None;
    }

    let client = reqwest::Client::builder()
        .connect_timeout(CHECK_TIMEOUT)
        .timeout(CHECK_TIMEOUT)
        .user_agent(format!("boxcode/{CURRENT}"))
        .build()
        .ok()?;

    let latest = fetch_version_from(&client, base).await.ok()?;

    // Stamped only after a request that actually answered. A failed check
    // must not count as today's, or one flaky morning silently skips the
    // whole day.
    if let Some(parent) = stamp.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(stamp, today);

    version_is_newer(&latest, CURRENT).then_some(latest)
}

pub async fn run(force: bool) -> Result<(), Box<dyn Error>> {
    run_for(force, cfg!(windows)).await
}

async fn run_for(force: bool, windows: bool) -> Result<(), Box<dyn Error>> {
    println!("Checking {} for a newer version...", base_url());

    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(30))
        .user_agent(format!("boxcode/{CURRENT}"))
        .build()?;

    match fetch_latest_version(&client).await {
        Ok(latest) if version_is_newer(&latest, CURRENT) => {
            println!("⬆️  {CURRENT} → {latest}");
        }
        Ok(latest) if force => {
            println!("✓ Already on {latest}. Reinstalling anyway (--force).");
        }
        Ok(_) => {
            println!("✓ Already up to date ({CURRENT}). Nothing to do.");
            println!();
            println!("main can also carry changes that haven't been given a new version");
            println!("number yet. To rebuild from the latest source regardless, run:");
            println!("    boxcode --upgrade --force");
            return Ok(());
        }
        Err(e) => {
            // An unreachable network or a moved file shouldn't block an
            // explicit upgrade request — go ahead and let the installer itself
            // fail loudly if it genuinely can't run.
            eprintln!("⚠️  Could not determine the latest version ({e}). Installing anyway.");
        }
    }

    println!("📥 Fetching the installer...");
    let script = fetch_installer(&client, windows).await?;

    let script_path = std::env::temp_dir().join(format!(
        "boxcode-install-{}.{}",
        std::process::id(),
        installer_extension(windows)
    ));
    std::fs::write(&script_path, script)?;
    println!();

    let status = installer_command(&script_path, windows).status();
    let _ = std::fs::remove_file(&script_path);

    match status {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(format!("installer exited with {status}").into()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let interpreter = interpreter_name(windows);
            Err(format!(
                "--upgrade needs `{interpreter}` on PATH, and it wasn't found. \
                 Reinstall manually: https://github.com/HolboxAI/boxcode"
            )
            .into())
        }
        Err(e) => Err(Box::new(e)),
    }
}

async fn fetch_latest_version(client: &reqwest::Client) -> Result<String, Box<dyn Error>> {
    fetch_version_from(client, &base_url()).await
}

/// `fetch_latest_version` against an explicit base, so a caller that already
/// knows where to look does not have to go back through the environment.
async fn fetch_version_from(
    client: &reqwest::Client,
    base: &str,
) -> Result<String, Box<dyn Error>> {
    let response = client.get(join_url(base, "Cargo.toml")).send().await?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("HTTP {status} fetching Cargo.toml").into());
    }
    let body = response.text().await?;
    parse_package_version(&body).ok_or_else(|| "no [package] version in Cargo.toml".into())
}

fn installer_filename(windows: bool) -> &'static str {
    if windows {
        "install.ps1"
    } else {
        "install.sh"
    }
}

fn installer_extension(windows: bool) -> &'static str {
    if windows {
        "ps1"
    } else {
        "sh"
    }
}

fn interpreter_name(windows: bool) -> &'static str {
    if windows {
        "powershell.exe"
    } else {
        "bash"
    }
}

/// The command that will actually run the downloaded installer. Windows'
/// default execution policy blocks running an unsigned, freshly-downloaded
/// `.ps1` at all -- `-ExecutionPolicy Bypass` scopes that override to just
/// this one process, the same way `bash <script>` never needed the script's
/// own execute bit set.
fn installer_command(script_path: &std::path::Path, windows: bool) -> Command {
    let mut cmd = Command::new(interpreter_name(windows));
    if windows {
        cmd.args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"]);
    }
    cmd.arg(script_path);
    cmd
}

async fn fetch_installer(client: &reqwest::Client, windows: bool) -> Result<String, Box<dyn Error>> {
    let url = raw_url(installer_filename(windows));
    let response = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("could not download {url}: {e}"))?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("HTTP {status} fetching {url}").into());
    }
    let body = response.text().await?;
    if !looks_like_installer(&body, windows) {
        return Err(format!("downloaded {} does not look like the installer", installer_filename(windows)).into());
    }
    Ok(body)
}

/// Never hand an unexpected body to the interpreter — a captive-portal login
/// page or an error blob would otherwise be executed as a script. PowerShell
/// has no shebang convention, so `install.ps1` is required to open with a
/// `#`-prefixed comment line instead; either way, a real installer body and
/// stray HTML/JSON are trivially told apart.
fn looks_like_installer(body: &str, windows: bool) -> bool {
    if windows {
        body.trim_start().starts_with('#') && body.contains("boxcode")
    } else {
        body.starts_with("#!") && body.contains("boxcode")
    }
}

/// Reads `version` from the `[package]` table only — a `version = "..."` under
/// `[dependencies]` must not be mistaken for the crate's own version.
fn parse_package_version(manifest: &str) -> Option<String> {
    let mut in_package = false;
    for line in manifest.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_package = line == "[package]";
            continue;
        }
        if !in_package {
            continue;
        }
        if let Some(rest) = line.strip_prefix("version") {
            if let Some(value) = rest.trim_start().strip_prefix('=') {
                return Some(value.trim().trim_matches('"').to_string());
            }
        }
    }
    None
}

fn parse_semver(version: &str) -> Option<(u64, u64, u64)> {
    // Ignore any pre-release/build suffix: 1.2.3-rc1 compares as 1.2.3.
    let core = version.trim().trim_start_matches('v');
    let core = core.split(['-', '+']).next()?;
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().unwrap_or("0").parse().ok()?;
    let patch = parts.next().unwrap_or("0").parse().ok()?;
    Some((major, minor, patch))
}

fn version_is_newer(candidate: &str, current: &str) -> bool {
    match (parse_semver(candidate), parse_semver(current)) {
        (Some(candidate), Some(current)) => candidate > current,
        // Unparseable on either side: treat "a different string" as newer, so an
        // unusual version scheme still offers an upgrade instead of pinning the
        // user to their current build forever.
        _ => candidate.trim() != current.trim(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    // ---- the startup check ---------------------------------------------------
    //
    // Every test here asserts the check does NOTHING. That is the whole point
    // of it: it runs on every launch, before the app the user actually asked
    // for, so the only acceptable failure mode is silence. None of these touch
    // the network -- they assert it never gets that far.

    #[tokio::test]
    async fn the_startup_check_is_skipped_when_switched_off() {
        assert_eq!(check_on_start(false).await, None);
    }

    /// The escape hatch for a CI job or a container, where config may not be
    /// reachable but the environment always is.
    #[tokio::test]
    async fn the_environment_can_switch_the_startup_check_off() {
        crate::config::test_support::with_isolated_home(|| {
            std::env::set_var(NO_CHECK_ENV, "1");
        });
        let verdict = check_on_start(true).await;
        std::env::remove_var(NO_CHECK_ENV);
        assert_eq!(verdict, None);
    }

    /// The check, end to end, against a server that says a newer release
    /// exists. Serves the same `Cargo.toml` the real one reads, so this
    /// exercises the fetch, the parse and the comparison rather than mocking
    /// past them. Nothing process-global is touched: the base URL and the
    /// stamp are arguments, so this cannot race the other tests here.
    #[tokio::test]
    async fn a_newer_release_is_reported_and_the_day_is_stamped() {
        let base = serve_repo("[package]\nversion = \"99.9.9\"\n".to_string(), String::new()).await;
        let dir = std::env::temp_dir().join("boxcode-upd-newer");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let stamp = dir.join("last_update_check");
        let _ = std::fs::remove_file(&stamp);

        let first = check_against(&base, &stamp, "2026-08-13").await;
        // Second call, same day: the stamp the first one wrote must close the
        // gate, or the prompt appears on every single launch.
        let second = check_against(&base, &stamp, "2026-08-13").await;
        // Tomorrow it opens again.
        let tomorrow = check_against(&base, &stamp, "2026-08-14").await;

        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(first.as_deref(), Some("99.9.9"));
        assert_eq!(second, None, "the once-a-day gate did not close");
        assert_eq!(tomorrow.as_deref(), Some("99.9.9"), "the gate never reopened");
    }

    /// The version the user already has must not be offered to them.
    #[tokio::test]
    async fn the_current_version_is_not_offered_as_an_update() {
        let base = serve_repo(format!("[package]\nversion = \"{CURRENT}\"\n"), String::new()).await;
        let dir = std::env::temp_dir().join("boxcode-upd-same");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let stamp = dir.join("last_update_check");
        let _ = std::fs::remove_file(&stamp);

        let verdict = check_against(&base, &stamp, "2026-08-13").await;

        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(verdict, None);
    }

    /// An unreachable server is the common case on a laptop that just woke up,
    /// and it must be silent -- and must NOT stamp the day, or one flaky
    /// morning skips the check until tomorrow.
    #[tokio::test]
    async fn an_unreachable_server_is_silent_and_does_not_stamp_the_day() {
        let dir = std::env::temp_dir().join("boxcode-upd-offline");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let stamp = dir.join("last_update_check");
        let _ = std::fs::remove_file(&stamp);

        // Port 1 refuses immediately; no waiting on the timeout.
        let verdict = check_against("http://127.0.0.1:1", &stamp, "2026-08-13").await;

        let stamped = stamp.exists();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(verdict, None);
        assert!(!stamped, "a failed check must not count as today's");
    }

    /// A prompt on every single launch is one people learn to dismiss without
    /// reading, so a stamp from today closes the gate.
    #[test]
    fn a_check_already_made_today_closes_the_gate() {
        let dir = std::env::temp_dir().join(format!("boxcode-check-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("last_update_check");

        std::fs::write(&path, "2026-08-13\n").expect("stamp");
        assert!(already_checked_today(&path, "2026-08-13"));
        // Yesterday's stamp must not suppress today's check, or the feature
        // silently stops working the day after it is first used.
        assert!(!already_checked_today(&path, "2026-08-14"));

        // A stamp that was never written, or cannot be read, means "not yet":
        // an extra request costs one request, a skipped one leaves someone on
        // a stale build indefinitely.
        std::fs::remove_file(&path).expect("rm");
        assert!(!already_checked_today(&path, "2026-08-13"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reads_the_version_from_the_package_table() {
        let manifest = "\
[package]
name = \"boxcode\"
version = \"0.3.0\"
edition = \"2021\"
";
        assert_eq!(parse_package_version(manifest).as_deref(), Some("0.3.0"));
    }

    #[test]
    fn ignores_dependency_versions() {
        // The bug this guards: `version` under [dependencies] appears *after*
        // the package one in a real manifest, and matching greedily would
        // report a dependency's version as the app's.
        let manifest = "\
[package]
name = \"boxcode\"
version = \"0.3.0\"

[dependencies]
tokio = { version = \"1.35\" }
version = \"9.9.9\"
";
        assert_eq!(parse_package_version(manifest).as_deref(), Some("0.3.0"));
    }

    #[test]
    fn missing_package_version_is_none() {
        assert_eq!(parse_package_version("[dependencies]\nversion = \"1.0\"\n"), None);
        assert_eq!(parse_package_version(""), None);
    }

    #[test]
    fn parses_semver_variants() {
        assert_eq!(parse_semver("0.2.0"), Some((0, 2, 0)));
        assert_eq!(parse_semver("v1.4.9"), Some((1, 4, 9)));
        assert_eq!(parse_semver("1.2.3-rc1"), Some((1, 2, 3)));
        assert_eq!(parse_semver("2.1"), Some((2, 1, 0)));
        assert_eq!(parse_semver("not-a-version"), None);
    }

    #[test]
    fn compares_versions_numerically() {
        assert!(version_is_newer("0.3.0", "0.2.0"));
        assert!(version_is_newer("0.2.1", "0.2.0"));
        assert!(version_is_newer("1.0.0", "0.99.99"));
        // 10 > 9 numerically, but "0.10.0" < "0.9.0" as strings.
        assert!(version_is_newer("0.10.0", "0.9.0"));
    }

    #[test]
    fn same_or_older_version_is_not_newer() {
        assert!(!version_is_newer("0.2.0", "0.2.0"));
        assert!(!version_is_newer("0.1.0", "0.2.0"));
        assert!(!version_is_newer("0.9.0", "0.10.0"));
    }

    #[test]
    fn unparseable_versions_fall_back_to_string_inequality() {
        assert!(version_is_newer("nightly-2", "nightly-1"));
        assert!(!version_is_newer("nightly-1", "nightly-1"));
    }

    #[test]
    fn urls_join_without_doubling_the_slash() {
        let github = format!("https://raw.githubusercontent.com/{REPO}/{BRANCH}");
        assert_eq!(
            join_url(&github, "install.sh"),
            "https://raw.githubusercontent.com/HolboxAI/boxcode/main/install.sh"
        );
        assert_eq!(join_url("http://mirror.internal/", "Cargo.toml"), "http://mirror.internal/Cargo.toml");
    }

    #[test]
    fn only_a_real_installer_reaches_bash() {
        assert!(looks_like_installer("#!/bin/bash\ninstall boxcode\n", false));
        // What a captive portal or an error page would return.
        assert!(!looks_like_installer("<html><body>Sign in to continue</body></html>", false));
        assert!(!looks_like_installer("404: Not Found", false));
        // Right shebang, wrong script.
        assert!(!looks_like_installer("#!/bin/bash\nrm -rf /\n", false));
        assert!(!looks_like_installer("", false));
    }

    /// The Windows counterpart: `install.ps1` has no shebang convention to
    /// check, so this is `looks_like_installer`'s only guard against handing
    /// a captive-portal page or an error blob to PowerShell instead.
    #[test]
    fn only_a_real_installer_reaches_powershell() {
        assert!(looks_like_installer("# boxcode installer\nWrite-Host 'hi'\n", true));
        assert!(!looks_like_installer("<html><body>Sign in to continue</body></html>", true));
        assert!(!looks_like_installer("404: Not Found", true));
        // Right leading comment, wrong script.
        assert!(!looks_like_installer("# just a comment\nRemove-Item -Recurse C:\\\n", true));
        assert!(!looks_like_installer("", true));
    }

    /// Pure logic, not a real subprocess launch (see the module doc for why
    /// `windows: bool` is threaded through rather than checked via
    /// `cfg!(windows)`): proves `run_for` will reach for the right
    /// interpreter, with the right flags, on either platform -- without
    /// needing an actual Windows machine, or PowerShell installed here, to
    /// find out.
    #[test]
    fn installer_command_targets_the_right_interpreter_per_platform() {
        let script = std::path::Path::new("/tmp/whatever-install-script");

        let unix_cmd = installer_command(script, false);
        assert_eq!(unix_cmd.get_program(), "bash");
        let unix_args: Vec<_> = unix_cmd.get_args().collect();
        assert_eq!(unix_args, vec![script.as_os_str()]);

        let windows_cmd = installer_command(script, true);
        assert_eq!(windows_cmd.get_program(), "powershell.exe");
        let windows_args: Vec<_> = windows_cmd.get_args().collect();
        assert_eq!(
            windows_args,
            vec!["-NoProfile", "-ExecutionPolicy", "Bypass", "-File", script.to_str().unwrap()]
        );
    }

    #[test]
    fn installer_filenames_and_extensions_match_per_platform() {
        assert_eq!(installer_filename(false), "install.sh");
        assert_eq!(installer_extension(false), "sh");
        assert_eq!(interpreter_name(false), "bash");

        assert_eq!(installer_filename(true), "install.ps1");
        assert_eq!(installer_extension(true), "ps1");
        assert_eq!(interpreter_name(true), "powershell.exe");
    }

    /// Serve `Cargo.toml` and `install.sh` on an ephemeral port.
    async fn serve_repo(manifest: String, installer: String) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = [0u8; 4096];
                let Ok(n) = socket.read(&mut buf).await else {
                    return;
                };
                let request = String::from_utf8_lossy(&buf[..n]).to_string();
                let body = if request.contains("/Cargo.toml") {
                    manifest.clone()
                } else {
                    installer.clone()
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            }
        });
        format!("http://{addr}")
    }

    /// Every phase lives in one test on purpose: it sets a process-wide env var,
    /// and cargo runs tests in thread-parallel, so two tests racing on it would
    /// be flaky.
    #[tokio::test]
    async fn upgrade_end_to_end() {
        let marker =
            std::env::temp_dir().join(format!("boxcode-upgrade-test-{}", std::process::id()));
        let installer = format!(
            "#!/bin/bash\n# stand-in for the boxcode installer\necho ran > \"{}\"\n",
            marker.display()
        );
        let manifest = |version: &str| format!("[package]\nname = \"boxcode\"\nversion = \"{version}\"\n");
        let ran = || marker.exists();
        let reset = || {
            let _ = std::fs::remove_file(&marker);
        };

        // A newer version upstream: fetch the installer and run it.
        reset();
        let base = serve_repo(manifest("99.0.0"), installer.clone()).await;
        std::env::set_var(URL_BASE_ENV, &base);
        run_for(false, false).await.expect("upgrade should succeed");
        assert!(ran(), "a newer version should have run the installer");

        // Already current: nothing should be installed.
        reset();
        let base = serve_repo(manifest(CURRENT), installer.clone()).await;
        std::env::set_var(URL_BASE_ENV, &base);
        run_for(false, false).await.expect("up-to-date check should succeed");
        assert!(!ran(), "an up-to-date build must not reinstall");

        // ...unless --force is given.
        reset();
        let base = serve_repo(manifest(CURRENT), installer.clone()).await;
        std::env::set_var(URL_BASE_ENV, &base);
        run_for(true, false).await.expect("forced upgrade should succeed");
        assert!(ran(), "--force should reinstall even when up to date");

        // A body that isn't the installer must never be executed.
        reset();
        let base = serve_repo(manifest("99.0.0"), "<html>Sign in</html>".to_string()).await;
        std::env::set_var(URL_BASE_ENV, &base);
        let result = run_for(false, false).await;
        assert!(result.is_err(), "a non-installer body should be rejected");
        assert!(!ran(), "a rejected body must not be executed");

        reset();
        std::env::remove_var(URL_BASE_ENV);
    }

    /// Finds a real PowerShell interpreter already on `PATH` -- `pwsh`
    /// (PowerShell Core, cross-platform, what GitHub's own hosted Linux
    /// runners ship) or `powershell.exe` (Windows) -- without ever touching
    /// this process's own `PATH`. `None` means genuinely absent, not merely
    /// unsearched.
    fn find_real_powershell() -> Option<std::path::PathBuf> {
        let path = std::env::var_os("PATH")?;
        for dir in std::env::split_paths(&path) {
            for name in ["pwsh", "powershell.exe", "pwsh.exe"] {
                let candidate = dir.join(name);
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
        None
    }

    /// The real Windows path, proven against a real PowerShell interpreter
    /// rather than mocked -- skipped, not failed, when none is reachable
    /// (most non-Windows machines). Unlike `upgrade_end_to_end`, this never
    /// touches this *process's* `PATH`: the shim directory (containing a
    /// `powershell.exe` that is actually whatever real interpreter was
    /// found) is attached only to the child process's environment via
    /// `Command::env`, so it cannot race with anything else `cargo test`
    /// runs in parallel.
    #[tokio::test]
    async fn upgrade_end_to_end_on_windows_if_a_real_powershell_is_available() {
        let Some(real_interpreter) = find_real_powershell() else {
            eprintln!("skipping: no pwsh/powershell.exe found on PATH in this environment");
            return;
        };

        let shim_dir = std::env::temp_dir().join(format!("boxcode-pwsh-shim-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&shim_dir);
        std::fs::create_dir_all(&shim_dir).expect("create shim dir");
        let shim_path = shim_dir.join("powershell.exe");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real_interpreter, &shim_path).expect("symlink shim");
        #[cfg(not(unix))]
        std::fs::copy(&real_interpreter, &shim_path).expect("copy shim");

        let marker =
            std::env::temp_dir().join(format!("boxcode-upgrade-windows-test-{}", std::process::id()));
        let _ = std::fs::remove_file(&marker);
        // PowerShell, not batch: `install.ps1` is what actually ships.
        let installer = format!(
            "# boxcode installer (test stand-in)\nSet-Content -Path '{}' -Value 'ran'\n",
            marker.display()
        );
        let manifest = "[package]\nname = \"boxcode\"\nversion = \"99.0.0\"\n";
        let base = serve_repo(manifest.to_string(), installer).await;

        let script_path = std::env::temp_dir().join(format!("boxcode-install-{}.ps1", std::process::id()));
        // Mirrors fetch_installer's download, minus the HTTP round trip --
        // the point of this test is proving the *interpreter invocation*
        // works for real, which fetch_installer's own logic already has
        // dedicated coverage for above.
        let client = reqwest::Client::new();
        let body = client
            .get(format!("{base}/install.ps1"))
            .send()
            .await
            .expect("fetch stand-in installer")
            .text()
            .await
            .expect("read stand-in installer body");
        std::fs::write(&script_path, &body).expect("write script");

        let mut cmd = installer_command(&script_path, true);
        // The one line that makes this a real, not mocked, invocation: the
        // child process's PATH gains the shim directory, so
        // `Command::new("powershell.exe")` inside installer_command
        // resolves to the real interpreter found above -- entirely within
        // this one subprocess's environment.
        let shim_path_str = shim_dir.to_str().expect("shim dir is valid UTF-8");
        let child_path = match std::env::var_os("PATH") {
            Some(existing) => format!("{shim_path_str}:{}", existing.to_string_lossy()),
            None => shim_path_str.to_string(),
        };
        cmd.env("PATH", child_path);

        let status = cmd.status().expect("run the real (shimmed) powershell.exe");
        let _ = std::fs::remove_file(&script_path);
        let _ = std::fs::remove_dir_all(&shim_dir);

        assert!(status.success(), "the real PowerShell interpreter should have run the stand-in installer");
        assert!(marker.exists(), "the stand-in installer should have run and left its marker");
        let _ = std::fs::remove_file(&marker);
    }
}
