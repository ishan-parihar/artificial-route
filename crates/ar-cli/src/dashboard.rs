//! `aroute dashboard` — spawn the bundled web UI server.
//!
//! The dashboard ships as a *compiled* Next.js standalone tree (see
//! `dashboard/rebrand-dist.sh`): `server.js`, its traced `node_modules`, and
//! the built `.next` output — no TypeScript, no build tooling at runtime.
//! This verb is only the supervisor: it resolves that directory, hands Node a
//! port and a loopback bind, and blocks until the child exits. The child is a
//! sibling process, not a reverse-proxied route, so the `/v1` proxy and the
//! UI each own their socket and neither can fail the other.
//!
//! # The launch environment is set here, not inherited blindly
//!
//! The compiled dashboard is a **second LLM gateway** with its own credential
//! store, its own key table, and its own `/v1` surface. Three of its upstream
//! defaults are unsafe the moment the socket is reachable from a reverse
//! proxy rather than from the operator's own terminal:
//!
//! * `REQUIRE_API_KEY` ships **`"false"`** upstream
//!   (`src/shared/constants/featureFlagDefinitions.ts:22`). With it off, the
//!   dashboard's gateway tier admits **anonymous completions** — an
//!   unledgered spend surface over the operator's provider keys, outside
//!   aroute's gate, outside aroute's ledger. The compiled resolver reads
//!   `process.env.REQUIRE_API_KEY` at request time with the precedence
//!   *"DB feature flag > process.env > default"*, and it **fails closed** on
//!   a resolution error. So the env is the whole lever, and the supervisor
//!   sets it.
//! * `AUTH_COOKIE_SECURE` is unset upstream, leaving the cookie's `secure`
//!   flag to depend on `x-forwarded-proto` arriving — true through a
//!   tunnel, but by inference rather than by configuration.
//! * `OMNIROUTE_MEMORY_MB` is unset, so the child sizes its heap from the
//!   host's RAM. On a small box that is a second OOM waiting to happen
//!   next to the proxy.
//!
//! [`LaunchEnv`] therefore **sets** each of these rather than passing the
//! parent's environment through, and it only yields to the parent when the
//! operator has said so explicitly. The precedence, in one place:
//!
//! | variable | default here | parent wins when |
//! |---|---|---|
//! | `REQUIRE_API_KEY` | `true` (fail closed) | the parent sets it to a non-empty value |
//! | `AUTH_COOKIE_SECURE` | `true` | the parent sets it to `false` |
//! | `OMNIROUTE_MEMORY_MB` | `1024` | the parent sets it |
//! | `OMNIROUTE_BASE_PATH` | the dist's baked path | the parent sets it |
//!
//! The one thing the supervisor does **not** do is invent a password or a
//! secret. `INITIAL_PASSWORD`, `JWT_SECRET` and `API_KEY_SECRET` are the
//! operator's to choose; a generated default would be a secret nobody
//! knows, and an unset `JWT_SECRET` is the dashboard's own documented
//! failure mode (the login route 500s at boot). The supervisor's job is to
//! make the *unsafe* defaults safe and to leave the *operator's* choices
//! alone.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;

use anyhow::Context as _;

use crate::cli::DashboardArgs;

/// Names the dist directory when `--path` is absent.
pub const DASHBOARD_DIR_VAR: &str = "AR_DASHBOARD_DIR";

/// The dashboard's own heap ceiling, in MiB, when the operator has not chosen
/// one.
///
/// 1024 is the value `docs/41` §3 measured against the deployment target
/// (2.4 GiB total RAM, 844 MiB available, the proxy resident). A ceiling
/// above the available RAM turns the dashboard into the process the OOM
/// killer picks, and the proxy is the process that must survive.
const DEFAULT_MEMORY_MB: &str = "1024";

/// Where the dashboard keeps its own database and settings when the operator
/// has not pointed `DATA_DIR` elsewhere. Kept out of OmniRoute's default
/// (`~/.omniroute`) on purpose: the rebranded product must not write into a
/// directory owned by the tool it was forked from.
const DEFAULT_DATA_SUBDIR: &str = ".config/ar/dashboard-data";

