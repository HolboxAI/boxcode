//! Working out what a project is before deploying it.
//!
//! Purely a read of the filesystem: no network, no subprocess, no package
//! manager invoked. That matters because this runs the instant `/deploy` is
//! typed, before any provider has been chosen -- it has to be fast enough to
//! feel like the answer was already known, and it has to work with no CLI
//! installed and nobody signed in.
//!
//! Detection is ordered most specific first. A Next.js project also has React
//! in its dependencies, and a SvelteKit project also has Vite, so a rule that
//! fired on the general case would file every framework under its substrate.
//!
//! Everything here is a *default*, not a decision: `service.rs` shows what was
//! found and lets it be overridden before anything runs. The cost of guessing
//! wrong is therefore an edit, not a failed deployment -- which is the reason
//! this can afford to guess at all.

use serde_json::Value;
use std::path::{Path, PathBuf};

/// What the project is built with.
///
/// `Unknown` is a real answer, not a failure: a directory with a `build`
/// script and no framework this build recognises is still perfectly
/// deployable, and saying so beats guessing at a name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Framework {
    NextJs,
    Nuxt,
    Astro,
    SvelteKit,
    Remix,
    Vite,
    React,
    Node,
    Static,
    Unknown,
}

impl Framework {
    pub fn label(&self) -> String {
        match self {
            Framework::NextJs => "Next.js".to_string(),
            Framework::Nuxt => "Nuxt".to_string(),
            Framework::Astro => "Astro".to_string(),
            Framework::SvelteKit => "SvelteKit".to_string(),
            Framework::Remix => "Remix".to_string(),
            Framework::Vite => "Vite".to_string(),
            Framework::React => "React".to_string(),
            Framework::Node => "Node.js".to_string(),
            Framework::Static => "Static HTML".to_string(),
            Framework::Unknown => "Unknown".to_string(),
        }
    }

    /// The build a project of this kind normally needs, when its own
    /// `package.json` does not name one.
    pub fn default_build_command(&self) -> Option<&'static str> {
        match self {
            Framework::Static | Framework::Unknown => None,
            // A plain Node service is run, not built -- inventing an
            // `npm run build` for it produces a missing-script failure that
            // looks like our bug rather than a wrong guess.
            Framework::Node => None,
            _ => Some("npm run build"),
        }
    }

    /// Where the build leaves the files a provider should serve.
    ///
    /// `None` where the provider works it out itself, which is the honest
    /// answer for the frameworks with first-class support on both platforms --
    /// passing a directory there would override a correct answer with a guess.
    pub fn default_output_dir(&self) -> Option<&'static str> {
        match self {
            Framework::NextJs | Framework::Nuxt | Framework::Remix => None,
            Framework::Vite | Framework::SvelteKit => Some("dist"),
            Framework::Astro => Some("dist"),
            Framework::React => Some("build"),
            Framework::Static => Some("."),
            Framework::Node | Framework::Unknown => None,
        }
    }

    /// True when the provider infers the output itself and should not be told.
    pub fn output_is_provider_managed(&self) -> bool {
        matches!(self, Framework::NextJs | Framework::Nuxt | Framework::Remix)
    }

    /// Whether this is something a static host can serve at all. A bare Node
    /// server is the one case worth warning about before a deploy runs.
    pub fn is_static_hostable(&self) -> bool {
        !matches!(self, Framework::Node)
    }
}

/// Which package manager the project uses, from its lockfile.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PackageManager {
    Npm,
    Pnpm,
    Yarn,
    Bun,
}

impl PackageManager {
    pub fn label(self) -> &'static str {
        match self {
            PackageManager::Npm => "npm",
            PackageManager::Pnpm => "pnpm",
            PackageManager::Yarn => "yarn",
            PackageManager::Bun => "bun",
        }
    }

    /// How this manager spells "run the build script".
    fn run_build(self) -> &'static str {
        match self {
            PackageManager::Npm => "npm run build",
            PackageManager::Pnpm => "pnpm build",
            // `yarn build` works for both Yarn 1 and Berry.
            PackageManager::Yarn => "yarn build",
            PackageManager::Bun => "bun run build",
        }
    }
}

