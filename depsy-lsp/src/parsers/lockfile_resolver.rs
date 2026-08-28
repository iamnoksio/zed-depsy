//! Generic lockfile resolution trait + dispatch helper.
//!
//! Abstracts the per-ecosystem lockfile lookup/parse logic so that the LSP
//! backend can resolve versions through a single code path regardless of the
//! manifest format.
//!
//! The primary entry points are:
//!
//! - [`select_resolver`] — picks the right [`LockfileResolver`] for a given
//!   [`crate::file_types::FileType`].
//! - [`resolve_versions_from_lockfile`] — runs the full resolve pipeline,
//!   mutating each [`crate::parsers::Dependency`]'s `resolved_version` field in place.

use async_trait::async_trait;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::file_types::FileType;
use crate::parsers::Dependency;
use crate::parsers::lockfile_graph::LockfileGraph;
use crate::providers::inlay_hints::{is_local_dependency, normalize_version};

/// Strip a leading `v` (Go module versions, some Maven tags) so that the
/// remainder can be handed to `semver`.
fn strip_v(version: &str) -> &str {
    version
        .strip_prefix('v')
        .filter(|rest| rest.starts_with(|c: char| c.is_ascii_digit()))
        .unwrap_or(version)
}

/// Best-effort parse of a version or of the lower bound of a requirement.
///
/// Returns `None` for anything `semver` cannot make sense of
pub(crate) fn parse_loose(version: &str) -> Option<semver::Version> {
    semver::Version::parse(&normalize_version(strip_v(version.trim()))).ok()
}

/// True when the locked version sits *below* the lower bound the manifest
fn lockfile_is_stale(declared: &str, locked: &str) -> bool {
    let spec = declared.trim();
    if spec.is_empty()
        || spec.starts_with('<')
        || spec.starts_with("!=")
        || spec.starts_with('*')
        || is_local_dependency(spec)
    {
        return false;
    }
    let exclusive = spec.starts_with('>') && !spec.starts_with(">=");
    match (parse_loose(spec), parse_loose(locked)) {
        (Some(floor), Some(locked)) if exclusive => locked <= floor,
        (Some(floor), Some(locked)) => locked < floor,
        _ => false,
    }
}

/// Choose which lock entry to use when a lockfile pins several versions of the
/// same package
pub(crate) fn select_locked_version<'a>(
    declared: &str,
    candidates: impl IntoIterator<Item = &'a str>,
) -> Option<&'a str> {
    let candidates: Vec<&'a str> = candidates.into_iter().collect();
    if let Ok(req) = semver::VersionReq::parse(strip_v(declared.trim())) {
        let parsed: Vec<(semver::Version, &'a str)> = candidates
            .iter()
            .filter_map(|c| parse_loose(c).map(|v| (v, *c)))
            .collect();
        if !parsed.is_empty() {
            return parsed
                .into_iter()
                .filter(|(v, _)| req.matches(v))
                .max_by(|a, b| a.0.cmp(&b.0))
                .map(|(_, c)| c);
        }
    }
    candidates.first().copied()
}

/// Per-ecosystem lockfile resolver.
///
/// Each ecosystem that supports lockfiles provides a concrete implementation.
/// The three methods work together: [`find_lockfile`](LockfileResolver::find_lockfile)
/// locates the file on disk, [`parse_graph`](LockfileResolver::parse_graph) converts
/// its text into a [`LockfileGraph`], and [`resolve_version`](LockfileResolver::resolve_version)
/// maps a manifest dependency to its exact locked version.
///
/// Name normalization (lowercase, separator collapsing, etc.) is exposed through
/// [`normalize_name`](LockfileResolver::normalize_name) so that both sides of the
/// comparison use the same canonical form.
#[async_trait]
pub trait LockfileResolver: Send + Sync {
    /// Locate the lockfile relative to the manifest path.
    ///
    /// Returns `None` when no lockfile exists for this ecosystem (or when the
    /// filesystem probe fails).
    async fn find_lockfile(&self, manifest_path: &Path) -> Option<PathBuf>;

    /// Parse lockfile contents into a [`LockfileGraph`].
    ///
    /// On parse failure, returns an empty graph (silent — matches existing parser behavior).
    fn parse_graph(&self, lock_content: &str) -> LockfileGraph;

