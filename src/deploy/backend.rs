//! Working out whether a directory holds a server, and what kind.
//!
//! The sibling of `detect.rs`, and deliberately not part of it. `Framework`
//! there answers "what builds this site", and its whole vocabulary --
//! `build_command`, `output_dir`, `is_static_hostable` -- is about producing
//! files to serve. A backend produces no files. It has a runtime, an
//! entrypoint and a set of dependencies to install, and folding those into an
//! enum whose other variants mean "static site generator" would make every
//! caller ask which kind of thing it was holding before it could use any
//! field.
//!
//! Same discipline as `detect.rs` otherwise: a pure read of the filesystem, no
//! network, no subprocess, most specific rule first. And the same stance on
//! being wrong -- everything here is a default the user can override, so the
//! cost of a bad guess is an edit rather than a broken deployment.
//!
//! What this is *for*: boxcode-hosted backends run on Lambda, which does not
//! start a server. The user writes an ordinary Express or FastAPI app and an
//! adapter turns the platform's event into the request that app expects. So
//! detection has to find two things -- which adapter, and which file exports
//! the app for it to wrap.

use super::detect::DetectError;
use serde_json::Value;
use std::path::{Path, PathBuf};

/// Which language runtime the backend needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Runtime {
    Node,
    Python,
}

impl Runtime {
    pub fn label(self) -> &'static str {
        match self {
            Runtime::Node => "Node.js",
            Runtime::Python => "Python",
        }
    }
}

/// Which server framework, so the right adapter can be used.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendFramework {
    Express,
    Fastify,
    Koa,
    Nest,
    FastApi,
    Flask,
    Django,
    /// A server with no framework this build recognises. Still deployable --
    /// the adapter for a plain handler is the same one every framework gets --
    /// but nothing can be assumed about how the app is exported.
    Plain,
}

impl BackendFramework {
    pub fn label(self) -> &'static str {
        match self {
            BackendFramework::Express => "Express",
            BackendFramework::Fastify => "Fastify",
            BackendFramework::Koa => "Koa",
            BackendFramework::Nest => "NestJS",
            BackendFramework::FastApi => "FastAPI",
            BackendFramework::Flask => "Flask",
            BackendFramework::Django => "Django",
            BackendFramework::Plain => "a plain server",
        }
    }

    pub fn runtime(self) -> Runtime {
        match self {
            BackendFramework::FastApi | BackendFramework::Flask | BackendFramework::Django => {
                Runtime::Python
            }
            _ => Runtime::Node,
        }
    }

    /// Whether an ASGI/WSGI or Node adapter is known to work with it. `Plain`
    /// is the only one that has to be taken on trust.
    pub fn is_recognised(self) -> bool {
        self != BackendFramework::Plain
    }
}

/// What a directory holding a server looks like.
#[derive(Clone, Debug, PartialEq)]
pub struct BackendProfile {
    pub root: PathBuf,
    pub framework: BackendFramework,
    pub runtime: Runtime,
    /// The file that defines the app, relative to `root`. `None` when nothing
    /// obvious was found -- deployable only once the user says which file.
    pub entrypoint: Option<String>,
    /// Why this was identified as it was, shown so a wrong guess is obvious.
    pub markers: Vec<String>,
    pub warnings: Vec<String>,
}

/// Files that make a directory Python regardless of what is in them.
const PYTHON_MANIFESTS: &[&str] = &["requirements.txt", "pyproject.toml", "Pipfile"];

/// Where a Node app usually lives, most conventional first. Only consulted
/// when `package.json` has no `main`.
const NODE_ENTRYPOINTS: &[&str] = &[
    "index.js", "index.mjs", "server.js", "app.js",
    "src/index.js", "src/index.mjs", "src/server.js", "src/app.js",
    "index.ts", "src/index.ts", "server.ts", "src/server.ts",
];

/// Same for Python. `manage.py` is deliberately absent: it is Django's CLI,
/// not its app, and wrapping it would produce something that runs migrations
/// rather than serves requests.
const PYTHON_ENTRYPOINTS: &[&str] = &[
    "main.py", "app.py", "application.py", "asgi.py", "wsgi.py",
    "src/main.py", "src/app.py", "app/main.py", "api/main.py",
];