/// Everything detection worked out about a project.
#[derive(Clone, Debug, PartialEq)]
pub struct ProjectProfile {
    pub root: PathBuf,
    /// The deployable name: `package.json`'s `name`, else the directory's.
    pub name: String,
    pub framework: Framework,
    pub package_manager: PackageManager,
    pub build_command: Option<String>,
    pub output_dir: Option<String>,
    /// The files detection actually keyed on, shown on the confirm screen so a
    /// wrong guess is diagnosable without reading this source.
    pub markers: Vec<String>,
    /// Things worth saying before deploying, none of them fatal.
    pub warnings: Vec<String>,
    pub has_vercel_config: bool,
    pub has_netlify_config: bool,
}

impl ProjectProfile {
    /// Whether a provider config file already commits this project somewhere.
    pub fn configured_for(&self, provider_id: &str) -> bool {
        match provider_id {
            "vercel" => self.has_vercel_config,
            "netlify" => self.has_netlify_config,
            _ => false,
        }
    }
}

/// Why a directory cannot be deployed at all.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DetectError {
    NotADirectory(PathBuf),
    /// Nothing that could be served or built: no manifest, no entry page.
    NothingToDeploy(PathBuf),
}

impl std::fmt::Display for DetectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DetectError::NotADirectory(path) => {
                write!(f, "{} is not a directory", path.display())
            }
            DetectError::NothingToDeploy(path) => write!(
                f,
                "{} has no package.json and no index.html, so there is nothing to build or serve. \
                 Point /deploy at the directory that holds the project.",
                path.display()
            ),
        }
    }
}

/// Config files that identify a framework, checked before any dependency is.
/// A config file on disk is a stronger signal than a transitive dependency.
const NEXT_CONFIGS: &[&str] = &[
    "next.config.js",
    "next.config.mjs",
    "next.config.ts",
    "next.config.cjs",
];
const NUXT_CONFIGS: &[&str] = &["nuxt.config.js", "nuxt.config.ts", "nuxt.config.mjs"];
const ASTRO_CONFIGS: &[&str] = &["astro.config.mjs", "astro.config.js", "astro.config.ts"];
const SVELTE_CONFIGS: &[&str] = &["svelte.config.js", "svelte.config.mjs"];
const REMIX_CONFIGS: &[&str] = &["remix.config.js", "remix.config.mjs"];
const VITE_CONFIGS: &[&str] = &[
    "vite.config.js",
    "vite.config.ts",
    "vite.config.mjs",
    "vite.config.cjs",
];

