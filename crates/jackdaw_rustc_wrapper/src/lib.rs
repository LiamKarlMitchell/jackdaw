//! Thin rustc wrapper for jackdaw extension and game projects.
//!
//! # What it does
//!
//! Cargo invokes this binary as `RUSTC_WRAPPER`, so every rustc call
//! in the project passes through here. Target-side invocations are
//! rewritten so the whole dependency graph shares the SDK's crates:
//!
//! * `--extern bevy=<anything>` becomes
//!   `--extern bevy=$JACKDAW_SDK_DYLIB` for every consumer. The user's
//!   Cargo.toml still declares a normal bevy dependency so bevy's proc
//!   macros find it via `CARGO_MANIFEST_DIR` and emit `::bevy::...`
//!   paths. Cargo compiles real bevy into the project's target dir;
//!   those rlibs are ignored because every `--extern` that matters
//!   points at the SDK's artifacts.
//! * Every dependency edge listed in the `$JACKDAW_SDK_EXTERN_MAP`
//!   redirect plan is rewritten to the SDK artifact it names. The plan
//!   covers the SDK's full runtime closure (bevy subcrates plus public
//!   deps like glam and serde), per edge, only where the project's
//!   resolved version is byte-identical to the SDK's.
//! * `--extern jackdaw_api=$JACKDAW_SDK_DYLIB` is injected for the
//!   primary crate. The user never declares `jackdaw_api`; the wrapper
//!   makes `use jackdaw_api::...` work anyway.
//! * `-L dependency=$JACKDAW_SDK_DEPS` is appended so rustc can find
//!   transitive rlib metadata when resolving re-exported types.
//! * `-C prefer-dynamic` is appended so rustc links through the SDK
//!   dylib rather than statically embedding its rlib form.
//!
//! Host-side invocations (no `--target`: build scripts, proc-macro
//! crates and their deps) and compiles of plan-replaced packages pass
//! through untouched. The driving cargo invocation must therefore
//! always pass an explicit `--target` (the host triple).
//!
//! # Why the wrapper exists as a library plus a binary
//!
//! The logic lives in a library so the build driver can call [`run`]
//! in-process, and ships as a binary so rustc can exec it as
//! `RUSTC_WRAPPER`.
//!
//! # Why
//!
//! Cargo's `-Cmetadata` hash is not stable across independent
//! workspaces, so "build bevy twice and hope the hashes line up"
//! doesn't work. Forcing the user crate to link against the one
//! `libjackdaw_sdk.so` shipped with the editor makes every
//! `TypeId::of::<T>()` in user code agree with the editor's copy,
//! which is what reflection and dlopen require.
//!
//! # Env vars the wrapper reads
//!
//! | Var                       | Required       | Purpose                              |
//! |---------------------------|----------------|--------------------------------------|
//! | `JACKDAW_SDK_DYLIB`       | yes            | Absolute path to `libjackdaw_sdk.so` |
//! | `JACKDAW_SDK_DEPS`        | yes            | Absolute path to the `deps/` dir     |
//! | `JACKDAW_SDK_DYLIB_RMETA` | no             | Full metadata for the SDK dylib      |
//! | `JACKDAW_SDK_DEP_DIRS`    | no             | Extra `-L dependency=` dirs (joined) |
//! | `JACKDAW_SDK_LINK_PATHS`  | no             | SDK build-script `-L` dirs (per line) |
//! | `JACKDAW_SDK_HOST_DEPS`   | no             | Host deps dir (proc-macro dylibs)    |
//! | `JACKDAW_SDK_EXTERN_MAP`  | no             | Path to the per-edge redirect plan   |
//! | `JACKDAW_WRAPPER_LOG`     | no             | If `1`, log rewrites to stderr       |
//! | `CARGO_PRIMARY_PACKAGE`   | (set by cargo) | `1` while compiling the user crate   |
//! | `CARGO_PKG_NAME`          | (set by cargo) | Consumer key for plan edge lookups   |
//!
//! # Metadata companions
//!
//! Cargo on recent nightlies defaults to `-Zembed-metadata=no`: an
//! `.rlib` or dylib carries only a metadata *stub*, and the full
//! metadata lives in a sibling `.rmeta`. Cargo compensates by emitting
//! `--extern <alias>=` **twice** per dependency, once for the link
//! artifact and once for the `.rmeta`. Redirecting only ever produced
//! one path, which collapsed both halves of that pair onto a stub and
//! failed the compile with
//! `only metadata stub found for dylib dependency ...`. Every redirect
//! therefore emits its `.rmeta` companion too, when one exists; where
//! metadata is still embedded there is no companion and nothing changes.