/// Packages that compile against the host when installed.
///
/// Named individually rather than detected, because the thing that matters is
/// not knowing they are native -- it is being able to say *which one* will
/// break. A wheel built on macOS does not load on Amazon Linux, so a backend
/// packaged locally with any of these produces a deployment that installs
/// cleanly and then fails at import time, which is the least debuggable
/// failure available.
const NATIVE_NODE: &[&str] = &[
    "bcrypt", "sharp", "canvas", "sqlite3", "better-sqlite3", "node-gyp",
    "grpc", "@grpc/grpc-js", "node-sass", "sass-embedded", "argon2",
    "@tensorflow/tfjs-node", "puppeteer", "playwright",
];
const NATIVE_PYTHON: &[&str] = &[
    "psycopg2", "pillow", "numpy", "scipy", "pandas", "lxml", "cryptography",
    "grpcio", "pyarrow", "mysqlclient", "bcrypt", "cffi", "matplotlib",
];

/// Read a directory as a backend.
///
/// `NothingToDeploy` when there is neither a `package.json` nor any Python
/// manifest: without one there is nothing to install and no way to tell a
/// server from a folder of scripts.
pub fn detect_backend(root: &Path) -> Result<BackendProfile, DetectError> {
    if !root.is_dir() {
        return Err(DetectError::NotADirectory(root.to_path_buf()));
    }

    let manifest = read_json(root, "package.json");
    let python_manifest = PYTHON_MANIFESTS.iter().find(|f| root.join(f).is_file());

    if manifest.is_none() && python_manifest.is_none() {
        return Err(DetectError::NothingToDeploy(root.to_path_buf()));
    }

    let mut markers = Vec::new();
    let mut warnings = Vec::new();

    // Python first when a Python manifest is present. A repository can hold
    // both -- a Python API with a package.json for its tooling is ordinary --
    // and in that case the manifest that names a *server* framework should
    // win, which the framework rules below decide rather than this.
    let node_deps = node_dependencies(manifest.as_ref());
    let python_deps = python_dependencies(root, python_manifest.copied());

    let framework = pick_framework(root, &node_deps, &python_deps, &mut markers);
    let runtime = framework.runtime();

    if let Some(file) = python_manifest {
        if runtime == Runtime::Python {
            markers.push((*file).to_string());
        }
    }

    let entrypoint = find_entrypoint(root, runtime, manifest.as_ref());
    if entrypoint.is_none() {
        warnings.push(format!(
            "No obvious entrypoint. Looked for {}. Say which file defines the app.",
            match runtime {
                Runtime::Node => NODE_ENTRYPOINTS[..4].join(", "),
                Runtime::Python => PYTHON_ENTRYPOINTS[..4].join(", "),
            }
        ));
    }

    if !framework.is_recognised() {
        warnings.push(
            "No server framework recognised, so the app has to export a handler itself. \
             Express, Fastify, Koa, NestJS, FastAPI, Flask and Django are wrapped automatically."
                .to_string(),
        );
    }

    let native = match runtime {
        Runtime::Node => named_natives(&node_deps, NATIVE_NODE),
        Runtime::Python => named_natives(&python_deps, NATIVE_PYTHON),
    };
    if !native.is_empty() {
        warnings.push(format!(
            "{} compile against the machine that installs them. Built here, they will not load \
             on the deployment host — the app will install cleanly and then fail at import.",
            native.join(", ")
        ));
    }

    // Said every time, not as a warning about this project but as a fact about
    // where it is going. It is the first thing a working backend runs into,
    // and finding out at runtime costs an hour.
    warnings.push(
        "A hosted backend has no outbound internet access. Calls to third-party APIs (Stripe, \
         OpenAI, an SMTP host) will time out; the project database is reachable."
            .to_string(),
    );

    Ok(BackendProfile {
        root: root.to_path_buf(),
        framework,
        runtime,
        entrypoint,
        markers,
        warnings,
    })
}

/// Most specific first, and a *server* framework always outranks the presence
/// of a manifest. A FastAPI project with a package.json for its linting is a
/// Python backend; a package.json naming Express next to a stray
/// requirements.txt is a Node one.
fn pick_framework(
    root: &Path,
    node_deps: &[String],
    python_deps: &[String],
    markers: &mut Vec<String>,
) -> BackendFramework {
    let node_rules: &[(&str, BackendFramework)] = &[
        ("@nestjs/core", BackendFramework::Nest),
        ("express", BackendFramework::Express),
        ("fastify", BackendFramework::Fastify),
        ("koa", BackendFramework::Koa),
    ];
    let python_rules: &[(&str, BackendFramework)] = &[
        ("fastapi", BackendFramework::FastApi),
        ("django", BackendFramework::Django),
        ("flask", BackendFramework::Flask),
    ];

    for (dep, framework) in python_rules {
        if python_deps.iter().any(|d| d == dep) {
            markers.push(format!("python dependency → {dep}"));
            return *framework;
        }
    }
    for (dep, framework) in node_rules {
        if node_deps.iter().any(|d| d == dep) {
            markers.push(format!("package.json → {dep}"));
            return *framework;
        }
    }

    // Django without a declared dependency is still recognisable by the file
    // it always ships, and a project vendored into the repo has no manifest
    // entry to find.
    if root.join("manage.py").is_file() {
        markers.push("manage.py".to_string());
        return BackendFramework::Django;
    }

    if !python_deps.is_empty() {
        markers.push("python dependencies".to_string());
    } else if !node_deps.is_empty() {
        markers.push("package.json".to_string());
    }
    BackendFramework::Plain
}