/// Inspect `root` and work out how to deploy it.
pub fn detect(root: &Path) -> Result<ProjectProfile, DetectError> {
    if !root.is_dir() {
        return Err(DetectError::NotADirectory(root.to_path_buf()));
    }

    let manifest = read_package_json(root);
    let has_index_html = exists_any(root, &["index.html", "public/index.html", "src/index.html"]);

    if manifest.is_none() && !has_index_html {
        return Err(DetectError::NothingToDeploy(root.to_path_buf()));
    }

    let mut markers = Vec::new();
    let mut warnings = Vec::new();

    let package_manager = detect_package_manager(root, &mut markers);
    let (framework, scripts) = detect_framework(root, manifest.as_ref(), has_index_html, &mut markers);

    let name = manifest
        .as_ref()
        .and_then(|m| m.get("name"))
        .and_then(Value::as_str)
        .map(sanitize_name)
        .filter(|n| !n.is_empty())
        .or_else(|| {
            root.file_name()
                .map(|n| sanitize_name(&n.to_string_lossy()))
                .filter(|n| !n.is_empty())
        })
        .unwrap_or_else(|| "project".to_string());

    // The project's own build script wins over the framework default: it is
    // what the person who wrote the project actually runs.
    let build_command = if scripts.contains(&"build".to_string()) {
        Some(package_manager.run_build().to_string())
    } else {
        framework.default_build_command().map(str::to_string)
    };

    let output_dir = framework.default_output_dir().map(str::to_string);

    if manifest.is_some() && !scripts.contains(&"build".to_string()) && framework != Framework::Static
    {
        warnings.push(
            "package.json has no `build` script, so the build step may do nothing.".to_string(),
        );
    }
    if !framework.is_static_hostable() {
        // Named rather than guessed at where possible. "This looks like a
        // long-running Node server" is true of an Express app and of a CLI
        // with a start script, and only one of those is worth warning about
        // in the same words -- so ask what kind of server it actually is.
        let named = super::backend::detect_backend(root)
            .ok()
            .filter(|b| b.framework.is_recognised())
            .map(|b| format!("This is {} ({}).", b.framework.label(), b.runtime.label()))
            // The unchanged wording when nothing more specific is known. This
            // branch only fires for `Framework::Node`, so "Node" is accurate
            // even when the backend detector cannot name a framework.
            .unwrap_or_else(|| "This looks like a long-running Node server.".to_string());
        warnings.push(format!(
            "{named} Vercel and Netlify serve static output and serverless functions, so a \
             plain `node server.js` app will not run as-is."
        ));
    }
    if let Some(dir) = &output_dir {
        if dir != "." && !root.join(dir).exists() {
            warnings.push(format!(
                "Output directory '{dir}' does not exist yet — it should appear once the build runs."
            ));
        }
    }
    if exists_any(root, &[".env"]) {
        // Named, not read: the point is that it exists and will not travel.
        warnings.push(
            ".env is present locally. Its values are not uploaded — add anything the build needs \
             as an environment variable in the next step."
                .to_string(),
        );
    }

    let has_vercel_config = mark_if_present(root, &["vercel.json", ".vercel/project.json"], &mut markers);
    let has_netlify_config = mark_if_present(root, &["netlify.toml", ".netlify/state.json"], &mut markers);

    Ok(ProjectProfile {
        root: root.to_path_buf(),
        name,
        framework,
        package_manager,
        build_command,
        output_dir,
        markers,
        warnings,
        has_vercel_config,
        has_netlify_config,
    })
}

fn read_package_json(root: &Path) -> Option<Value> {
    let text = std::fs::read_to_string(root.join("package.json")).ok()?;
    // A malformed manifest is not a detection failure: the directory is still
    // deployable, we just know less about it. Falling over here would refuse a
    // project the provider's own CLI would happily build.
    serde_json::from_str(&text).ok()
}

fn exists_any(root: &Path, candidates: &[&str]) -> bool {
    candidates.iter().any(|c| root.join(c).exists())
}

/// Record every candidate that exists, and report whether any did.
fn mark_if_present(root: &Path, candidates: &[&str], markers: &mut Vec<String>) -> bool {
    let mut found = false;
    for candidate in candidates {
        if root.join(candidate).exists() {
            markers.push((*candidate).to_string());
            found = true;
        }
    }
    found
}

fn detect_package_manager(root: &Path, markers: &mut Vec<String>) -> PackageManager {
    for (lockfile, manager) in [
        ("pnpm-lock.yaml", PackageManager::Pnpm),
        ("bun.lockb", PackageManager::Bun),
        ("yarn.lock", PackageManager::Yarn),
        ("package-lock.json", PackageManager::Npm),
    ] {
        if root.join(lockfile).exists() {
            markers.push(lockfile.to_string());
            return manager;
        }
    }
    PackageManager::Npm
}