    /// Normalize a package name for version-map lookup.
    ///
    /// Default implementation is the identity function.
    /// Override for ecosystems with case-insensitive or separator-normalized names:
    /// - Python: PEP 503 (`_`/`.`/`-` → `-`, lowercase)
    /// - Ruby/NuGet/Composer: lowercase
    fn normalize_name(&self, name: &str) -> String {
        name.to_string()
    }

    /// Resolve the locked version for a single dependency from a parsed graph.
    ///
    /// The default implementation looks the package up by normalized name,
    /// applying [`normalize_name`](LockfileResolver::normalize_name) to **both**
    /// `dep.name` and each [`crate::parsers::lockfile_graph::LockfilePackage`]`::name`
    /// so the comparison is consistent regardless of whether the parser pre-normalized
    /// graph entries.
    ///
    /// Override for ecosystems with multi-version semantics (e.g., Go) or
    /// root-package disambiguation (e.g., Cargo).
    fn resolve_version(&self, dep: &Dependency, graph: &LockfileGraph) -> Option<String> {
        let normalized = self.normalize_name(&dep.name);
        select_locked_version(
            &dep.version,
            graph
                .packages
                .iter()
                .filter(|p| self.normalize_name(&p.name) == normalized)
                .map(|p| p.version.as_str()),
        )
        .map(str::to_owned)
    }
}

/// Return the [`LockfileResolver`] that matches `file_type`.
///
/// For `Npm` and `Python`, the on-disk sub-format is probed eagerly at call
/// time so the resolver can cache the lockfile path and sub-format variant
/// (e.g., `package-lock.json` vs `yarn.lock`).
///
/// # Returns
///
/// `Some(resolver)` for supported manifest/lockfile pairs. Returns `None` for
/// [`FileType::Maven`] and for C# manifests that are not `.csproj` files.
pub async fn select_resolver(
    file_type: FileType,
    manifest_path: &Path,
    manifest_content: &str,
) -> Option<Box<dyn LockfileResolver>> {
    match file_type {
        FileType::Cargo => {
            let root_package = crate::parsers::cargo::cargo_root_package_name(manifest_content);
            Some(Box::new(crate::parsers::cargo_lock::CargoResolver {
                root_package,
            }))
        }
        FileType::Npm => {
            let (lock_path, sub) =
                crate::parsers::npm_lock::find_npm_lockfile(manifest_path).await?;
            Some(Box::new(crate::parsers::npm_lock::NpmResolver {
                lock_path,
                sub,
            }))
        }
        FileType::Python => {
            let preferred = crate::parsers::python_lock::detect_python_tool(manifest_content);
            let (lock_path, sub) =
                crate::parsers::python_lock::find_python_lockfile(manifest_path, preferred).await?;
            Some(Box::new(crate::parsers::python_lock::PythonResolver {
                lock_path,
                sub,
            }))
        }
        FileType::Go => Some(Box::new(crate::parsers::go_sum::GoResolver)),
        FileType::Php => Some(Box::new(crate::parsers::composer_lock::PhpResolver)),
        FileType::Dart => Some(Box::new(crate::parsers::pubspec_lock::DartResolver)),
        FileType::Csharp => {
            if manifest_path.extension().and_then(OsStr::to_str) == Some("csproj") {
                Some(Box::new(crate::parsers::packages_lock_json::CsharpResolver))
            } else {
                None
            }
        }
        FileType::Ruby => Some(Box::new(crate::parsers::gemfile_lock::RubyResolver)),
        FileType::Maven => None,
    }
}