fn find_entrypoint(root: &Path, runtime: Runtime, manifest: Option<&Value>) -> Option<String> {
    if runtime == Runtime::Node {
        // The project saying so beats any convention.
        if let Some(main) = manifest
            .and_then(|m| m.get("main"))
            .and_then(Value::as_str)
            .filter(|m| !m.is_empty())
        {
            if root.join(main).is_file() {
                return Some(main.to_string());
            }
        }
    }
    let candidates = match runtime {
        Runtime::Node => NODE_ENTRYPOINTS,
        Runtime::Python => PYTHON_ENTRYPOINTS,
    };
    candidates
        .iter()
        .find(|c| root.join(c).is_file())
        .map(|c| (*c).to_string())
}

fn read_json(root: &Path, name: &str) -> Option<Value> {
    let text = std::fs::read_to_string(root.join(name)).ok()?;
    serde_json::from_str(&text).ok()
}

fn node_dependencies(manifest: Option<&Value>) -> Vec<String> {
    let mut out = Vec::new();
    let Some(manifest) = manifest else { return out };
    for table in ["dependencies", "devDependencies", "peerDependencies"] {
        if let Some(Value::Object(map)) = manifest.get(table) {
            out.extend(map.keys().cloned());
        }
    }
    out
}

/// Dependency *names* from whichever Python manifest is present.
///
/// Deliberately crude: the leading package name up to the first version
/// specifier or comment, lowercased. A real requirements parser would have to
/// understand markers, extras, editable installs and `-r` includes, and none
/// of that changes the answer to "is fastapi in here".
fn python_dependencies(root: &Path, manifest: Option<&str>) -> Vec<String> {
    let Some(name) = manifest else { return Vec::new() };
    let Ok(text) = std::fs::read_to_string(root.join(name)) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('-') {
            continue;
        }
        // Works for requirements.txt lines, and for the `name = "x"` /
        // `"fastapi>=0.1"` shapes that pyproject.toml and Pipfile put
        // dependencies in -- close enough to answer a membership question.
        let cleaned = line.trim_start_matches(['"', '\'']);
        let end = cleaned
            .find(|c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '_' && c != '.')
            .unwrap_or(cleaned.len());
        let package = cleaned[..end].trim().to_ascii_lowercase();
        if !package.is_empty() {
            out.push(package);
        }
    }
    out
}