/// Dist resolution: `--path`, then `$AR_DASHBOARD_DIR`, then a
/// `dashboard/dist` beside the binary, then the `--with-dashboard`
/// install location (`~/.config/ar/dashboard`, where `install.sh` lays the
/// locally built dist down). Falls back to a CWD-relative path so the
/// error message still names something actionable.
fn resolve_dist(flag: Option<PathBuf>, env: Option<PathBuf>, exe_dir: Option<PathBuf>) -> PathBuf {
    resolve_dist_with(flag, env, exe_dir, std::env::var_os("HOME"))
}

/// [`resolve_dist`] with HOME as a parameter, so the whole chain is testable
/// without mutating process env.
///
/// `--path` and `$AR_DASHBOARD_DIR` are authoritative: a caller who names a
/// dist gets exactly that, right or wrong. The two location *guesses* — beside
/// the binary, and the `--with-dashboard` install location — are validated: a
/// guess only wins when it actually contains a `server.js`, so an installed
/// binary finds the installed dist instead of stopping at an empty guess.
fn resolve_dist_with(
    flag: Option<PathBuf>,
    env: Option<PathBuf>,
    exe_dir: Option<PathBuf>,
    home: Option<std::ffi::OsString>,
) -> PathBuf {
    if let Some(explicit) = flag.or(env) {
        return explicit;
    }
    let mut guesses: Vec<PathBuf> = Vec::new();
    if let Some(dir) = exe_dir {
        guesses.push(dir.join("dashboard").join("dist"));
    }
    if let Some(h) = home {
        guesses.push(
            PathBuf::from(h)
                .join(".config")
                .join("ar")
                .join("dashboard"),
        );
    }
    guesses.push(PathBuf::from("dashboard/dist"));
    guesses
        .iter()
        .find(|g| g.join("server.js").is_file())
        .cloned()
        .unwrap_or_else(|| guesses[0].clone())
}

/// The environment the dashboard child is launched with.
///
/// Built once, then applied to the child's `Command` in one place, so the
/// precedence in the module doc is a *property of the type* rather than a
/// convention scattered across `env()` calls. Every override is
/// **defaulting to the safe value**; the parent's environment only wins
/// where the parent has said something deliberate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchEnv {
    vars: BTreeMap<String, String>,
}

impl LaunchEnv {
    /// Builds the launch environment from the parent's.
    ///
    /// `base_path` is the subpath the dist was built for (empty for a
    /// root-built dist), read from the dist's own
    /// `BUILD_OMNIROUTE_BASE_PATH` sentinel when present. It is applied only
    /// when the parent has not chosen one, because a mismatched base path
    /// between the bundle and the runtime is the one thing the standalone
    /// server refuses to start on.
    #[must_use]
    pub fn from_parent(parent: &BTreeMap<String, String>, base_path: &str) -> Self {
        let mut vars = BTreeMap::new();

        // The gateway tier. Upstream's shipped default is "false"; the only
        // safe default for a socket a reverse proxy can reach is "true".
        // An explicit parent value wins — an operator who has decided the
        // dashboard serves its own playground anonymously has said so.
        let require_api_key = match parent.get("REQUIRE_API_KEY") {
            Some(v) if !v.trim().is_empty() => v.clone(),
            _ => "true".to_string(),
        };
        vars.insert("REQUIRE_API_KEY".to_string(), require_api_key);

        // The login cookie's `secure` flag. Set by configuration, not left to
        // depend on `x-forwarded-proto` arriving.
        let cookie_secure = match parent.get("AUTH_COOKIE_SECURE") {
            Some(v) if !v.trim().is_empty() => v.clone(),
            _ => "true".to_string(),
        };
        vars.insert("AUTH_COOKIE_SECURE".to_string(), cookie_secure);

        // The child's heap ceiling, so it cannot out-grow the box the proxy
        // lives on.
        let memory = match parent.get("OMNIROUTE_MEMORY_MB") {
            Some(v) if !v.trim().is_empty() => v.clone(),
            _ => DEFAULT_MEMORY_MB.to_string(),
        };
        vars.insert("OMNIROUTE_MEMORY_MB".to_string(), memory);

        // The subpath. A baked dist already carries it in its manifests, but
        // the server also reads the env at boot, and the two must agree.
        let path = match parent.get("OMNIROUTE_BASE_PATH") {
            Some(v) if !v.trim().is_empty() => v.clone(),
            _ => base_path.to_string(),
        };
        if !path.is_empty() {
            vars.insert("OMNIROUTE_BASE_PATH".to_string(), path.clone());
            vars.insert("NEXT_PUBLIC_OMNIROUTE_BASE_PATH".to_string(), path);
        }

        // The peer-stamp token. Upstream mints one per process
        // (`randomUUID()`), which means every restart invalidates whatever
        // the middleware cached against it — and, more importantly, means
        // the *first-password* window reopens on every restart. A pinned
        // token in the unit's env file closes that window. Left unset, the
        // child's own mint stands, exactly as upstream does it.
        if let Some(token) = parent.get("OMNIROUTE_PEER_STAMP_TOKEN")
            && !token.trim().is_empty()
        {
            vars.insert("OMNIROUTE_PEER_STAMP_TOKEN".to_string(), token.clone());
        }

        // The public origin, when the operator has named one. Redirects and
        // the CSRF origin check read it.
        for key in [
            "OMNIROUTE_PUBLIC_BASE_URL",
            "INITIAL_PASSWORD",
            "JWT_SECRET",
            "API_KEY_SECRET",
            "OMNIROUTE_TRUSTED_PROXIES",
            "OMNIROUTE_TRUST_PROXY",
        ] {
            if let Some(value) = parent.get(key)
                && !value.trim().is_empty()
            {
                vars.insert(key.to_string(), value.clone());
            }
        }

        Self { vars }
    }