/// The framework, plus the manifest's script names (needed by the caller and
/// already parsed here).
fn detect_framework(
    root: &Path,
    manifest: Option<&Value>,
    has_index_html: bool,
    markers: &mut Vec<String>,
) -> (Framework, Vec<String>) {
    let dependencies = collect_dependencies(manifest);
    let scripts = collect_scripts(manifest);

    // Config file first, dependency second -- see the module doc on ordering.
    let rules: &[(&[&str], &[&str], Framework)] = &[
        (NEXT_CONFIGS, &["next"], Framework::NextJs),
        (NUXT_CONFIGS, &["nuxt", "nuxt3"], Framework::Nuxt),
        (ASTRO_CONFIGS, &["astro"], Framework::Astro),
        (SVELTE_CONFIGS, &["@sveltejs/kit"], Framework::SvelteKit),
        (REMIX_CONFIGS, &["@remix-run/dev", "@remix-run/node"], Framework::Remix),
        (VITE_CONFIGS, &["vite"], Framework::Vite),
        (&[], &["react-scripts"], Framework::React),
        (&[], &["react", "react-dom"], Framework::React),
    ];

    for (configs, deps, framework) in rules {
        if mark_if_present(root, configs, markers) {
            return (framework.clone(), scripts);
        }
        if deps.iter().any(|d| dependencies.contains(&d.to_string())) {
            let named = deps
                .iter()
                .find(|d| dependencies.contains(&d.to_string()))
                .expect("just matched");
            markers.push(format!("package.json → {named}"));
            return (framework.clone(), scripts);
        }
    }

    if manifest.is_some() {
        markers.push("package.json".to_string());
        // A manifest with a build script but no framework we know: treat it as
        // a buildable site rather than a server, since that is what a build
        // script implies.
        if scripts.contains(&"build".to_string()) {
            return (Framework::Unknown, scripts);
        }
        if scripts.contains(&"start".to_string())
            || manifest.and_then(|m| m.get("main")).is_some()
        {
            return (Framework::Node, scripts);
        }
        return (Framework::Unknown, scripts);
    }

    if has_index_html {
        markers.push("index.html".to_string());
        return (Framework::Static, scripts);
    }

    (Framework::Unknown, scripts)
}

fn collect_dependencies(manifest: Option<&Value>) -> Vec<String> {
    let mut out = Vec::new();
    let Some(manifest) = manifest else { return out };
    for table in ["dependencies", "devDependencies", "peerDependencies"] {
        if let Some(Value::Object(map)) = manifest.get(table) {
            out.extend(map.keys().cloned());
        }
    }
    out
}

fn collect_scripts(manifest: Option<&Value>) -> Vec<String> {
    match manifest.and_then(|m| m.get("scripts")) {
        Some(Value::Object(map)) => map.keys().cloned().collect(),
        _ => Vec::new(),
    }
}