fn named_natives(deps: &[String], known: &[&str]) -> Vec<String> {
    let mut hits: Vec<String> = known
        .iter()
        .filter(|n| deps.iter().any(|d| d == *n))
        .map(|n| (*n).to_string())
        .collect();
    hits.sort();
    hits.dedup();
    hits
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a directory the way a real project looks, so the tests exercise
    /// the filesystem reads rather than a mocked view of them.
    fn project(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        for (name, contents) in files {
            let path = dir.path().join(name);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("mkdir");
            }
            std::fs::write(path, contents).expect("write");
        }
        dir
    }

    fn pkg(deps: &str) -> String {
        format!(r#"{{"name":"api","dependencies":{{{deps}}}}}"#)
    }

    // ---- what each framework looks like -----------------------------------

    #[test]
    fn an_express_app_is_node_and_recognised() {
        let dir = project(&[
            ("package.json", &pkg(r#""express":"^4.19.2""#)),
            ("index.js", "const app = require('express')()\n"),
        ]);
        let p = detect_backend(dir.path()).expect("detected");
        assert_eq!(p.framework, BackendFramework::Express);
        assert_eq!(p.runtime, Runtime::Node);
        assert_eq!(p.entrypoint.as_deref(), Some("index.js"));
        assert!(p.markers.iter().any(|m| m.contains("express")), "{:?}", p.markers);
    }

    #[test]
    fn a_fastapi_app_is_python() {
        let dir = project(&[
            ("requirements.txt", "fastapi==0.111.0\nuvicorn[standard]>=0.29\n"),
            ("main.py", "from fastapi import FastAPI\napp = FastAPI()\n"),
        ]);
        let p = detect_backend(dir.path()).expect("detected");
        assert_eq!(p.framework, BackendFramework::FastApi);
        assert_eq!(p.runtime, Runtime::Python);
        assert_eq!(p.entrypoint.as_deref(), Some("main.py"));
    }

    #[test]
    fn flask_fastify_koa_and_nest_are_each_recognised() {
        for (files, expected) in [
            (vec![("package.json", pkg(r#""fastify":"^4""#))], BackendFramework::Fastify),
            (vec![("package.json", pkg(r#""koa":"^2""#))], BackendFramework::Koa),
            (vec![("package.json", pkg(r#""@nestjs/core":"^10""#))], BackendFramework::Nest),
            (vec![("requirements.txt", "flask==3.0.0\n".to_string())], BackendFramework::Flask),
        ] {
            let owned: Vec<(&str, &str)> = files.iter().map(|(n, c)| (*n, c.as_str())).collect();
            let dir = project(&owned);
            let p = detect_backend(dir.path()).expect("detected");
            assert_eq!(p.framework, expected, "for {owned:?}");
        }
    }

    /// NestJS is built on Express and declares both. The more specific rule
    /// has to win or every Nest project files as Express.
    #[test]
    fn nest_outranks_the_express_it_is_built_on() {
        let dir = project(&[("package.json", &pkg(r#""@nestjs/core":"^10","express":"^4""#))]);
        assert_eq!(
            detect_backend(dir.path()).unwrap().framework,
            BackendFramework::Nest
        );
    }

    /// A Python API with a package.json for its tooling is still Python. The
    /// framework decides the runtime, not which manifest happens to exist.
    #[test]
    fn a_server_framework_outranks_a_stray_manifest() {
        let dir = project(&[
            ("requirements.txt", "fastapi\n"),
            ("package.json", r#"{"name":"tooling","devDependencies":{"prettier":"^3"}}"#),
            ("main.py", "app = 1\n"),
        ]);
        let p = detect_backend(dir.path()).expect("detected");
        assert_eq!(p.framework, BackendFramework::FastApi);
        assert_eq!(p.runtime, Runtime::Python);
    }

    /// Django is recognisable by the file it always ships, even vendored with
    /// no manifest entry.
    #[test]
    fn django_is_found_by_manage_py_without_a_declared_dependency() {
        let dir = project(&[("requirements.txt", "gunicorn\n"), ("manage.py", "#!/usr/bin/env python\n")]);
        let p = detect_backend(dir.path()).expect("detected");
        assert_eq!(p.framework, BackendFramework::Django);
        assert!(p.markers.iter().any(|m| m == "manage.py"), "{:?}", p.markers);
    }

    // ---- entrypoints -------------------------------------------------------

    #[test]
    fn package_main_beats_convention() {
        let dir = project(&[
            ("package.json", r#"{"main":"src/server.js","dependencies":{"express":"^4"}}"#),
            ("index.js", "// a decoy the convention would have picked\n"),
            ("src/server.js", "module.exports = app\n"),
        ]);
        assert_eq!(
            detect_backend(dir.path()).unwrap().entrypoint.as_deref(),
            Some("src/server.js")
        );
    }

    #[test]
    fn a_main_that_does_not_exist_falls_back_to_convention() {
        let dir = project(&[
            ("package.json", r#"{"main":"dist/bundle.js","dependencies":{"express":"^4"}}"#),
            ("server.js", "module.exports = app\n"),
        ]);
        assert_eq!(
            detect_backend(dir.path()).unwrap().entrypoint.as_deref(),
            Some("server.js")
        );
    }

    #[test]
    fn no_entrypoint_is_a_warning_not_a_failure() {
        let dir = project(&[("package.json", &pkg(r#""express":"^4""#))]);
        let p = detect_backend(dir.path()).expect("still detected");
        assert!(p.entrypoint.is_none());
        assert!(
            p.warnings.iter().any(|w| w.contains("No obvious entrypoint")),
            "{:?}",
            p.warnings
        );
    }

    /// manage.py is Django's CLI, not its app. Wrapping it would produce
    /// something that runs migrations rather than serving requests.
    #[test]
    fn manage_py_is_never_chosen_as_the_entrypoint() {
        let dir = project(&[("requirements.txt", "django\n"), ("manage.py", "x\n")]);
        let p = detect_backend(dir.path()).expect("detected");
        assert_ne!(p.entrypoint.as_deref(), Some("manage.py"));
    }

    // ---- the warnings that save an hour ------------------------------------

    #[test]
    fn native_modules_are_named_individually() {
        let dir = project(&[("package.json", &pkg(r#""express":"^4","bcrypt":"^5","sharp":"^0.33""#))]);
        let p = detect_backend(dir.path()).expect("detected");
        let w = p.warnings.iter().find(|w| w.contains("compile against")).expect("warned");
        assert!(w.contains("bcrypt"), "{w}");
        assert!(w.contains("sharp"), "{w}");
    }

    #[test]
    fn python_natives_are_named_too() {
        let dir = project(&[("requirements.txt", "fastapi\npsycopg2==2.9.9\npillow>=10\n")]);
        let p = detect_backend(dir.path()).expect("detected");
        let w = p.warnings.iter().find(|w| w.contains("compile against")).expect("warned");
        assert!(w.contains("psycopg2"), "{w}");
        assert!(w.contains("pillow"), "{w}");
    }

    #[test]
    fn a_clean_project_gets_no_native_warning() {
        let dir = project(&[("package.json", &pkg(r#""express":"^4""#)), ("index.js", "x\n")]);
        let p = detect_backend(dir.path()).expect("detected");
        assert!(!p.warnings.iter().any(|w| w.contains("compile against")), "{:?}", p.warnings);
    }

    /// Said for every backend, because it is the first thing a working one
    /// runs into and discovering it at runtime costs an hour.
    #[test]
    fn every_backend_is_told_it_has_no_internet() {
        for files in [
            vec![("package.json", pkg(r#""express":"^4""#))],
            vec![("requirements.txt", "fastapi\n".to_string())],
            vec![("package.json", r#"{"name":"x"}"#.to_string())],
        ] {
            let owned: Vec<(&str, &str)> = files.iter().map(|(n, c)| (*n, c.as_str())).collect();
            let dir = project(&owned);
            let p = detect_backend(dir.path()).expect("detected");
            assert!(
                p.warnings.iter().any(|w| w.contains("no outbound internet")),
                "{:?}",
                p.warnings
            );
        }
    }

    // ---- the edges ---------------------------------------------------------

    #[test]
    fn a_directory_with_no_manifest_is_refused() {
        let dir = project(&[("README.md", "# just docs\n")]);
        assert!(matches!(
            detect_backend(dir.path()),
            Err(DetectError::NothingToDeploy(_))
        ));
    }

    #[test]
    fn a_file_is_not_a_project() {
        let dir = project(&[("thing.js", "x\n")]);
        assert!(matches!(
            detect_backend(&dir.path().join("thing.js")),
            Err(DetectError::NotADirectory(_))
        ));
    }

    #[test]
    fn an_unrecognised_server_is_still_deployable_and_says_so() {
        let dir = project(&[("package.json", r#"{"name":"x","main":"index.js"}"#), ("index.js", "x\n")]);
        let p = detect_backend(dir.path()).expect("detected");
        assert_eq!(p.framework, BackendFramework::Plain);
        assert!(!p.framework.is_recognised());
        assert!(
            p.warnings.iter().any(|w| w.contains("No server framework recognised")),
            "{:?}",
            p.warnings
        );
    }

    /// A frontend is not a backend. Detecting one as such would package a
    /// React app as a server and deploy something that cannot start.
    #[test]
    fn a_plain_react_frontend_is_not_recognised_as_a_server() {
        let dir = project(&[
            ("package.json", &pkg(r#""react":"^18","react-dom":"^18","vite":"^5""#)),
            ("index.html", "<html></html>"),
        ]);
        let p = detect_backend(dir.path()).expect("has a manifest, so it parses");
        assert_eq!(
            p.framework,
            BackendFramework::Plain,
            "no server framework should be claimed for a frontend"
        );
    }

    #[test]
    fn requirements_parsing_survives_the_shapes_it_meets() {
        let dir = project(&[(
            "requirements.txt",
            "# a comment\n\n-r other.txt\nFastAPI==0.111.0\n  uvicorn[standard]>=0.29  \npsycopg2-binary\n",
        )]);
        let deps = python_dependencies(dir.path(), Some("requirements.txt"));
        assert!(deps.contains(&"fastapi".to_string()), "{deps:?}");
        assert!(deps.contains(&"uvicorn".to_string()), "{deps:?}");
        assert!(deps.contains(&"psycopg2-binary".to_string()), "{deps:?}");
        assert!(!deps.iter().any(|d| d.starts_with('#')), "{deps:?}");
    }
}