    /// The variables to apply to the child.
    #[must_use]
    pub fn vars(&self) -> &BTreeMap<String, String> {
        &self.vars
    }
}

/// Reads the dist's baked base path from its sentinel file.
///
/// Upstream writes `BUILD_OMNIROUTE_BASE_PATH` into the standalone bundle
/// during the build (`scripts/build/write-build-base-path.mjs`), and the
/// runtime patcher compares the runtime env against it. A dist that carries
/// no sentinel was built for the domain root, which is the honest answer
/// when the file is missing.
fn baked_base_path(dist: &std::path::Path) -> String {
    let sentinel = dist.join("BUILD_OMNIROUTE_BASE_PATH");
    let Ok(raw) = std::fs::read_to_string(sentinel) else {
        return String::new();
    };
    raw.trim().to_string()
}

/// The parent's environment as a map, for [`LaunchEnv::from_parent`].
fn parent_env() -> BTreeMap<String, String> {
    std::env::vars().collect()
}

/// Spawns the dashboard server and blocks until it exits.
pub fn run(args: &DashboardArgs) -> anyhow::Result<()> {
    let env = std::env::var_os(DASHBOARD_DIR_VAR).map(PathBuf::from);
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(std::path::Path::to_path_buf));
    let dist = resolve_dist(args.path.clone(), env, exe_dir);

    if !dist.join("server.js").is_file() {
        anyhow::bail!(
            "no dashboard at {} — build it with dashboard/rebrand-dist.sh, then pass --path or set {DASHBOARD_DIR_VAR}",
            dist.display()
        );
    }

    // The peer-stamp launcher must wrap server.js: the compiled UI's authz
    // layer only trusts requests stamped with the real TCP peer, which the
    // bare standalone server never sets — without it every request fails
    // closed to "remote" and onboarding demands the log bootstrap token.
    //
    // A rebuilt dist ships upstream's own `peer-stamp.mjs` next to a
    // `server-ws.mjs` wrapper that installs the stamp for real; that wrapper
    // is preferred when it is present, because it also carries the
    // Cloudflare-edge and trusted-proxy handling the vendored launcher lacks
    // (docs/41 §6, gaps A5/B2/B3).
    let entry = if dist.join("server-ws.mjs").is_file() {
        "server-ws.mjs"
    } else if dist.join("peer-stamp-launcher.cjs").is_file() {
        "peer-stamp-launcher.cjs"
    } else {
        "server.js"
    };

    // stderr, not stdout: the data channel (docs/06) stays clean, matching
    // `serve`'s banner.
    eprintln!(
        "aroute dashboard starting on http://127.0.0.1:{} (dist: {}, entry: {entry})",
        args.port,
        dist.display()
    );

    let launch = LaunchEnv::from_parent(&parent_env(), &baked_base_path(&dist));
    for (key, value) in launch.vars() {
        // The names are ours, not the parent's, and the values are the
        // operator's own configuration — nothing here is a secret, so the
        // banner may name them. The *secrets* live in the env file and are
        // never printed.
        eprintln!("aroute dashboard env: {key}={value}");
    }

    let mut command = Command::new("node");
    command
        .arg(entry)
        .current_dir(&dist)
        // Loopback only: the dashboard manages credentials, and the child
        // would otherwise bind every interface like its parent project does.
        //
        // TWO names, on purpose. `HOSTNAME` is what the bare standalone
        // `server.js` reads. But upstream's `dev/run-standalone.mjs` wrapper
        // recomputes the child's env (`build/runtime-env.mjs`
        // `withRuntimePortEnv`) and OVERWRITES `HOSTNAME` with
        // `OMNIROUTE_HOSTNAME || "0.0.0.0"`, dropping this value — measured
        // 2026-10-22: a unit that set `HOSTNAME=127.0.0.1` produced a
        // `0.0.0.0` listener (ledger F12). `OMNIROUTE_HOSTNAME` is the INPUT
        // that wrapper honours, so pinning it keeps the loopback contract
        // through both entry shapes.
        .env("HOSTNAME", "127.0.0.1")
        .env("OMNIROUTE_HOSTNAME", "127.0.0.1")
        .env("PORT", args.port.to_string())
        .env("DATA_DIR", data_dir());
    for (key, value) in launch.vars() {
        command.env(key, value);
    }
    let mut child = command.spawn().map_err(|e| {
        // `node` absent from PATH is by far the common case for this
        // error; name it rather than letting the raw io Error speak.
        if e.kind() == std::io::ErrorKind::NotFound {
            anyhow::anyhow!("node not found on PATH — the dashboard runtime needs Node.js")
        } else {
            anyhow::Error::new(e).context("could not spawn the dashboard server")
        }
    })?;

    // Same process group by default, so an interactive Ctrl-C reaches both
    // this supervisor and the child; `wait` is what turns the child's exit
    // status into this command's result.
    let status = child
        .wait()
        .context("the dashboard server stopped unexpectedly")?;
    if !status.success() {
        anyhow::bail!("the dashboard server exited with {status}");
    }
    Ok(())
}

