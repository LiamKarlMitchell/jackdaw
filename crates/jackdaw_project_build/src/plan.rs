//! The per-edge extern redirect plan and the lock alignment that
//! precedes it.
//!
//! A project builds against the shipped SDK by having the rustc
//! wrapper rewrite `--extern` flags to the SDK's exact artifacts. The
//! decisions are per dependency edge: an edge redirects only when the
//! project resolves that dependency at the byte-identical version the
//! SDK holds. Name-keyed redirection cannot work (the SDK closure
//! itself holds two hashbrown versions; user graphs hold private newer
//! copies of closure crates), so the plan is a `consumer:alias=artifact`
//! line per edge, consumed by `jackdaw-rustc-wrapper`.
//!
//! Before planning, the project's lockfile is aligned: closure crates
//! resolved at a semver-compatible but different version get pinned to
//! the SDK's exact version. The lockfile in question is the generated
//! shim crate's, never the user's.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::sdk_paths::SdkPaths;

#[derive(Debug)]
pub enum PlanError {
    Io(std::io::Error),
    Cargo(String),
    Parse(String),
}

impl std::fmt::Display for PlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "io error: {e}"),
            Self::Cargo(msg) => write!(f, "cargo failed: {msg}"),
            Self::Parse(msg) => write!(f, "could not parse: {msg}"),
        }
    }
}

impl std::error::Error for PlanError {}

impl From<std::io::Error> for PlanError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

/// One SDK crate as it was actually compiled.
///
/// The feature set matters as much as the path. Cargo unifies features
/// per build, so the SDK's unification and a project's are two
/// different answers for the same `(name, version)`, and redirecting an
/// edge to an artifact built with fewer features than the consumer
/// needs fails on symbols that simply are not in it.
#[derive(Debug, Clone)]
struct SdkArtifact {
    features: BTreeSet<String>,
    path: String,
}

/// The SDK's runtime closure with the exact artifact each crate
/// compiled to: `(name, version) -> artifact`. Loaded from the
/// shipped `manifest.txt` in installed layouts, generated from the
/// workspace build in dev.
pub struct SdkManifest {
    artifacts: BTreeMap<(String, String), SdkArtifact>,
}

impl SdkManifest {
    /// Load `name version features artifact` lines, where `features` is
    /// comma-separated (`-` when none).
    ///
    /// Manifests written before features were recorded have no such
    /// field, and a shipped SDK carries whichever form its jackdaw
    /// wrote, so the older `name version artifact` shape is still
    /// accepted: an artifact path always contains a separator or a
    /// library extension, and a feature list never does. Such an entry
    /// loads with an unknown feature set, which
    /// [`covers_features`](Self::covers_features) then treats as
    /// "cannot prove it covers anything".
    pub fn load(path: &Path) -> Result<Self, PlanError> {
        let contents = std::fs::read_to_string(path)?;
        let mut artifacts = BTreeMap::new();
        for line in contents.lines() {
            let mut parts = line.splitn(4, ' ');
            let (Some(name), Some(version), Some(third), rest) =
                (parts.next(), parts.next(), parts.next(), parts.next())
            else {
                return Err(PlanError::Parse(format!("bad manifest line: {line}")));
            };
            let (features, path) = match rest {
                Some(path) if !looks_like_path(third) => (parse_features(third), path),
                // Legacy three-field line: the third field is the path,
                // and `splitn(4)` may have split it at a space inside it.
                _ => {
                    let path_start = line
                        .match_indices(' ')
                        .nth(1)
                        .map(|(i, _)| i + 1)
                        .unwrap_or(line.len());
                    (BTreeSet::new(), &line[path_start..])
                }
            };
            artifacts.insert(
                (name.to_string(), version.to_string()),
                SdkArtifact {
                    features,
                    path: path.to_string(),
                },
            );
        }
        Ok(Self { artifacts })
    }

    /// Whether the SDK's build of `name@version` was compiled with every
    /// feature `wanted` needs, so an edge can safely redirect to it.
    ///
    /// Conservative by construction: an entry whose features are unknown
    /// (a legacy manifest) covers nothing but the empty request, so a
    /// stale manifest degrades to fewer redirects rather than to
    /// artifacts that are missing the symbols the consumer expects.
    fn covers_features(&self, name: &str, version: &str, wanted: &BTreeSet<String>) -> bool {
        self.artifacts
            .get(&(name.to_string(), version.to_string()))
            .is_some_and(|artifact| wanted.is_subset(&artifact.features))
    }