/// Both providers accept lowercase letters, digits and hyphens in a project
/// name. A scoped npm name (`@acme/site`) and a capitalised one are both
/// ordinary and both rejected verbatim, so they are folded here rather than
/// left to fail at the far end of a deployment.
pub fn sanitize_name(raw: &str) -> String {
    let stripped = raw.rsplit('/').next().unwrap_or(raw);
    let mut out = String::with_capacity(stripped.len());
    let mut last_was_hyphen = false;
    for ch in stripped.chars() {
        let mapped = if ch.is_ascii_alphanumeric() {
            ch.to_ascii_lowercase()
        } else {
            '-'
        };
        if mapped == '-' {
            if last_was_hyphen || out.is_empty() {
                continue;
            }
            last_was_hyphen = true;
        } else {
            last_was_hyphen = false;
        }
        out.push(mapped);
    }
    // Providers cap this well above anything a person types; the trim keeps a
    // pathological directory name from producing an invalid request.
    let trimmed = out.trim_end_matches('-');
    trimmed.chars().take(52).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A project directory with whatever files a test needs.
    fn project(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("temp dir");
        for (path, contents) in files {
            let full = dir.path().join(path);
            if let Some(parent) = full.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(full, contents).unwrap();
        }
        dir
    }

    fn manifest(extra: &str) -> String {
        format!("{{\"name\": \"my-app\", {extra}}}")
    }

    #[test]
    fn next_js_is_detected_from_its_config_file() {
        let dir = project(&[
            ("package.json", &manifest("\"scripts\": {\"build\": \"next build\"}")),
            ("next.config.js", "module.exports = {}"),
        ]);
        let profile = detect(dir.path()).expect("detects");
        assert_eq!(profile.framework, Framework::NextJs);
        assert!(profile.markers.iter().any(|m| m == "next.config.js"), "{:?}", profile.markers);
        // Both providers infer Next.js output themselves; overriding it with a
        // guess is worse than saying nothing.
        assert_eq!(profile.output_dir, None);
        assert!(profile.framework.output_is_provider_managed());
    }

    /// A Next.js project also has React in its dependencies. Detection that
    /// fired on the general case would file every framework under its
    /// substrate -- this is the ordering property, tested directly.
    #[test]
    fn next_js_wins_over_the_react_dependency_it_also_has() {
        let dir = project(&[(
            "package.json",
            &manifest("\"dependencies\": {\"next\": \"14.0.0\", \"react\": \"18.0.0\"}"),
        )]);
        assert_eq!(detect(dir.path()).unwrap().framework, Framework::NextJs);
    }

    /// Same again one level down: SvelteKit is built on Vite.
    #[test]
    fn sveltekit_wins_over_the_vite_it_is_built_on() {
        let dir = project(&[
            ("package.json", &manifest("\"devDependencies\": {\"vite\": \"5\", \"@sveltejs/kit\": \"2\"}")),
            ("svelte.config.js", "export default {}"),
            ("vite.config.js", "export default {}"),
        ]);
        assert_eq!(detect(dir.path()).unwrap().framework, Framework::SvelteKit);
    }

    #[test]
    fn vite_is_detected_and_defaults_to_dist() {
        let dir = project(&[
            ("package.json", &manifest("\"scripts\": {\"build\": \"vite build\"}")),
            ("vite.config.ts", "export default {}"),
        ]);
        let profile = detect(dir.path()).expect("detects");
        assert_eq!(profile.framework, Framework::Vite);
        assert_eq!(profile.output_dir.as_deref(), Some("dist"));
        assert_eq!(profile.build_command.as_deref(), Some("npm run build"));
    }

    #[test]
    fn create_react_app_is_detected_and_defaults_to_build() {
        let dir = project(&[(
            "package.json",
            &manifest("\"dependencies\": {\"react-scripts\": \"5\"}, \"scripts\": {\"build\": \"react-scripts build\"}"),
        )]);
        let profile = detect(dir.path()).expect("detects");
        assert_eq!(profile.framework, Framework::React);
        assert_eq!(profile.output_dir.as_deref(), Some("build"));
    }

    #[test]
    fn a_plain_node_service_is_recognised_and_warned_about() {
        let dir = project(&[(
            "package.json",
            &manifest("\"main\": \"server.js\", \"scripts\": {\"start\": \"node server.js\"}"),
        )]);
        let profile = detect(dir.path()).expect("detects");
        assert_eq!(profile.framework, Framework::Node);
        // A build command invented for it would fail with a missing script,
        // which reads as our bug rather than a wrong guess.
        assert_eq!(profile.build_command, None);
        assert!(
            profile.warnings.iter().any(|w| w.contains("long-running Node server")),
            "{:?}",
            profile.warnings
        );
    }

    #[test]
    fn a_bare_html_page_is_a_static_site_served_from_the_root() {
        let dir = project(&[("index.html", "<h1>hi</h1>")]);
        let profile = detect(dir.path()).expect("detects");
        assert_eq!(profile.framework, Framework::Static);
        assert_eq!(profile.build_command, None);
        assert_eq!(profile.output_dir.as_deref(), Some("."));
    }

    #[test]
    fn a_directory_with_nothing_deployable_is_refused_with_a_reason() {
        let dir = project(&[("notes.txt", "hello")]);
        match detect(dir.path()) {
            Err(DetectError::NothingToDeploy(_)) => {}
            other => panic!("expected NothingToDeploy, got {other:?}"),
        }
        let message = DetectError::NothingToDeploy(dir.path().to_path_buf()).to_string();
        assert!(message.contains("package.json"), "{message}");
        assert!(message.contains("index.html"), "{message}");
    }

    #[test]
    fn a_missing_directory_is_refused() {
        assert!(matches!(
            detect(Path::new("/definitely/not/here")),
            Err(DetectError::NotADirectory(_))
        ));
    }

    /// A malformed manifest must not refuse a project the provider's own CLI
    /// would happily build.
    #[test]
    fn a_broken_package_json_still_leaves_a_deployable_project() {
        let dir = project(&[("package.json", "{ not json at all"), ("index.html", "<p>x</p>")]);
        let profile = detect(dir.path()).expect("still deployable");
        assert_eq!(profile.framework, Framework::Static);
    }

    #[test]
    fn the_package_manager_comes_from_the_lockfile() {
        for (lockfile, expected) in [
            ("pnpm-lock.yaml", PackageManager::Pnpm),
            ("yarn.lock", PackageManager::Yarn),
            ("bun.lockb", PackageManager::Bun),
            ("package-lock.json", PackageManager::Npm),
        ] {
            let dir = project(&[
                ("package.json", &manifest("\"scripts\": {\"build\": \"x\"}")),
                (lockfile, ""),
            ]);
            let profile = detect(dir.path()).expect("detects");
            assert_eq!(profile.package_manager, expected, "{lockfile}");
            assert!(
                profile.build_command.as_deref() == Some(expected.run_build()),
                "{lockfile} should build with {}",
                expected.run_build()
            );
        }
    }

    /// With no lockfile at all, npm is the assumption -- it is what ships with
    /// Node, so it is the one that is certainly there.
    #[test]
    fn no_lockfile_assumes_npm() {
        let dir = project(&[("package.json", &manifest("\"scripts\": {\"build\": \"x\"}"))]);
        assert_eq!(detect(dir.path()).unwrap().package_manager, PackageManager::Npm);
    }

    #[test]
    fn existing_provider_config_is_noticed() {
        let dir = project(&[
            ("package.json", &manifest("\"scripts\": {\"build\": \"x\"}")),
            ("vercel.json", "{}"),
            ("netlify.toml", "[build]"),
        ]);
        let profile = detect(dir.path()).expect("detects");
        assert!(profile.has_vercel_config);
        assert!(profile.has_netlify_config);
        assert!(profile.configured_for("vercel"));
        assert!(profile.configured_for("netlify"));
        assert!(!profile.configured_for("render"));
    }

    #[test]
    fn a_missing_build_script_is_warned_about_rather_than_refused() {
        let dir = project(&[("package.json", &manifest("\"dependencies\": {\"react\": \"18\"}"))]);
        let profile = detect(dir.path()).expect("detects");
        assert!(
            profile.warnings.iter().any(|w| w.contains("no `build` script")),
            "{:?}",
            profile.warnings
        );
    }

    /// A local `.env` is named but never read: what matters is that it exists
    /// and that its values will not travel with the deployment.
    #[test]
    fn a_local_env_file_is_mentioned_without_being_read() {
        let dir = project(&[
            ("package.json", &manifest("\"scripts\": {\"build\": \"x\"}")),
            (".env", "SECRET_KEY=super-secret-value"),
        ]);
        let profile = detect(dir.path()).expect("detects");
        let joined = profile.warnings.join(" ");
        assert!(joined.contains(".env is present"), "{joined}");
        assert!(!joined.contains("super-secret-value"), "a value leaked: {joined}");
    }

    #[test]
    fn the_name_comes_from_package_json_then_the_directory() {
        let named = project(&[("package.json", "{\"name\": \"my-site\"}"), ("index.html", "")]);
        assert_eq!(detect(named.path()).unwrap().name, "my-site");

        let unnamed = project(&[("index.html", "")]);
        let from_dir = detect(unnamed.path()).unwrap().name;
        assert!(!from_dir.is_empty());
        assert!(from_dir.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'));
    }

    #[test]
    fn names_are_folded_into_something_a_provider_will_accept() {
        assert_eq!(sanitize_name("@acme/My Site"), "my-site");
        assert_eq!(sanitize_name("My_Cool_App!"), "my-cool-app");
        assert_eq!(sanitize_name("---weird---"), "weird");
        assert_eq!(sanitize_name("already-fine"), "already-fine");
        assert!(sanitize_name(&"x".repeat(200)).len() <= 52);
    }

    /// Every variant has to answer the settings questions, or the config
    /// screen has a hole in it for whichever framework was added last.
    #[test]
    fn every_framework_has_a_label_and_settled_defaults() {
        for framework in [
            Framework::NextJs,
            Framework::Nuxt,
            Framework::Astro,
            Framework::SvelteKit,
            Framework::Remix,
            Framework::Vite,
            Framework::React,
            Framework::Node,
            Framework::Static,
            Framework::Unknown,
        ] {
            assert!(!framework.label().is_empty(), "{framework:?}");
            // Calling these must never panic; their values are asserted above
            // for the cases where a specific answer is load-bearing.
            let _ = framework.default_build_command();
            let _ = framework.default_output_dir();
            let _ = framework.output_is_provider_managed();
        }
    }
}