/// The child's data directory: the operator's `DATA_DIR` if set, otherwise the
/// aroute-owned default. Passed explicitly either way so the child's own
/// fallback (`~/.omniroute`) never applies.
fn data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("DATA_DIR") {
        return PathBuf::from(dir);
    }
    std::env::var_os("HOME")
        .map(|home| PathBuf::from(home).join(DEFAULT_DATA_SUBDIR))
        // HOME unset (containers, service units without a home): the CWD is
        // the only writable place we know about.
        .unwrap_or_else(|| PathBuf::from("dashboard-data"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    /// An empty parent environment: the case that must still produce a
    /// publish-safe child.
    fn empty_parent() -> BTreeMap<String, String> {
        BTreeMap::new()
    }

    #[test]
    fn launch_env_requires_an_api_key_when_the_parent_says_nothing() {
        let env = LaunchEnv::from_parent(&empty_parent(), "");
        assert_eq!(
            env.vars().get("REQUIRE_API_KEY").map(String::as_str),
            Some("true")
        );
    }

    #[test]
    fn launch_env_keeps_the_parents_explicit_api_key_choice() {
        let mut parent = empty_parent();
        parent.insert("REQUIRE_API_KEY".to_string(), "false".to_string());
        let env = LaunchEnv::from_parent(&parent, "");
        assert_eq!(
            env.vars().get("REQUIRE_API_KEY").map(String::as_str),
            Some("false")
        );
    }

    #[test]
    fn launch_env_treats_a_blank_api_key_as_unset() {
        let mut parent = empty_parent();
        parent.insert("REQUIRE_API_KEY".to_string(), "   ".to_string());
        let env = LaunchEnv::from_parent(&parent, "");
        assert_eq!(
            env.vars().get("REQUIRE_API_KEY").map(String::as_str),
            Some("true")
        );
    }

    #[test]
    fn launch_env_marks_the_login_cookie_secure_by_default() {
        let env = LaunchEnv::from_parent(&empty_parent(), "");
        assert_eq!(
            env.vars().get("AUTH_COOKIE_SECURE").map(String::as_str),
            Some("true")
        );
    }

    #[test]
    fn launch_env_caps_the_child_heap_by_default() {
        let env = LaunchEnv::from_parent(&empty_parent(), "");
        assert_eq!(
            env.vars().get("OMNIROUTE_MEMORY_MB").map(String::as_str),
            Some(DEFAULT_MEMORY_MB)
        );
    }

    #[test]
    fn launch_env_keeps_the_parents_heap_ceiling() {
        let mut parent = empty_parent();
        parent.insert("OMNIROUTE_MEMORY_MB".to_string(), "2048".to_string());
        let env = LaunchEnv::from_parent(&parent, "");
        assert_eq!(
            env.vars().get("OMNIROUTE_MEMORY_MB").map(String::as_str),
            Some("2048")
        );
    }

    #[test]
    fn launch_env_adopts_the_baked_base_path_when_the_parent_has_none() {
        let env = LaunchEnv::from_parent(&empty_parent(), "/dash");
        assert_eq!(
            env.vars().get("OMNIROUTE_BASE_PATH").map(String::as_str),
            Some("/dash")
        );
    }

    #[test]
    fn launch_env_mirrors_the_base_path_into_the_client_visible_name() {
        let env = LaunchEnv::from_parent(&empty_parent(), "/dash");
        assert_eq!(
            env.vars()
                .get("NEXT_PUBLIC_OMNIROUTE_BASE_PATH")
                .map(String::as_str),
            Some("/dash")
        );
    }

    #[test]
    fn launch_env_omits_the_base_path_entirely_for_a_root_built_dist() {
        let env = LaunchEnv::from_parent(&empty_parent(), "");
        assert!(!env.vars().contains_key("OMNIROUTE_BASE_PATH"));
    }

    #[test]
    fn launch_env_carries_the_pinned_peer_stamp_token_when_present() {
        let mut parent = empty_parent();
        parent.insert(
            "OMNIROUTE_PEER_STAMP_TOKEN".to_string(),
            "pinned-token".to_string(),
        );
        let env = LaunchEnv::from_parent(&parent, "");
        assert_eq!(
            env.vars()
                .get("OMNIROUTE_PEER_STAMP_TOKEN")
                .map(String::as_str),
            Some("pinned-token")
        );
    }

    #[test]
    fn launch_env_does_not_invent_a_peer_stamp_token() {
        let env = LaunchEnv::from_parent(&empty_parent(), "");
        assert!(!env.vars().contains_key("OMNIROUTE_PEER_STAMP_TOKEN"));
    }

    #[test]
    fn launch_env_carries_the_operators_secrets_without_printing_them() {
        let mut parent = empty_parent();
        parent.insert("INITIAL_PASSWORD".to_string(), "s3cret".to_string());
        parent.insert("JWT_SECRET".to_string(), "jwt".to_string());
        let env = LaunchEnv::from_parent(&parent, "");
        assert_eq!(
            env.vars().get("INITIAL_PASSWORD").map(String::as_str),
            Some("s3cret")
        );
        assert_eq!(
            env.vars().get("JWT_SECRET").map(String::as_str),
            Some("jwt")
        );
    }

    #[test]
    fn launch_env_does_not_invent_a_password_or_a_secret() {
        let env = LaunchEnv::from_parent(&empty_parent(), "");
        assert!(!env.vars().contains_key("INITIAL_PASSWORD"));
        assert!(!env.vars().contains_key("JWT_SECRET"));
        assert!(!env.vars().contains_key("API_KEY_SECRET"));
    }

    #[test]
    fn launch_env_ignores_unrelated_parent_variables() {
        let mut parent = empty_parent();
        parent.insert("AR_KEY_OPENAI".to_string(), "sk-secret".to_string());
        let env = LaunchEnv::from_parent(&parent, "");
        assert!(!env.vars().contains_key("AR_KEY_OPENAI"));
    }

    #[test]
    fn baked_base_path_reads_the_dist_sentinel() {
        let root = std::env::temp_dir().join(format!("ar-dash-sentinel-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("BUILD_OMNIROUTE_BASE_PATH"), "/dash\n").unwrap();
        assert_eq!(baked_base_path(&root), "/dash");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn baked_base_path_is_empty_for_a_dist_without_the_sentinel() {
        let root = std::env::temp_dir().join(format!("ar-dash-nosentinel-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        assert_eq!(baked_base_path(&root), "");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn resolve_dist_prefers_the_flag_when_all_three_are_set() {
        let got = resolve_dist(Some(p("/flag")), Some(p("/env")), Some(p("/exe")));
        assert_eq!(got, p("/flag"));
    }

    #[test]
    fn resolve_dist_falls_back_to_the_env_var_when_flag_missing() {
        let got = resolve_dist(None, Some(p("/env")), Some(p("/exe")));
        assert_eq!(got, p("/env"));
    }

    /// A temp dir with `server.js` inside, so an existence-validated guess has
    /// something to find. The caller removes the dir.
    fn dist_dir(tag: &str) -> (PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!("ar-dash-guess-{tag}-{}", std::process::id()));
        let dist = root.join("dashboard").join("dist");
        std::fs::create_dir_all(&dist).unwrap();
        std::fs::write(dist.join("server.js"), b"// next standalone").unwrap();
        (root, dist)
    }

    #[test]
    fn resolve_dist_looks_beside_the_binary_when_only_exe_dir_known() {
        let (root, dist) = dist_dir("beside");
        // exe_dir IS the dir the binary sits in, so its guess is
        // <exe_dir>/dashboard/dist — exactly what dist_dir created under root.
        let got = resolve_dist_with(
            None,
            None,
            Some(root.clone()),
            Some(std::ffi::OsString::from("/nonexistent-home")),
        );
        assert_eq!(got, dist);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn resolve_dist_names_a_cwd_relative_path_when_nothing_is_known() {
        let got = resolve_dist_with(None, None, None, None);
        assert_eq!(got, p("dashboard/dist"));
    }

    #[test]
    fn resolve_dist_finds_the_installer_location_when_home_is_known() {
        // No flag, no env, no dist beside the binary: the --with-dashboard
        // install location is what `aroute dashboard` serves.
        let (root, _unused) = dist_dir("installed");
        let home = root.join("home");
        let installed = home.join(".config").join("ar").join("dashboard");
        std::fs::create_dir_all(&installed).unwrap();
        std::fs::write(installed.join("server.js"), b"// next standalone").unwrap();
        let got = resolve_dist_with(
            None,
            None,
            Some(root.join("bin")),
            Some(home.into_os_string()),
        );
        assert_eq!(
            got, installed,
            "an installed binary must find the installed dist"
        );
        // A dist beside the binary still outranks the install location.
        let (beside_root, beside_dist) = dist_dir("beside-wins");
        let beside_home = beside_root.join("home");
        let beside_installed = beside_home.join(".config").join("ar").join("dashboard");
        std::fs::create_dir_all(&beside_installed).unwrap();
        std::fs::write(beside_installed.join("server.js"), b"// next standalone").unwrap();
        let beside = resolve_dist_with(
            None,
            None,
            Some(beside_root.clone()),
            Some(beside_home.into_os_string()),
        );
        assert_eq!(beside, beside_dist);
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&beside_root);
    }

    #[test]
    fn resolve_dist_names_the_first_guess_when_none_exist() {
        // All guesses missing: the error message still names the most likely
        // intended location rather than a bare relative path.
        let got = resolve_dist_with(
            None,
            None,
            Some(p("/opt/bin")),
            Some(std::ffi::OsString::from("/nonexistent-home")),
        );
        assert_eq!(got, p("/opt/bin/dashboard/dist"));
    }
}
