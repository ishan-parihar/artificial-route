//! `aroute dashboard` — spawn the bundled web UI server.
//!
//! The dashboard ships as a *compiled* Next.js standalone tree (see
//! `dashboard/rebrand-dist.sh`): `server.js`, its traced `node_modules`, and
//! the built `.next` output — no TypeScript, no build tooling at runtime.
//! This verb is only the supervisor: it resolves that directory, hands Node a
//! port and a loopback bind, and blocks until the child exits. The child is a
//! sibling process, not a reverse-proxied route, so the `/v1` proxy and the
//! UI each own their socket and neither can fail the other.

use std::path::PathBuf;
use std::process::Command;

use anyhow::Context as _;

use crate::cli::DashboardArgs;

/// Names the dist directory when `--path` is absent.
pub const DASHBOARD_DIR_VAR: &str = "AR_DASHBOARD_DIR";

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
fn resolve_dist_with(
    flag: Option<PathBuf>,
    env: Option<PathBuf>,
    exe_dir: Option<PathBuf>,
    home: Option<std::ffi::OsString>,
) -> PathBuf {
    flag.or(env)
        .or_else(|| exe_dir.map(|d| d.join("dashboard").join("dist")))
        .or_else(|| {
            home.map(|h| {
                PathBuf::from(h)
                    .join(".config")
                    .join("ar")
                    .join("dashboard")
            })
        })
        .unwrap_or_else(|| PathBuf::from("dashboard/dist"))
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
    let entry = if dist.join("peer-stamp-launcher.cjs").is_file() {
        "peer-stamp-launcher.cjs"
    } else {
        "server.js"
    };

    // stderr, not stdout: the data channel (docs/06) stays clean, matching
    // `serve`'s banner.
    eprintln!(
        "aroute dashboard starting on http://127.0.0.1:{} (dist: {})",
        args.port,
        dist.display()
    );

    let mut child = Command::new("node")
        .arg(entry)
        .current_dir(&dist)
        // Loopback only: the dashboard manages credentials, and the child
        // would otherwise bind every interface like its parent project does.
        .env("HOSTNAME", "127.0.0.1")
        .env("PORT", args.port.to_string())
        .env("DATA_DIR", data_dir())
        .spawn()
        .map_err(|e| {
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

    #[test]
    fn resolve_dist_looks_beside_the_binary_when_only_exe_dir_known() {
        let got = resolve_dist(None, None, Some(p("/usr/local/bin")));
        assert_eq!(got, p("/usr/local/bin/dashboard/dist"));
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
        let got = resolve_dist_with(None, None, None, Some(std::ffi::OsString::from("/home/op")));
        assert_eq!(got, p("/home/op/.config/ar/dashboard"));
        // A dist beside the binary still outranks the install location.
        let beside = resolve_dist_with(
            None,
            None,
            Some(p("/opt/bin")),
            Some(std::ffi::OsString::from("/home/op")),
        );
        assert_eq!(beside, p("/opt/bin/dashboard/dist"));
    }
}