    /// The features a consumer needs that the SDK's build of
    /// `name@version` does not have, comma-separated. Empty when the SDK
    /// covers them (or has no entry to compare).
    fn feature_shortfall(&self, name: &str, version: &str, wanted: &BTreeSet<String>) -> String {
        self.artifacts
            .get(&(name.to_string(), version.to_string()))
            .map(|artifact| {
                wanted
                    .difference(&artifact.features)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .unwrap_or_default()
    }

    /// Generate the manifest from a dev workspace: the SDK's runtime
    /// closure (`cargo tree -e normal,no-proc-macro`, which keeps
    /// proc-macro crates and their host-side support libraries out)
    /// joined with the workspace build's artifact list. Requires the
    /// SDK to have been built with `--features dylib --target <triple>`;
    /// the triple-dir filter is what separates target-side artifacts
    /// from host-side units of the same crate. The result is written to
    /// `sdk.manifest` so later opens skip the cargo runs.
    pub fn generate_dev(workspace_root: &Path, sdk: &SdkPaths) -> Result<Self, PlanError> {
        // Enumerate artifacts from the same profile the SDK was built at, so a
        // release SDK's manifest points at release rlibs (what projects redirect
        // against) rather than debug ones from a stray earlier build.
        let mut args = vec!["-p", "jackdaw", "--features", "dylib"];
        let is_release = sdk
            .dylib
            .parent()
            .and_then(|p| p.file_name())
            .is_some_and(|name| name == "release");
        if is_release {
            args.push("--release");
        }
        Self::generate(workspace_root, sdk, &args)
    }

    /// Enumerate the SDK's runtime-closure artifacts by building
    /// `build_args` and reading cargo's JSON. The dev workspace builds the
    /// editor (`-p jackdaw --features dylib`); the bootstrap recipe, which
    /// has no editor package, builds the SDK crates directly
    /// (`-p jackdaw_sdk -p jackdaw_runner --release`). The SDK is already
    /// compiled, so this re-invocation just reports the (fresh) artifact
    /// filenames.
    ///
    /// `build_args` MUST name the same package set the SDK was built with.
    /// A narrower set resolves different features for shared dependencies
    /// (bevy) and turns this enumeration into a full second rebuild instead
    /// of a cache hit.
    pub fn generate(
        workspace_root: &Path,
        sdk: &SdkPaths,
        build_args: &[&str],
    ) -> Result<Self, PlanError> {
        let closure = sdk_runtime_closure(workspace_root)?;

        // Capture stdout (the JSON artifact stream) but let cargo's own
        // progress reach the terminal, so this step never looks hung on a
        // cold cache.
        let child = Command::new("cargo")
            .arg("build")
            .args(build_args)
            .args(["--target", &sdk.triple, "--message-format=json"])
            .current_dir(workspace_root)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .spawn()?;
        let output = child.wait_with_output()?;
        if !output.status.success() {
            return Err(PlanError::Cargo(
                "SDK build for manifest generation failed".into(),
            ));
        }

        let triple_dir = format!("/{}/", sdk.triple);
        // Cargo emits native separators, so on Windows every filename
        // reads `...\x86_64-pc-windows-msvc\release\...` and a
        // forward-slash needle matches nothing at all - the manifest
        // came out empty, the per-edge plan with it, and only the
        // `bevy` facade was ever redirected. That silently leaves the
        // project compiling its own `bevy_ecs` and `glam` against the
        // SDK's `bevy`, which surfaces much later as hundreds of
        // `X is not a Resource` / `glam::Vec3 does not implement
        // PartialReflect` errors in crates that did nothing wrong.
        let in_triple_dir = |f: &str| f.replace('\\', "/").contains(&triple_dir);
        let mut artifacts = BTreeMap::new();
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let Ok(msg) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            if msg["reason"] != "compiler-artifact" {
                continue;
            }
            let Some(name) = msg["target"]["name"].as_str() else {
                continue;
            };
            let name = name.replace('-', "_");
            let Some(version) = msg["package_id"].as_str().and_then(package_id_version) else {
                continue;
            };
            if !closure.contains(&(name.clone(), version.to_string())) {
                continue;
            }
            let kinds = msg["target"]["kind"]
                .as_array()
                .map(|k| k.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>())
                .unwrap_or_default();
            if kinds.contains(&"proc-macro") || kinds.contains(&"custom-build") {
                continue;
            }
            let Some(filenames) = msg["filenames"].as_array() else {
                continue;
            };
            // Prefer the rlib: the final dylib link needs code, and
            // rustc dedupes a same-SVH crate already linked into the
            // SDK dylib instead of embedding the rlib a second time.
            // Only accept artifacts from the triple dir.
            let artifact = filenames
                .iter()
                .filter_map(|f| f.as_str())
                .filter(|f| in_triple_dir(f))
                .find(|f| f.ends_with(".rlib"))
                .or_else(|| {
                    filenames
                        .iter()
                        .filter_map(|f| f.as_str())
                        .filter(|f| in_triple_dir(f))
                        .find(|f| f.ends_with(".rmeta"))
                });
            if let Some(artifact) = artifact {
                // Cargo reports the features it resolved for this unit;
                // recording them is what lets `write_plan` refuse an
                // edge whose consumer needs more than this build has.
                let features = msg["features"]
                    .as_array()
                    .map(|fs| {
                        fs.iter()
                            .filter_map(|f| f.as_str())
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default();
                artifacts.insert(
                    (name, version.to_string()),
                    SdkArtifact {
                        features,
                        path: artifact.to_string(),
                    },
                );
            }
        }

        // Native search paths the SDK's build scripts emitted. A
        // redirected rlib can carry a `#[link]` requirement whose
        // import library lives in a crate the PROJECT's graph does not
        // contain, so no build script of its own ever emits the path:
        // the SDK closure holds `windows-sys 0.45` -> `windows_x86_64_msvc
        // 0.42.2`, the only version shipping a plain `windows.lib`,
        // while the project resolves 0.52/0.60/0.61, whose files are
        // named `windows.0.52.0.lib` and so on. The link then fails with
        // `could not open 'windows.lib'` naming no crate at all.
        let mut link_paths: BTreeSet<String> = BTreeSet::new();
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let Ok(msg) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            if msg["reason"] != "build-script-executed" {
                continue;
            }
            for path in msg["linked_paths"].as_array().unwrap_or(&Vec::new()) {
                if let Some(path) = path.as_str() {
                    link_paths.insert(path.to_string());
                }
            }
        }
        write_link_paths(&link_paths_path(&sdk.manifest), &link_paths)?;

        let manifest = Self { artifacts };
        manifest.write(&sdk.manifest)?;
        Ok(manifest)
    }

    pub fn write(&self, path: &Path) -> Result<(), PlanError> {
        let mut file = std::fs::File::create(path)?;
        for ((name, version), artifact) in &self.artifacts {
            let features = if artifact.features.is_empty() {
                "-".to_string()
            } else {
                artifact.features.iter().cloned().collect::<Vec<_>>().join(",")
            };
            writeln!(file, "{name} {version} {features} {}", artifact.path)?;
        }
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.artifacts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.artifacts.is_empty()
    }

    /// Manifest entries whose artifact is no longer on disk, as
    /// `name version` pairs, capped at `limit`.
    ///
    /// A manifest is a cache of one build's filenames. Cargo replaces
    /// those on every rebuild and prunes what it no longer needs, so
    /// entries rot without anything noticing until rustc is handed a
    /// path that does not resolve. Checking costs one stat per entry.
    /// Only meaningful for absolute paths: a shipped manifest stores
    /// basenames that are rebased onto the install at use time.
    pub fn missing_artifacts(&self, limit: usize) -> Vec<String> {
        self.artifacts
            .iter()
            .filter(|(_, artifact)| {
                let path = Path::new(artifact.path.as_str());
                path.is_absolute() && !path.exists()
            })
            .take(limit)
            .map(|((name, version), _)| format!("{name} {version}"))
            .collect()
    }

    pub fn artifact(&self, name: &str, version: &str) -> Option<&str> {
        self.artifacts
            .get(&(name.to_string(), version.to_string()))
            .map(|artifact| artifact.path.as_str())
    }

    /// How many versions of a crate the SDK closure holds. A graph
    /// legitimately carries several majors of one crate (two `rand`s),
    /// each compiling to its own artifact, so this is what separates an
    /// expected pair of artifacts from a crate built twice over.
    pub fn version_count(&self, name: &str) -> usize {
        self.artifacts
            .keys()
            .filter(|(crate_name, _)| crate_name == name)
            .count()
    }

    /// The version of `name` the SDK holds, when it holds exactly one.
    /// `None` for a crate carried at several majors, where "the SDK's
    /// version" is not a well-defined thing to pin to.
    pub fn sole_version(&self, name: &str) -> Option<&str> {
        let mut matches = self
            .artifacts
            .keys()
            .filter(|(crate_name, _)| crate_name == name);
        let (_, version) = matches.next()?;
        matches.next().is_none().then_some(version.as_str())
    }

    /// The artifact for a crate by name, ignoring version. The SDK closure
    /// holds a single bevy and `jackdaw_api`, so this uniquely resolves the
    /// rlibs the static wrapper points the bevy facade and the `jackdaw_api`
    /// injection at.
    pub fn artifact_for(&self, name: &str) -> Option<&str> {
        self.artifacts
            .iter()
            .find(|((n, _), _)| n == name)
            .map(|(_, artifact)| artifact.path.as_str())
    }
}

/// Pin every closure crate in `build_root`'s lockfile to the exact
/// version the SDK holds, and report how many were moved.
///
/// Seeding the shim with the SDK's lockfile is not enough on its own:
/// cargo re-resolves on top of the seed, and a package whose dependency
/// edges differ drifts to a newer patch. `serde_json` did exactly that
/// (1.0.149 -> 1.0.151 within a minute of the seed). The version then no
/// longer matches the manifest, so `write_plan` writes no edge for it and
/// the project compiles its own copy - while `gltf_json`, which DOES
/// match and so does redirect, was built against the SDK's. `gltf` then
/// sees two `serde_json::Value` types and fails on code nobody wrote.
///
/// Only crates the SDK holds at exactly one version are pinned: a graph
/// legitimately carries several majors of one crate (the SDK itself has
/// four `windows-sys`), and there is no single right answer for those.
/// Every pin is best-effort - a package another requirement holds above
/// the SDK's version cannot move, and that is reported rather than
/// treated as fatal, since the redirect simply will not fire for it.
pub fn align_lock(build_root: &Path, manifest: &SdkManifest) -> Result<usize, PlanError> {
    let metadata = cargo_metadata(build_root)?;
    let empty = Vec::new();
    let mut drifted: BTreeMap<String, (String, String)> = BTreeMap::new();
    for pkg in metadata["packages"].as_array().unwrap_or(&empty) {
        let (Some(raw_name), Some(version)) = (pkg["name"].as_str(), pkg["version"].as_str())
        else {
            continue;
        };
        let normalized = raw_name.replace('-', "_");
        // Ambiguous when the SDK carries several majors: skip.
        if manifest.version_count(&normalized) != 1 {
            continue;
        }
        let Some(sdk_version) = manifest.sole_version(&normalized) else {
            continue;
        };
        if sdk_version != version {
            drifted.insert(
                raw_name.to_string(),
                (version.to_string(), sdk_version.to_string()),
            );
        }
    }

    let mut aligned = 0;
    for (name, (from, to)) in &drifted {
        // `cargo update` wants the name as cargo spells it, dashes and
        // all, and the spec pinned to the version actually in the lock
        // so an ambiguous name cannot match the wrong entry.
        let output = Command::new("cargo")
            .args([
                "update",
                "-p",
                &format!("{name}@{from}"),
                "--precise",
                to,
            ])
            .current_dir(build_root)
            .output()?;
        if output.status.success() {
            aligned += 1;
        } else {
            tracing::warn!(
                "could not pin {name} {from} -> {to} (the SDK's version): {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
    }
    Ok(aligned)
}

/// Where the SDK's build-script link paths are recorded, beside the
/// manifest they are generated with.
pub fn link_paths_path(manifest: &Path) -> PathBuf {
    manifest.with_file_name("jackdaw_sdk_link_paths.txt")
}

/// Read the recorded link paths; empty when the file is absent, which is
/// what an SDK built before these were captured looks like.
pub fn read_link_paths(manifest: &Path) -> Vec<String> {
    std::fs::read_to_string(link_paths_path(manifest))
        .map(|text| {
            text.lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn write_link_paths(path: &Path, paths: &BTreeSet<String>) -> Result<(), PlanError> {
    let contents: String = paths
        .iter()
        .map(|p| format!("{p}\n"))
        .collect();
    if std::fs::read_to_string(path).ok().as_deref() != Some(contents.as_str()) {
        std::fs::write(path, contents)?;
    }
    Ok(())
}

/// Whether a manifest field is an artifact path rather than a feature
/// list. Used to read manifests written before features were recorded.
fn looks_like_path(field: &str) -> bool {
    field.contains('/')
        || field.contains('\\')
        || field.ends_with(".rlib")
        || field.ends_with(".rmeta")
}

/// Parse a comma-separated feature field; `-` means none.
fn parse_features(field: &str) -> BTreeSet<String> {
    if field == "-" {
        return BTreeSet::new();
    }
    field
        .split(',')
        .filter(|f| !f.is_empty())
        .map(str::to_string)
        .collect()
}

/// Resolve a manifest artifact reference to an absolute path the rustc
/// wrapper can hand to `--extern`. Dev and bootstrap manifests store
/// absolute paths (used verbatim); a shipped SDK's manifest stores bare
/// basenames, which the loader has no fixed root for, so they are joined
/// with the install's `deps/` dir here. Keeping the shipped form
/// location-independent is what lets a downloaded SDK build projects
/// wherever it is unpacked, without rewriting the manifest on install.
fn resolve_artifact(artifact: &str, deps_dir: &Path) -> String {
    let path = Path::new(artifact);
    if path.is_absolute() {
        artifact.to_string()
    } else {
        deps_dir.join(artifact).to_string_lossy().into_owned()
    }
}

/// Write the per-edge redirect plan for the build root's resolve
/// graph: `consumer:alias=artifact` lines, one per dependency edge
/// whose resolved version is byte-identical to the SDK's. Consumed by
/// the rustc wrapper via `JACKDAW_SDK_EXTERN_MAP`. `deps_dir` is the
/// SDK's `deps/` directory, used to resolve basename-only artifacts in a
/// shipped manifest. Returns the number of edges written.
pub fn write_plan(
    build_root: &Path,
    manifest: &SdkManifest,
    deps_dir: &Path,
    out_path: &Path,
) -> Result<usize, PlanError> {
    let metadata = cargo_metadata(build_root)?;
    let mut contents = Vec::new();
    let mut edges = 0;
    let empty = Vec::new();
    // What the PROJECT resolved for each package. Cargo unifies
    // features across whichever graph it is building, so this is a
    // different answer from the SDK's for the same `(name, version)`
    // and has to be compared before an edge can redirect.
    let project_features: BTreeMap<String, BTreeSet<String>> = metadata["resolve"]["nodes"]
        .as_array()
        .unwrap_or(&empty)
        .iter()
        .filter_map(|node| {
            let id = node["id"].as_str()?;
            let features = node["features"]
                .as_array()
                .map(|fs| {
                    fs.iter()
                        .filter_map(|f| f.as_str())
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            Some((id.to_string(), features))
        })
        .collect();
    let no_features = BTreeSet::new();
    // Crates the SDK built with fewer features than the project needs.
    // Collected for the caller to surface: the redirect still happens
    // (see below), so a shortfall shows up later as missing items in a
    // crate nobody edited, and this is the only place that can name the
    // real cause.
    let mut shortfalls: BTreeMap<String, String> = BTreeMap::new();
    for node in metadata["resolve"]["nodes"].as_array().unwrap_or(&empty) {
        let Some(consumer_id) = node["id"].as_str() else {
            continue;
        };
        let (Some(consumer), Some(consumer_version)) = (
            package_id_name(consumer_id),
            package_id_version(consumer_id),
        ) else {
            continue;
        };
        for dep in node["deps"].as_array().unwrap_or(&empty) {
            let Some(alias) = dep["name"].as_str() else {
                continue;
            };
            let Some(pkg_id) = dep["pkg"].as_str() else {
                continue;
            };
            let (Some(dep_name), Some(dep_version)) =
                (package_id_name(pkg_id), package_id_version(pkg_id))
            else {
                continue;
            };
            // Feature mismatches are REPORTED, never acted on. Skipping
            // such an edge looks safer and is worse: redirection has to
            // apply to a crate everywhere or nowhere, and dropping one
            // edge while keeping its siblings puts two instances of that
            // crate in one graph. Dropping `bevy_tasks:async_task` while
            // keeping `bevy_tasks:async_executor` did exactly that -
            // `async_executor::spawn` returns the SDK's
            // `async_task::Task`, the local one is a different type, and
            // `bevy_tasks` stopped compiling. A mismatch means the SDK
            // was not built as a superset of the project, which is a
            // property of the SDK's own feature list, fixable only
            // there.
            let wanted = project_features.get(pkg_id).unwrap_or(&no_features);
            if !manifest.covers_features(&dep_name, dep_version, wanted)
                && manifest.artifact(&dep_name, dep_version).is_some()
            {
                shortfalls
                    .entry(format!("{dep_name} {dep_version}"))
                    .or_insert_with(|| manifest.feature_shortfall(&dep_name, dep_version, wanted));
            }
            if let Some(artifact) = manifest.artifact(&dep_name, dep_version) {
                // Key the edge on the consumer's exact version: a graph
                // can hold two versions of one crate (two `rand`s), each
                // wanting a different version of the same dependency, so
                // the name alone cannot pick the right redirect.
                let resolved = resolve_artifact(artifact, deps_dir);
                writeln!(contents, "{consumer}@{consumer_version}:{alias}={resolved}")?;
                edges += 1;
            }
        }
    }
    if std::fs::read(out_path).ok().as_deref() != Some(contents.as_slice()) {
        std::fs::write(out_path, contents)?;
    }
    // Name the SDK's feature shortfalls up front. Without this the
    // consequence appears as items "configured out" of a crate the
    // project never touched, with nothing pointing at the SDK's own
    // feature list as the thing to change.
    for (crate_id, missing) in &shortfalls {
        if !missing.is_empty() {
            tracing::warn!(
                "SDK built {crate_id} without {missing}; the project needs it. \
                 Add the feature that pulls it to jackdaw's workspace `bevy` \
                 dependency and rebuild the SDK."
            );
        }
    }
    Ok(edges)
}

/// The SDK dylib's runtime dependency closure: `(name, version)`
/// pairs from `cargo tree`. Dev-checkout only.
fn sdk_runtime_closure(workspace_root: &Path) -> Result<BTreeSet<(String, String)>, PlanError> {
    let output = Command::new("cargo")
        .args([
            "tree",
            "-p",
            "jackdaw_sdk",
            "-e",
            "normal,no-proc-macro",
            "--prefix",
            "none",
        ])
        .current_dir(workspace_root)
        .output()?;
    if !output.status.success() {
        return Err(PlanError::Cargo("cargo tree failed".into()));
    }
    let mut closure = BTreeSet::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let mut parts = line.split_whitespace();
        let (Some(name), Some(version)) = (parts.next(), parts.next()) else {
            continue;
        };
        let Some(version) = version.strip_prefix('v') else {
            continue;
        };
        closure.insert((name.replace('-', "_"), version.to_string()));
    }
    Ok(closure)
}

fn cargo_metadata(dir: &Path) -> Result<serde_json::Value, PlanError> {
    let output = Command::new("cargo")
        .args(["metadata", "--format-version", "1"])
        .current_dir(dir)
        .output()?;
    if !output.status.success() {
        return Err(PlanError::Cargo(format!(
            "cargo metadata failed in {}",
            dir.display()
        )));
    }
    serde_json::from_slice(&output.stdout)
        .map_err(|e| PlanError::Parse(format!("cargo metadata output: {e}")))
}

/// The version embedded in a cargo package id
/// (`registry+https://...#glam@0.32.1` or `path+file:///...#0.1.0`).
fn package_id_version(id: &str) -> Option<&str> {
    let fragment = id.rsplit_once('#')?.1;
    Some(match fragment.rsplit_once('@') {
        Some((_, version)) => version,
        None => fragment,
    })
}

/// The package name embedded in a cargo package id, normalized to
/// underscores. Registry ids carry it in the fragment; path ids carry
/// only a version there, so the name is the last path segment.
fn package_id_name(id: &str) -> Option<String> {
    package_id_raw_name(id).map(|n| n.replace('-', "_"))
}

/// The package name exactly as cargo spells it (`cargo update -p`
/// rejects normalized names).
fn package_id_raw_name(id: &str) -> Option<String> {
    let (base, fragment) = id.rsplit_once('#')?;
    let name = match fragment.rsplit_once('@') {
        Some((name, _)) => name,
        None => base.rsplit('/').next()?,
    };
    Some(name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_ids_parse() {
        let reg = "registry+https://github.com/rust-lang/crates.io-index#glam@0.32.1";
        assert_eq!(package_id_version(reg), Some("0.32.1"));
        assert_eq!(package_id_name(reg).as_deref(), Some("glam"));
        let path = "path+file:///home/u/my-game#0.1.0";
        assert_eq!(package_id_version(path), Some("0.1.0"));
        assert_eq!(package_id_name(path).as_deref(), Some("my_game"));
        assert_eq!(package_id_raw_name(path).as_deref(), Some("my-game"));
    }

    fn manifest_of(entries: &[(&str, &str, &[&str], &str)]) -> SdkManifest {
        SdkManifest {
            artifacts: entries
                .iter()
                .map(|(name, version, features, path)| {
                    (
                        (name.to_string(), version.to_string()),
                        SdkArtifact {
                            features: features.iter().map(|f| f.to_string()).collect(),
                            path: path.to_string(),
                        },
                    )
                })
                .collect(),
        }
    }

    fn feature_set(features: &[&str]) -> BTreeSet<String> {
        features.iter().map(|f| f.to_string()).collect()
    }

    /// Lock alignment pins a crate to "the SDK's version", which only
    /// means something when the SDK holds one. It carries four
    /// `windows-sys` majors, and picking one of those to pin to would be
    /// arbitrary and wrong.
    #[test]
    fn only_an_unambiguous_sdk_version_can_be_pinned_to() {
        let manifest = manifest_of(&[
            ("serde_json", "1.0.149", &["std"], "/sdk/libserde_json.rlib"),
            ("windows_sys", "0.52.0", &["Win32"], "/sdk/libws52.rlib"),
            ("windows_sys", "0.60.2", &["Win32"], "/sdk/libws60.rlib"),
        ]);

        assert_eq!(manifest.sole_version("serde_json"), Some("1.0.149"));
        assert_eq!(manifest.version_count("serde_json"), 1);

        assert_eq!(
            manifest.sole_version("windows_sys"),
            None,
            "several majors: no single version to pin to"
        );
        assert_eq!(manifest.version_count("windows_sys"), 2);

        assert_eq!(manifest.sole_version("not_in_the_sdk"), None);
    }

    /// Feature coverage drives a diagnostic, not a skip. Redirection has
    /// to apply to a crate everywhere or nowhere, so a shortfall is
    /// reported and the edge still redirects; the cure is the SDK's own
    /// feature list.
    #[test]
    fn a_feature_shortfall_is_detected_and_named() {
        let manifest = manifest_of(&[(
            "windows_sys",
            "0.60.2",
            &["Win32", "Win32_Foundation"],
            "/sdk/libwindows_sys.rlib",
        )]);
        let wanted = feature_set(&["Win32", "Win32_UI", "Win32_System_Ole"]);

        assert!(!manifest.covers_features("windows_sys", "0.60.2", &wanted));
        assert_eq!(
            manifest.feature_shortfall("windows_sys", "0.60.2", &wanted),
            "Win32_System_Ole,Win32_UI",
            "the message must name exactly what the SDK lacks"
        );
        // Covered: nothing to report.
        assert_eq!(
            manifest.feature_shortfall("windows_sys", "0.60.2", &feature_set(&["Win32"])),
            ""
        );
    }

    /// An edge may only redirect to an SDK artifact built with at least
    /// the features its consumer resolved. Matching on version alone
    /// handed `arboard` a `windows-sys` compiled with every feature off,
    /// and it failed on imports that had been configured out.
    #[test]
    fn an_edge_redirects_only_when_the_sdk_covers_its_features() {
        let manifest = manifest_of(&[(
            "windows_sys",
            "0.60.2",
            &["Win32", "Win32_Foundation"],
            "/sdk/libwindows_sys.rlib",
        )]);

        // Subset and exact match both redirect.
        assert!(manifest.covers_features("windows_sys", "0.60.2", &feature_set(&["Win32"])));
        assert!(manifest.covers_features(
            "windows_sys",
            "0.60.2",
            &feature_set(&["Win32", "Win32_Foundation"])
        ));
        // The real failure: the consumer needs a feature the SDK build
        // does not have.
        assert!(!manifest.covers_features(
            "windows_sys",
            "0.60.2",
            &feature_set(&["Win32", "Win32_UI"])
        ));
        // A version the SDK does not carry redirects to nothing.
        assert!(!manifest.covers_features("windows_sys", "0.61.2", &feature_set(&[])));
    }

    /// A manifest written before features were recorded still loads, and
    /// its entries are treated as covering nothing rather than as
    /// covering everything - a stale manifest should cost redirects, not
    /// hand out artifacts missing the symbols a consumer imports.
    #[test]
    fn a_legacy_manifest_loads_and_claims_no_features() {
        let dir = std::env::temp_dir().join(format!("jackdaw_legacy_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("manifest.txt");
        std::fs::write(
            &path,
            "glam 0.32.1 C:\\sdk\\build\\glam\\abc\\out\\libglam-abc.rlib\n",
        )
        .unwrap();

        let manifest = SdkManifest::load(&path).unwrap();
        assert_eq!(
            manifest.artifact("glam", "0.32.1"),
            Some("C:\\sdk\\build\\glam\\abc\\out\\libglam-abc.rlib"),
            "the path must survive intact, separators and all"
        );
        assert!(manifest.covers_features("glam", "0.32.1", &feature_set(&[])));
        assert!(!manifest.covers_features("glam", "0.32.1", &feature_set(&["std"])));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Features survive a write/load cycle, so the gate behaves the same
    /// against a manifest read back from disk as against a fresh one.
    #[test]
    fn features_round_trip_through_the_manifest_file() {
        let dir = std::env::temp_dir().join(format!("jackdaw_features_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("manifest.txt");

        manifest_of(&[
            ("windows_sys", "0.60.2", &["Win32", "Win32_UI"], "/a.rlib"),
            ("glam", "0.32.1", &[], "/b.rlib"),
        ])
        .write(&path)
        .unwrap();

        let back = SdkManifest::load(&path).unwrap();
        assert!(back.covers_features("windows_sys", "0.60.2", &feature_set(&["Win32_UI"])));
        assert!(!back.covers_features("windows_sys", "0.60.2", &feature_set(&["Win32_Ole"])));
        // An empty feature set writes as `-` and reads back as empty,
        // not as a feature literally named "-".
        assert_eq!(back.artifact("glam", "0.32.1"), Some("/b.rlib"));
        assert!(back.covers_features("glam", "0.32.1", &feature_set(&[])));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn manifest_round_trips() {
        let dir = std::env::temp_dir().join("jackdaw_manifest_test");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("manifest.txt");
        let manifest = manifest_of(&[("glam", "0.32.1", &["std"], "/sdk/deps/libglam-abc.rlib")]);
        manifest.write(&path).unwrap();
        let back = SdkManifest::load(&path).unwrap();
        assert_eq!(
            back.artifact("glam", "0.32.1"),
            Some("/sdk/deps/libglam-abc.rlib")
        );
    }

    #[test]
    fn resolve_artifact_absolute_passthrough_basename_rebases() {
        let deps = Path::new("/opt/jackdaw/sdk/x86_64/deps");
        // Dev/bootstrap manifests store absolute paths: used verbatim.
        assert_eq!(
            resolve_artifact("/ws/target/x86_64/release/deps/libglam-abc.rlib", deps),
            "/ws/target/x86_64/release/deps/libglam-abc.rlib"
        );
        // A shipped manifest stores basenames, rebased onto the install deps.
        // Compared with native separators normalized away: `join` uses the
        // host's, so a literal forward-slash expectation only holds on unix.
        assert_eq!(
            resolve_artifact("libglam-abc.rlib", deps).replace('\\', "/"),
            "/opt/jackdaw/sdk/x86_64/deps/libglam-abc.rlib"
        );
    }

    /// Cargo emits native separators, so the triple-dir filter that
    /// selects target-side artifacts has to match a Windows path too.
    /// When it did not, the manifest came out empty and every redirect
    /// beyond the `bevy` facade silently stopped happening.
    #[test]
    fn the_triple_dir_filter_matches_native_separators() {
        let triple = "/x86_64-pc-windows-msvc/";
        let in_triple_dir = |f: &str| f.replace('\\', "/").contains(triple);

        assert!(in_triple_dir(
            r"C:\cache\build\target\x86_64-pc-windows-msvc\release\build\bevy_ecs\abc\out\libbevy_ecs-abc.rlib"
        ));
        assert!(in_triple_dir(
            "/home/u/build/target/x86_64-pc-windows-msvc/release/deps/libbevy_ecs-abc.rlib"
        ));
        // Host-side units live outside the triple dir and must not match:
        // they are built for the build host, not the target.
        assert!(!in_triple_dir(
            r"C:\cache\build\target\release\build\proc-macro2\abc\out\libproc_macro2-abc.rlib"
        ));
    }
}