use std::env;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use tracing::error;

const ENV_SDK_DYLIB: &str = "JACKDAW_SDK_DYLIB";
const ENV_SDK_DYLIB_RMETA: &str = "JACKDAW_SDK_DYLIB_RMETA";
const ENV_SDK_DEPS: &str = "JACKDAW_SDK_DEPS";
const ENV_SDK_DEP_DIRS: &str = "JACKDAW_SDK_DEP_DIRS";
const ENV_SDK_LINK_PATHS: &str = "JACKDAW_SDK_LINK_PATHS";
const ENV_SDK_HOST_DEPS: &str = "JACKDAW_SDK_HOST_DEPS";
const ENV_PRIMARY_PACKAGE: &str = "CARGO_PRIMARY_PACKAGE";
const ENV_LOG: &str = "JACKDAW_WRAPPER_LOG";
const ENV_EXTERN_MAP: &str = "JACKDAW_SDK_EXTERN_MAP";
// Static-SDK model: when set to "1", bevy and jackdaw_api are redirected
// to their prebuilt rlibs and `-C prefer-dynamic` is not added, so the
// project dylib embeds one shared bevy compilation (matching TypeIds via
// the rmeta trick) rather than linking a `libjackdaw_sdk` dll. This is the
// only model that links on Windows, where a dll cannot export bevy's
// reflect statics.
const ENV_STATIC: &str = "JACKDAW_SDK_STATIC";
const ENV_SDK_BEVY_RLIB: &str = "JACKDAW_SDK_BEVY_RLIB";
const ENV_SDK_API_RLIB: &str = "JACKDAW_SDK_API_RLIB";

/// Crate aliases we redirect to `libjackdaw_sdk.so` whenever cargo
/// emits an `--extern` flag for them. User code writes
/// `use bevy::prelude::*;` and cargo passes `--extern bevy=<stub>.rlib`
/// to rustc; we rewrite the value here.
const REDIRECTED_CRATES: &[&str] = &["bevy"];

/// Crate aliases we inject unconditionally so `use jackdaw_api::...`
/// resolves without the user having to declare `jackdaw_api` in
/// their Cargo.toml. The rustc command picks up these `--extern`
/// flags exactly as cargo-emitted ones would be.
const INJECTED_CRATES: &[&str] = &["jackdaw_api"];