/// Resolve locked versions for all `dependencies` using the provided resolver.
///
/// Locates the lockfile, parses it into a [`LockfileGraph`], then sets each
/// `dependency.resolved_version` to the exact version pinned in the lockfile.
/// Dependencies that are absent from the lockfile are left unchanged, and so
/// are dependencies whose lock pin is older than the version declared in the
/// manifest: a hand-edited manifest must not be
/// reported as outdated just because the lockfile has not been regenerated
///
/// # Returns
///
/// `Some(Arc<LockfileGraph>)` whenever the lockfile is located and read
/// successfully, so downstream consumers (e.g., vulnerability attribution) can
/// reuse the already-parsed graph. The wrapped graph may still be empty — most
/// resolvers swallow parse failures silently and return an empty
/// [`LockfileGraph`] rather than propagating the error.
/// Returns `None` when no lockfile is found or the lockfile cannot be read.
pub async fn resolve_versions_from_lockfile(
    dependencies: &mut [Dependency],
    resolver: Box<dyn LockfileResolver>,
    manifest_path: &Path,
) -> Option<Arc<LockfileGraph>> {
    let lock_path = resolver.find_lockfile(manifest_path).await?;
    let lock_content = match crate::parsers::lockfile_graph::read_lockfile_capped(&lock_path).await
    {
        Ok(c) => c,
        Err(e) => {
            tracing::debug!("Could not read lockfile at {}: {}", lock_path.display(), e);
            return None;
        }
    };
    let graph = resolver.parse_graph(&lock_content);
    for dep in dependencies.iter_mut() {
        if let Some(v) = resolver.resolve_version(dep, &graph) {
            if lockfile_is_stale(&dep.version, &v) {
                tracing::debug!(
                    "Ignoring stale lock pin {v} for '{}': manifest declares {}",
                    dep.name,
                    dep.version
                );
                continue;
            }
            dep.resolved_version = Some(v);
        }
    }
    tracing::debug!(
        "Resolved {} versions from {}",
        dependencies
            .iter()
            .filter(|d| d.resolved_version.is_some())
            .count(),
        lock_path.display()
    );
    Some(Arc::new(graph))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn select_resolver_returns_cargo_resolver_for_cargo_filetype() {
        let path = Path::new("/tmp/Cargo.toml");
        let manifest = r#"[package]
name = "demo"
version = "0.0.1"
"#;
        let resolver = select_resolver(FileType::Cargo, path, manifest).await;
        assert!(resolver.is_some(), "Cargo should yield a resolver");
    }

    #[tokio::test]
    async fn select_resolver_returns_none_for_maven() {
        let path = Path::new("/tmp/pom.xml");
        let result = select_resolver(FileType::Maven, path, "").await;
        assert!(result.is_none(), "Maven should not produce a resolver");
    }

    #[tokio::test]
    async fn select_resolver_restricts_csharp_lockfiles_to_csproj() {
        for (path, expected) in [
            ("/tmp/App.csproj", true),
            ("/tmp/Directory.Build.props", false),
            ("/tmp/Directory.Packages.props", false),
        ] {
            let resolver = select_resolver(FileType::Csharp, Path::new(path), "").await;
            assert_eq!(resolver.is_some(), expected, "unexpected result for {path}");
        }
    }

    struct StubResolver {
        lock_path: Option<PathBuf>,
        graph: LockfileGraph,
    }

    #[async_trait]
    impl LockfileResolver for StubResolver {
        async fn find_lockfile(&self, _manifest_path: &Path) -> Option<PathBuf> {
            self.lock_path.clone()
        }
        fn parse_graph(&self, _content: &str) -> LockfileGraph {
            LockfileGraph {
                packages: self.graph.packages.clone(),
            }
        }
    }

    fn test_dep(name: &str, version: &str) -> Dependency {
        Dependency {
            name: name.to_string(),
            version: version.to_string(),
            name_span: crate::parsers::Span {
                line: 0,
                line_start: 0,
                line_end: 0,
            },
            version_span: crate::parsers::Span {
                line: 0,
                line_start: 0,
                line_end: 0,
            },
            dev: false,
            optional: false,
            registry: None,
            resolved_version: None,
            has_additional_version_constraints: false,
        }
    }

    fn test_pkg(name: &str, version: &str) -> crate::parsers::lockfile_graph::LockfilePackage {
        crate::parsers::lockfile_graph::LockfilePackage {
            name: name.to_string(),
            version: version.to_string(),
            dependencies: Vec::new(),
            is_root: false,
        }
    }

    #[tokio::test]
    async fn helper_returns_none_when_resolver_finds_no_lockfile() {
        let resolver: Box<dyn LockfileResolver> = Box::new(StubResolver {
            lock_path: None,
            graph: LockfileGraph { packages: vec![] },
        });
        let mut deps = vec![test_dep("serde", "1.0.0")];
        let result =
            resolve_versions_from_lockfile(&mut deps, resolver, Path::new("/tmp/Cargo.toml")).await;
        assert!(result.is_none());
        assert_eq!(deps[0].resolved_version, None);
    }

    /// Default `resolve_version` must normalize BOTH sides so that resolvers
    /// whose `parse_graph` does not pre-normalize names still match correctly.
    #[test]
    fn default_resolve_version_normalizes_both_sides() {
        struct LowercaseResolver;
        #[async_trait]
        impl LockfileResolver for LowercaseResolver {
            async fn find_lockfile(&self, _: &Path) -> Option<PathBuf> {
                None
            }
            fn parse_graph(&self, _: &str) -> LockfileGraph {
                LockfileGraph { packages: vec![] }
            }
            fn normalize_name(&self, name: &str) -> String {
                name.to_lowercase()
            }
        }

        let resolver = LowercaseResolver;
        let graph = LockfileGraph {
            // Graph stores name un-normalized (mixed case).
            packages: vec![test_pkg("Newtonsoft.Json", "13.0.1")],
        };
        let dep = test_dep("newtonsoft.json", "13.0");
        assert_eq!(
            resolver.resolve_version(&dep, &graph),
            Some("13.0.1".to_string()),
            "normalization must apply to both dep.name AND package.name"
        );
    }

    #[tokio::test]
    async fn helper_resolves_versions_via_resolver() {
        use std::io::Write;
        let tmp = tempfile::tempdir().expect("tempdir");
        let lock_path = tmp.path().join("Cargo.lock");
        let mut file = std::fs::File::create(&lock_path).expect("create lockfile");
        writeln!(file, "# stub lockfile content").expect("write lockfile");

        let resolver: Box<dyn LockfileResolver> = Box::new(StubResolver {
            lock_path: Some(lock_path.clone()),
            graph: LockfileGraph {
                packages: vec![test_pkg("serde", "1.0.230"), test_pkg("tokio", "1.50.0")],
            },
        });
        let mut deps = vec![
            test_dep("serde", "1.0"),
            test_dep("tokio", "1.0"),
            test_dep("absent", "0"),
        ];
        let manifest_path = tmp.path().join("Cargo.toml");
        let arc = resolve_versions_from_lockfile(&mut deps, resolver, &manifest_path)
            .await
            .expect("expected Some(graph)");
        assert_eq!(arc.packages.len(), 2);
        assert_eq!(deps[0].resolved_version, Some("1.0.230".to_string()));
        assert_eq!(deps[1].resolved_version, Some("1.50.0".to_string()));
        assert_eq!(deps[2].resolved_version, None);
    }

    #[test]
    fn stale_lock_pin_is_detected() {
        // Manifest hand-edited, lockfile not regenerated
        assert!(lockfile_is_stale("1.0.220", "1.0.210"));
        assert!(lockfile_is_stale("^1.0.220", "1.0.210"));
        assert!(lockfile_is_stale("~=4.1", "4.0.3"));
        assert!(lockfile_is_stale(">=2.0", "1.9.9"));
        assert!(lockfile_is_stale("v1.22.0", "v1.21.5"));
        assert!(lockfile_is_stale(">1.2.0", "1.2.0"));
    }

    #[test]
    fn fresh_lock_pin_is_kept() {
        // The normal case: the lock is more precise than the declared range
        assert!(!lockfile_is_stale("^1.0", "1.0.210"));
        assert!(!lockfile_is_stale("1.0.210", "1.0.210"));
        assert!(!lockfile_is_stale("~1.2", "1.2.9"));
        assert!(!lockfile_is_stale(">=1.2.0", "1.2.0"));
        assert!(!lockfile_is_stale(">1.2.0", "1.2.1"));
        // No lower bound declared, or nothing parseable: never reject
        assert!(!lockfile_is_stale("<2.0", "1.9.0"));
        assert!(!lockfile_is_stale("!=1.2.0", "1.1.0"));
        assert!(!lockfile_is_stale("*", "1.0.0"));
        assert!(!lockfile_is_stale("", "1.0.0"));
        assert!(!lockfile_is_stale("workspace:*", "1.0.0"));
        assert!(!lockfile_is_stale("file:../local", "1.0.0"));
        assert!(!lockfile_is_stale("^4.0.0a7", "4.0.0a6"));
    }

    #[test]
    fn select_locked_version_never_returns_an_incompatible_entry() {
        // Requirement parses and candidates parse, but nothing satisfies it:
        // leaving it unresolved is better than pinning a forbidden version
        assert_eq!(select_locked_version("~1.1", ["1.2.0", "1.3.0"]), None);
        // Same rule with a single candidate: the sole-pin path must not be a
        // hole in the check
        assert_eq!(select_locked_version("~1.1", ["1.2.0"]), None);
        assert_eq!(select_locked_version("~1.1", ["1.1.9"]), Some("1.1.9"));
        // Tolerant fallbacks still apply with one candidate
        assert_eq!(select_locked_version("~=1.4", ["1.6.0"]), Some("1.6.0"));
        assert_eq!(
            select_locked_version(">=1.0", ["2024.1.1.5"]),
            Some("2024.1.1.5")
        );
    }

    #[tokio::test]
    async fn manual_bump_is_not_overridden_by_stale_lockfile() {
        use std::io::Write;
        let tmp = tempfile::tempdir().expect("tempdir");
        let lock_path = tmp.path().join("Cargo.lock");
        let mut file = std::fs::File::create(&lock_path).expect("create lockfile");
        writeln!(file, "# stub lockfile content").expect("write lockfile");

        let resolver: Box<dyn LockfileResolver> = Box::new(StubResolver {
            lock_path: Some(lock_path),
            graph: LockfileGraph {
                packages: vec![test_pkg("serde", "1.0.210"), test_pkg("tokio", "1.44.0")],
            },
        });
        // serde was bumped by hand past the lock pin; tokio was not
        let mut deps = vec![test_dep("serde", "1.0.220"), test_dep("tokio", "1.44")];
        resolve_versions_from_lockfile(&mut deps, resolver, &tmp.path().join("Cargo.toml"))
            .await
            .expect("expected Some(graph)");
        assert_eq!(
            deps[0].resolved_version, None,
            "stale pin must not shadow the manifest version"
        );
        assert_eq!(deps[0].effective_version(), "1.0.220");
        assert_eq!(deps[1].resolved_version, Some("1.44.0".to_string()));
    }

    #[test]
    fn multi_version_lockfile_picks_the_matching_entry() {
        struct PlainResolver;
        #[async_trait]
        impl LockfileResolver for PlainResolver {
            async fn find_lockfile(&self, _: &Path) -> Option<PathBuf> {
                None
            }
            fn parse_graph(&self, _: &str) -> LockfileGraph {
                LockfileGraph { packages: vec![] }
            }
        }

        let graph = LockfileGraph {
            packages: vec![
                test_pkg("windows-sys", "0.52.0"),
                test_pkg("windows-sys", "0.59.0"),
            ],
        };
        assert_eq!(
            PlainResolver.resolve_version(&test_dep("windows-sys", "0.59"), &graph),
            Some("0.59.0".to_string()),
            "must not first-wins onto the 0.52 entry"
        );
        assert_eq!(
            PlainResolver.resolve_version(&test_dep("windows-sys", "0.52"), &graph),
            Some("0.52.0".to_string()),
            "must not jump to a version the manifest does not allow"
        );
    }

    #[test]
    fn select_locked_version_falls_back_to_first_wins() {
        // Declared spec is not a semver requirement: keep the previous behaviour
        assert_eq!(
            select_locked_version("~=1.4", ["1.4.0", "1.6.0"]),
            Some("1.4.0")
        );
        assert_eq!(
            select_locked_version("^1.0", std::iter::empty::<&str>()),
            None
        );
        assert_eq!(
            select_locked_version(">=1.0", ["2024.1.1.5", "2024.2.0.0"]),
            Some("2024.1.1.5")
        );
        assert_eq!(
            select_locked_version("^1.0", std::iter::empty::<&str>()),
            None
        );
    }
}