/// Entry point for both the standalone wrapper binary in this crate
/// and the wrapper binary shipped by the top-level `jackdaw` package.
/// Returns the exit code rustc produced (or 1 on a wrapper-side
/// failure).
pub fn run() -> ExitCode {
    // Stderr only: cargo parses this wrapper's stdout during its
    // `--print=file-names` probe invocations, and any stray line there
    // corrupts cargo's idea of what every unit emits.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();
    let mut argv: Vec<OsString> = env::args_os().collect();
    // argv[0] is our binary; argv[1] is the real rustc path; argv[2..]
    // are rustc's args.
    if argv.len() < 2 {
        error!("jackdaw-rustc-wrapper: no rustc path provided");
        return ExitCode::from(1);
    }
    let rustc = argv.remove(1);
    // Cargo has its own response-file mechanism, independent of rustc's: when
    // cargo's own invocation of us would be too long, it collapses it to a
    // single `@cargo-argfile.XXXXXX` token before ever calling this wrapper.
    // Expand that ourselves rather than writing the literal "@path" string
    // into our own outgoing argfile and hoping rustc's nested-argfile
    // expansion resolves it - relying on that indirection both crashed (the
    // nested open failed) and silently broke the SDK redirects below, which
    // never saw `--target`/`--extern` because they were hidden inside
    // cargo's file.
    let mut rustc_args: Vec<OsString> = expand_argfiles(argv.split_off(1));

    let is_primary = env::var_os(ENV_PRIMARY_PACKAGE).is_some_and(|v| v == "1");
    let log = env::var_os(ENV_LOG).is_some_and(|v| v == "1");

    // The redirects apply to EVERY target-side crate in the graph, so
    // ecosystem dependencies (physics, etc.) compile against the same
    // SDK bevy and closure crates the user's crate does: one instance
    // of every shared crate. Host-side units pass through unchanged.
    if let Err(e) = rewrite_args(&mut rustc_args, is_primary, log) {
        error!("jackdaw-rustc-wrapper: {e}");
        return ExitCode::from(1);
    }

    // Always route through an rustc @argfile rather than the raw command
    // line. Windows' ~32KB CreateProcess limit is far below what a large
    // bevy + jackdaw dependency graph's `-L`/`--extern` flags produce (see
    // docs/jackdaw-improvements.md, "filename or extension too long"); other
    // platforms have a much higher ARG_MAX but pay only a negligible extra
    // temp-file write per rustc invocation, so one code path for every OS
    // beats maintaining a length-threshold branch that could still be wrong.
    let argfile_path = env::temp_dir().join(format!(
        "jackdaw-rustc-wrapper-{}-{}.args",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let argfile_contents = rustc_args
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("\n");
    if let Err(e) = std::fs::write(&argfile_path, argfile_contents) {
        error!(
            "jackdaw-rustc-wrapper: failed to write argfile {argfile_path:?}: {e}"
        );
        return ExitCode::from(1);
    }

    let mut argfile_arg = OsString::from("@");
    argfile_arg.push(&argfile_path);
    let status = Command::new(&rustc).arg(argfile_arg).status();
    let _ = std::fs::remove_file(&argfile_path);

    match status {
        Ok(s) => ExitCode::from(s.code().unwrap_or(1) as u8),
        Err(e) => {
            error!("jackdaw-rustc-wrapper: failed to spawn {rustc:?}: {e}");
            ExitCode::from(1)
        }
    }
}

/// Recursively expand any `@path` argument into the lines of that file, one
/// arg per line (rustc/cargo's shared, unquoted response-file format - see
/// `-Zshell-argfiles` in `rustc -Z help` for the opt-in quoted alternative,
/// which neither side uses here). Handles both rustc's own `@file` and
/// cargo's independent pre-collapse of an overlong invocation into
/// `@cargo-argfile.XXXXXX` before it ever reaches this wrapper.
fn expand_argfiles(args: Vec<OsString>) -> Vec<OsString> {
    let mut out = Vec::with_capacity(args.len());
    for arg in args {
        match arg.to_str().and_then(|s| s.strip_prefix('@')) {
            Some(path) => match std::fs::read_to_string(path) {
                Ok(contents) => {
                    let lines: Vec<OsString> = contents.lines().map(OsString::from).collect();
                    out.extend(expand_argfiles(lines));
                }
                Err(e) => {
                    error!("jackdaw-rustc-wrapper: failed to expand argfile {path}: {e}");
                    out.push(arg);
                }
            },
            None => out.push(arg),
        }
    }
    out
}

/// Rewrite one rustc invocation. The `bevy` facade extern redirects to
/// the SDK dylib; any other extern named in the SDK extern map (the
/// SDK's runtime dependency closure: bevy subcrates plus their public
/// deps like `glam` and `serde`) redirects to the exact artifact the
/// SDK was built with. When any redirect fired, a
/// `-L dependency=$JACKDAW_SDK_DEPS` and `-C prefer-dynamic` are
/// appended so the redirected metadata resolves and the final link goes
/// through the dylib. The primary package additionally gets
/// `--extern jackdaw_api=` injected.
///
/// Host-side units (no `--target` flag) pass through untouched. Every
/// target-side unit is rewritten uniformly, including compiles of
/// crates the plan itself replaces: a unit compiled vanilla could
/// consume a rewritten sibling and see two instances of one crate, so
/// coherence requires the SDK-preferred resolution to apply everywhere
/// or nowhere.
fn rewrite_args(argv: &mut Vec<OsString>, is_primary: bool, log: bool) -> Result<(), String> {
    let deps = env::var_os(ENV_SDK_DEPS)
        .ok_or_else(|| format!("{ENV_SDK_DEPS} not set; cannot point -L at deps/"))?;
    let static_mode = env::var_os(ENV_STATIC).is_some_and(|v| v == "1");
    // Redirect targets for the bevy facade and the injected jackdaw_api. In
    // the shared-dylib model both point at libjackdaw_sdk; in the static
    // model each points at its own prebuilt rlib, so rustc embeds one
    // shared bevy compilation instead of linking a dll.
    let (bevy_target, api_target) = if static_mode {
        (
            env::var_os(ENV_SDK_BEVY_RLIB)
                .ok_or_else(|| format!("{ENV_SDK_BEVY_RLIB} not set in static mode"))?,
            env::var_os(ENV_SDK_API_RLIB)
                .ok_or_else(|| format!("{ENV_SDK_API_RLIB} not set in static mode"))?,
        )
    } else {
        let dylib = env::var_os(ENV_SDK_DYLIB)
            .ok_or_else(|| format!("{ENV_SDK_DYLIB} not set; cannot redirect --extern"))?;
        (dylib.clone(), dylib)
    };
    let extern_map = load_extern_map();

    // Host-side units (no --target: build scripts, proc-macro crates
    // and their deps) run against their own host dep units and must
    // never see SDK artifacts. This requires the driving cargo
    // invocation to pass an explicit --target (the host triple), which
    // is what makes cargo omit the flag on host units.
    if !argv.iter().any(|a| a == "--target") {
        return Ok(());
    }

    // The consumer key is name@version: a graph can hold two versions
    // of one crate, and the plan records a distinct redirect per
    // version. cargo sets both vars for every compile.
    let consumer = match (env::var("CARGO_PKG_NAME"), env::var("CARGO_PKG_VERSION")) {
        (Ok(name), Ok(version)) => format!("{}@{version}", name.replace('-', "_")),
        _ => String::new(),
    };

    // The SDK dylib's own full metadata. It is not a sibling of the
    // uplifted dylib (cargo leaves it in the unit's build dir), so it
    // cannot be probed for and has to be passed in.
    let dylib_rmeta = env::var_os(ENV_SDK_DYLIB_RMETA).filter(|p| Path::new(p).is_file());

    // Rebuild the `--extern` flags rather than editing in place. Cargo
    // emits a pair per dependency under `-Zembed-metadata=no`, and both
    // halves rewrite to the same artifact, so the pass has to drop the
    // resulting duplicate and append the redirect's own `.rmeta`
    // companion in its place.
    let mut redirected = false;
    let mut seen: Vec<OsString> = Vec::new();
    let mut companions: Vec<OsString> = Vec::new();
    let mut out: Vec<OsString> = Vec::with_capacity(argv.len());
    let mut i = 0;
    while i < argv.len() {
        if argv[i] == "--extern" && i + 1 < argv.len() {
            let value = match rewrite_extern(&argv[i + 1], &bevy_target, &extern_map, &consumer) {
                Some(new_value) => {
                    if log {
                        error!(
                            "jackdaw-rustc-wrapper: rewrite --extern {:?} -> {:?}",
                            argv[i + 1], new_value
                        );
                    }
                    redirected = true;
                    if let Some(rmeta) =
                        metadata_companion(&new_value, &bevy_target, dylib_rmeta.as_ref())
                        && !companions.contains(&rmeta)
                    {
                        if log {
                            error!("jackdaw-rustc-wrapper: metadata companion {rmeta:?}");
                        }
                        companions.push(rmeta);
                    }
                    new_value
                }
                None => argv[i + 1].clone(),
            };
            if !seen.contains(&value) {
                seen.push(value.clone());
                out.push(OsString::from("--extern"));
                out.push(value);
            }
            i += 2;
            continue;
        }
        out.push(argv[i].clone());
        i += 1;
    }
    for companion in companions {
        out.push(OsString::from("--extern"));
        out.push(companion);
    }
    *argv = out;

    if is_primary {
        for alias in INJECTED_CRATES {
            let mut flag = OsString::from(alias);
            flag.push("=");
            flag.push(&api_target);
            let companion = metadata_companion(&flag, &bevy_target, dylib_rmeta.as_ref());
            argv.push(OsString::from("--extern"));
            argv.push(flag);
            if log {
                error!(
                    "jackdaw-rustc-wrapper: injected --extern {}={}",
                    alias,
                    api_target.to_string_lossy()
                );
            }
            if let Some(companion) = companion {
                argv.push(OsString::from("--extern"));
                argv.push(companion);
            }
        }
    }

    // Every target-side unit gets the SDK deps dir as a search path: a
    // unit that was NOT itself rewritten can still consume a crate that
    // was, and loading that crate's metadata requires resolving the SDK
    // artifacts it references. rustc matches transitive crates by exact
    // SVH, so the extra path cannot shadow the user graph's own crates.
    let mut deps_flag = OsString::from("dependency=");
    deps_flag.push(&deps);
    argv.push(OsString::from("-L"));
    argv.push(deps_flag);
    // SDK rlibs can reference host-side proc-macro dylibs (a MacrosOnly
    // dependency like thiserror's derive crate); those live in the SDK
    // build's host deps dir, not the triple dir.
    if let Some(host_deps) = env::var_os(ENV_SDK_HOST_DEPS) {
        let mut host_flag = OsString::from("dependency=");
        host_flag.push(&host_deps);
        argv.push(OsString::from("-L"));
        argv.push(host_flag);
    }
    // Recent cargo nightlies no longer collect a build's libraries into
    // one `deps/` dir; each unit gets its own
    // `build/<pkg>/<hash>/out/`. `-L dependency=` does not recurse, so
    // one path can no longer cover the SDK's closure and the driver
    // passes the whole list. Absent (older cargo, shipped SDK) the two
    // paths above still cover it.
    if let Some(dep_dirs) = env::var_os(ENV_SDK_DEP_DIRS) {
        for dir in env::split_paths(&dep_dirs) {
            let mut flag = OsString::from("dependency=");
            flag.push(dir);
            argv.push(OsString::from("-L"));
            argv.push(flag);
        }
    }
    // Native search paths from the SDK's build scripts. A redirected
    // rlib can require an import library the project's own graph never
    // builds a path to - the SDK's `windows_x86_64_msvc 0.42.2` ships
    // the plain `windows.lib`, while a project on 0.52/0.60 has only
    // `windows.0.52.0.lib` - and the link then fails naming the file
    // and no crate. Newline-separated, since each entry already carries
    // cargo's `KIND=PATH` form and a path list separator would clash
    // with the `=` and with Windows drive letters.
    if let Some(link_paths) = env::var_os(ENV_SDK_LINK_PATHS)
        && let Some(link_paths) = link_paths.to_str()
    {
        for entry in link_paths.lines().filter(|l| !l.trim().is_empty()) {
            // Cargo emits `native=/path`; pass a bare path through as
            // `native=` since that is what a build script's
            // `rustc-link-search` defaults to.
            let flag = if entry.contains('=') {
                entry.to_string()
            } else {
                format!("native={entry}")
            };
            argv.push(OsString::from("-L"));
            argv.push(OsString::from(flag));
        }
    }

    // `-C prefer-dynamic` links through the SDK dll. In the static model
    // there is no dll: the redirected rlibs are embedded, so it is omitted
    // (and would otherwise pull the toolchain's dynamic std/test crates).
    if !static_mode && (redirected || is_primary) {
        argv.push(OsString::from("-C"));
        argv.push(OsString::from("prefer-dynamic"));
        if log {
            error!("jackdaw-rustc-wrapper: appended -C prefer-dynamic");
        }
    }

    Ok(())
}

/// Parse `$JACKDAW_SDK_EXTERN_MAP`, the per-project redirect plan the
/// editor generates by joining the SDK's artifact list against the
/// project's resolve graph. Each `consumer@version:alias=artifact`
/// line redirects one dependency edge; the consumer carries its exact
/// version so two versions of one crate get distinct redirects. Lines
/// are only emitted where the consumer's resolved version of the
/// dependency is byte-identical to the SDK's. Absent or unreadable
/// plans degrade to facade-only redirection.
fn load_extern_map() -> ExternMap {
    let mut map = ExternMap::default();
    let Some(path) = env::var_os(ENV_EXTERN_MAP) else {
        return map;
    };
    let Ok(contents) = std::fs::read_to_string(&path) else {
        error!(
            "jackdaw-rustc-wrapper: could not read extern map at {:?}; \
             falling back to facade-only redirection",
            path
        );
        return map;
    };
    for line in contents.lines() {
        if let Some((edge, artifact)) = line.split_once('=')
            && let Some((consumer, alias)) = edge.split_once(':')
        {
            map.edges.push((
                consumer.to_string(),
                alias.to_string(),
                OsString::from(artifact),
            ));
        }
    }
    map
}

#[derive(Default)]
struct ExternMap {
    edges: Vec<(String, String, OsString)>,
}

impl ExternMap {
    fn edge_artifact(&self, consumer: &str, alias: &str) -> Option<&OsString> {
        self.edges
            .iter()
            .find(|(c, a, _)| c == consumer && a == alias)
            .map(|(_, _, artifact)| artifact)
    }
}

/// If `value` is `<alias>=<path>` with a redirect target, return the
/// redirected form. The `bevy` facade goes to `bevy_target` (the SDK dll
/// in the shared model, the prebuilt bevy rlib in the static model); an
/// edge listed in the plan for this consumer goes to its recorded
/// artifact. Otherwise `None`, and the caller leaves the flag alone.
fn rewrite_extern(
    value: &OsString,
    bevy_target: &OsString,
    extern_map: &ExternMap,
    consumer: &str,
) -> Option<OsString> {
    let s = value.to_str()?;
    let (alias, _rest) = s.split_once('=')?;
    if REDIRECTED_CRATES.contains(&alias) {
        let mut out = OsString::from(alias);
        out.push("=");
        out.push(bevy_target);
        return Some(out);
    }
    let artifact = extern_map.edge_artifact(consumer, alias)?;
    let mut out = OsString::from(alias);
    out.push("=");
    out.push(artifact);
    Some(out)
}

/// The `<alias>=<path>.rmeta` companion for a rewritten `--extern`
/// value, or `None` when the artifact already carries its metadata.
///
/// Under `-Zembed-metadata=no` a library holds a stub and the real
/// metadata sits in a `.rmeta`. For a plan artifact
/// (`.../out/libbevy_render-<hash>.rlib`) that file is its sibling. The
/// SDK dylib is the exception: cargo uplifts it to the profile dir and
/// leaves its `.rmeta` behind in the unit's build dir, so the driver
/// passes that path in `$JACKDAW_SDK_DYLIB_RMETA` and it is used for
/// any redirect pointing at the dylib.
fn metadata_companion(
    value: &OsStr,
    dylib_target: &OsStr,
    dylib_rmeta: Option<&OsString>,
) -> Option<OsString> {
    let s = value.to_str()?;
    let (alias, path) = s.split_once('=')?;
    let path = Path::new(path);
    if path.extension().is_some_and(|ext| ext == "rmeta") {
        return None;
    }
    let rmeta = match dylib_rmeta {
        // The dylib case only; the static model redirects to rlibs,
        // whose companions sit beside them like any other unit's.
        Some(rmeta) if path == Path::new(dylib_target) => PathBuf::from(rmeta),
        _ => {
            let sibling = path.with_extension("rmeta");
            if !sibling.is_file() {
                return None;
            }
            sibling
        }
    };
    let mut out = OsString::from(alias);
    out.push("=");
    out.push(rmeta);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = env::temp_dir().join(format!(
            "jackdaw_wrapper_{name}_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn extern_value(alias: &str, path: &Path) -> OsString {
        let mut v = OsString::from(alias);
        v.push("=");
        v.push(path);
        v
    }

    /// Under `-Zembed-metadata=no` a plan artifact is a stub whose real
    /// metadata is the `.rmeta` beside it. Redirecting to the stub alone
    /// is what produces `only metadata stub found for dylib dependency`.
    #[test]
    fn a_plan_artifact_takes_its_sibling_rmeta() {
        let dir = scratch("sibling");
        let rlib = dir.join("libbevy_render-abc.rlib");
        std::fs::write(&rlib, b"stub").unwrap();
        std::fs::write(dir.join("libbevy_render-abc.rmeta"), b"meta").unwrap();

        let got = metadata_companion(&extern_value("bevy_render", &rlib), OsStr::new(""), None);
        assert_eq!(
            got,
            Some(extern_value(
                "bevy_render",
                &dir.join("libbevy_render-abc.rmeta")
            ))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Embedded metadata (older cargo, a shipped SDK) has no companion,
    /// and inventing one would hand rustc a path that does not resolve.
    #[test]
    fn an_embedded_artifact_has_no_companion() {
        let dir = scratch("embedded");
        let rlib = dir.join("libglam-abc.rlib");
        std::fs::write(&rlib, b"whole").unwrap();

        assert_eq!(
            metadata_companion(&extern_value("glam", &rlib), OsStr::new(""), None),
            None
        );
        // Cargo's own `.rmeta` half of the pair is already metadata.
        let rmeta = dir.join("libglam-abc.rmeta");
        std::fs::write(&rmeta, b"meta").unwrap();
        assert_eq!(
            metadata_companion(&extern_value("glam", &rmeta), OsStr::new(""), None),
            None
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The SDK dylib is the one artifact whose `.rmeta` is not beside
    /// it - cargo uplifts the dylib and leaves the metadata in the unit
    /// build dir - so the path has to be passed in.
    #[test]
    fn the_dylib_takes_the_path_it_is_given() {
        let dir = scratch("dylib");
        let dylib = dir.join("jackdaw_sdk.dll");
        std::fs::write(&dylib, b"stub").unwrap();
        let rmeta = dir.join("build/jackdaw_sdk/abc/out/libjackdaw_sdk.rmeta");
        std::fs::create_dir_all(rmeta.parent().unwrap()).unwrap();
        std::fs::write(&rmeta, b"meta").unwrap();
        let rmeta_env = OsString::from(&rmeta);

        let got = metadata_companion(
            &extern_value("bevy", &dylib),
            dylib.as_os_str(),
            Some(&rmeta_env),
        );
        assert_eq!(got, Some(extern_value("bevy", &rmeta)));

        // Without it there is nothing beside the dylib to fall back to.
        assert_eq!(
            metadata_companion(&extern_value("bevy", &dylib), dylib.as_os_str(), None),
            None
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
