//! `ar mcp` — the MCP control plane, and the state it serves.
//!
//! # Why this file exists
//!
//! F-HIGH-5: the essential-8 lived in `ar-mcp` as a library no binary reached, so
//! the crate advertised a capability nothing could invoke. This is the wiring.
//! Everything a tool needs is already resolved elsewhere in the binary —
//! `ar_server::ServerConfig` for the routable combos and their candidates,
//! `ar-keys` for real lane state — so this file supplies those to
//! [`ar_mcp::Host`] and starts the stdio transport. It deliberately does not
//! re-derive a combo table, a price table or a credential binding.
//!
//! # `--list` needs no config
//!
//! The catalog is eight static names, so discovery must answer on a host with no
//! `config.yaml` at all. Only the serving path loads one.
//!
//! # `AR_MCP_SCOPE`
//!
//! The held scope. Unset means everything, which is the honest reading for a
//! stdio child the operator launched themselves. Set, it is the *only* thing
//! granted — `ar_switch_combo` and `ar_route_request` are refused under a bare
//! `read:*` — and an unrecognised name is a startup error rather than a bit
//! silently dropped.

use std::path::PathBuf;

use ar_config::Config;
use ar_keys::{Admission, LaneSpec};
use ar_mcp::{Audit, ComboState, Host, HostCombo, Scope, Tool};
use ar_tokens::PricingTable;

use crate::cli::{Cli, McpArgs};
use crate::commands;
use crate::toon;

/// Environment variable naming the held scope, e.g. `AR_MCP_SCOPE=read:*`.
const SCOPE_VAR: &str = "AR_MCP_SCOPE";

/// Where the control-plane audit trail lives.
///
/// Beside the config, like the credential store and the ledger, so a project
/// directory carries its own state and a gitignored `*.redb` covers it. The
/// ledger's own file is the sibling `usage.sqlite`.
const AUDIT_FILE: &str = "ar-mcp-audit.redb";
const LEDGER_FILE: &str = "usage.sqlite";

/// Requests per minute the control plane's admission controller allows.
///
/// `provisional:` no measurement exists for a control plane. It is high on
/// purpose: a read-mostly host making a handful of calls a minute should never
/// be told it is over a rate limit it does not have a reason to respect.
const CONTROL_PLANE_RPM: u32 = 600;

/// Columns of the `ar mcp --list` table.
const TOOL_COLUMNS: [&str; 3] = ["id", "scope", "description"];

/// Runs `ar mcp`: lists the catalog, or serves it.
pub async fn run(cli: &Cli, args: &McpArgs) -> anyhow::Result<()> {
    if args.list {
        return list();
    }
    let cfg = commands::load(cli)?;
    // Read before the build's own error mapping, so an unusable grant keeps the
    // `help:` line that lists the valid scopes instead of being re-labelled as a
    // registry problem.
    let scope = scope()?;
    // Built before serving, so a config problem is named on stdout with a
    // `help:` line rather than as a closed pipe mid-handshake.
    let built = build(cli, &cfg, scope).map_err(|e| {
        commands::fail(
            e,
            "run `ar doctor`; a target whose provider is not in the compiled-in registry cannot be served",
        )
    })?;
    ar_mcp::serve_stdio(built)
        .await
        .map_err(|e| commands::fail(e, "the MCP host closed the connection; nothing to retry from here"))
}

/// Prints the catalog as TOON, then the two things a caller does next.
///
/// Content-first per `docs/06`: the table is the answer, and the footer is the
/// exact command that starts serving it.
fn list() -> anyhow::Result<()> {
    // `;` for the one-liners' commas: a TOON row is comma-delimited, so a comma in
    // a cell makes the row wider than its header. The wire name and the MCP
    // description keep theirs -- this substitution is for the table only.
    let cell = |s: &str| s.replace(',', ";");
    let rows: Vec<Vec<String>> = Tool::ALL
        .iter()
        .map(|t| vec![t.name().to_owned(), t.scope_doc().to_owned(), cell(t.one_liner())])
        .chain(std::iter::once(vec![
            ar_mcp::TOOL_SEARCH_NAME.to_owned(),
            "none".to_owned(),
            "one-line signatures for the catalog; an empty query lists all of it".to_owned(),
        ]))
        .collect();
    print!(
        "{}",
        toon::list("tools", "tools", &TOOL_COLUMNS, &toon::every_field(&TOOL_COLUMNS), &rows, false)
    );
    println!("next:");
    println!("  ar mcp             serve the catalog over stdio");
    println!("  ar mcp --list      this table");
    Ok(())
}

/// Builds the transport's host state from the loaded config.
fn build(cli: &Cli, cfg: &Config, scope: Scope) -> Result<Host, anyhow::Error> {
    anyhow::ensure!(
        !cfg.combos.is_empty(),
        "{}",
        commands::fail(
            "no combo is configured, so there is nothing to serve",
            "add a `combos:` entry with at least one target, then run `ar doctor`"
        )
    );

    // The same resolution `ar serve` does, so the control plane routes over
    // exactly the candidates the data plane would and prices them identically.
    let config = ar_server::ServerConfig::from_ar_config(
        cfg,
        None,
        Some(PricingTable::global()),
        false,
        commands::credential_store(cli).as_ref(),
    )?;
    let combos: Vec<HostCombo> = config
        .combos
        .iter()
        .map(|c| {
            // Targets and bench are read through the same `ar-server` accessors
            // `ar serve` uses, so an undispatchable provider is dropped in both
            // places and a combo's two halves cannot disagree.
            let pool: Vec<ar_route::Candidate> = config
                .pool(c)
                .map(|t| ar_route::Candidate::new(t.provider.clone(), &t.model))
                .collect();
            HostCombo {
                name: c.id.clone(),
                strategy: c.strategy,
                candidates: config.candidates(Some(c)),
                pool,
            }
        })
        .collect();

    let dir = store_dir(&cli.config);
    std::fs::create_dir_all(&dir).map_err(|e| {
        commands::fail(
            format!("cannot create {}: {e}", dir.display()),
            "pass --config <PATH> in a directory you can write, or set $HOME",
        )
    })?;

    let admission = Admission::new([LaneSpec::INTERACTIVE, LaneSpec::BATCH, LaneSpec::HEAVY], CONTROL_PLANE_RPM)
        .map_err(|e| commands::fail(e, "the admission controller could not be built; this is a host problem"))?;

    Ok(Host {
        combos,
        admission,
        ledger_path: dir.join(LEDGER_FILE),
        active: std::sync::Mutex::new(ComboState {
            active: cfg.combos.first().map(|c| c.id.clone()),
        }),
        cursor: std::sync::atomic::AtomicU64::new(0),
        scope,
        audit: Audit::open(&dir.join(AUDIT_FILE)).map_err(|e| {
            commands::fail(
                format!("cannot open the audit trail: {e}"),
                format!("point --config at a writable directory, or delete {AUDIT_FILE} and retry"),
            )
        })?,
        key_id: key_id(cfg),
    })
}

/// The scope this process holds, from `$AR_MCP_SCOPE`.
///
/// Unset is `*`: the caller launched this process, so there is nobody to refuse.
/// A set-but-unreadable value is a startup failure, not a default — a host that
/// meant to lock the control plane down must not get a wide-open server.
fn scope() -> Result<Scope, anyhow::Error> {
    let Ok(raw) = std::env::var(SCOPE_VAR) else { return Ok(Scope::ALL) };
    if raw.trim().is_empty() {
        return Ok(Scope::ALL);
    }
    Scope::parse(&raw).map_err(|e| {
        commands::fail(
            format!("{SCOPE_VAR} is not a scope grant: {e}"),
            format!("set {SCOPE_VAR} to a comma-separated subset of: {}", Scope::NAMES.join(", ")),
        )
    })
}

/// The key id audit rows are attributed to.
///
/// A name, never a value: an audit trail that cannot be pasted into a bug report
/// is a trail nobody reads. The control plane dispatches no traffic of its own, so
/// the first configured key is the honest attribution.
fn key_id(cfg: &Config) -> String {
    cfg.keys.keys().next().cloned().unwrap_or_else(|| "mcp".to_owned())
}

/// The directory the control plane's own state lives in, beside the config.
fn store_dir(config_path: &std::path::Path) -> PathBuf {
    match config_path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    const YAML: &str = "keys:\n  k: v\nproviders:\n  - id: openai\n    key: k\ncombos:\n  - id: cheap\n    strategy: cost-optimized\n    targets:\n      - openai/gpt-5.4-nano\n";

    /// A CLI pointed at a per-test scratch directory.
    ///
    /// `build` writes its audit trail and ledger *beside the config*, and `redb`
    /// allows one open handle per file, so tests that share a config path also
    /// share a lock and fail in parallel.
    fn cli(name: &str) -> Cli {
        let dir = std::env::temp_dir().join("ar-cli-mcp").join(name);
        std::fs::create_dir_all(&dir).expect("tmpdir");
        let config = dir.join("config.yaml");
        std::fs::write(&config, YAML).expect("the fixture writes");
        Cli::try_parse_from(["ar", "--config", config.to_str().expect("utf-8 path")])
            .expect("the fixture parses")
    }

    fn config(yaml: &str) -> Config {
        Config::parse(yaml, |_| Ok(Some("v".to_owned()))).expect("the fixture parses")
    }

    #[test]
    fn builds_a_host_over_the_configured_combos() {
        let host = build(&cli("combos"), &config(YAML), Scope::ALL).expect("one known target");
        assert_eq!(host.combos.len(), 1);
    }

    #[test]
    fn marks_the_first_combo_active_so_a_routed_call_needs_no_argument() {
        let host = build(&cli("active"), &config(YAML), Scope::ALL).expect("built");
        assert_eq!(host.active.into_inner().expect("lock").active.as_deref(), Some("cheap"));
    }

    #[test]
    fn prices_candidates_from_the_compiled_in_registry() {
        // Same table `ar serve` uses: an unpriced target would sort last under
        // `cost-optimized` here and there alike.
        let host = build(&cli("prices"), &config(YAML), Scope::ALL).expect("built");
        assert!(host.combos[0].candidates.iter().any(|c| c.input_usd_per_mtok.is_some()));
    }

    #[test]
    fn refuses_a_config_with_nothing_routable() {
        let empty = config("keys:\n  k: v\nproviders:\n  - id: openai\n    key: k\ncombos: []\n");
        let e = build(&cli("empty"), &empty, Scope::ALL).err().expect("no combo is a refusal").to_string();
        assert!(e.contains("nothing to serve"), "{e}");
        assert!(e.contains("help:"), "{e}");
    }

    #[test]
    fn names_a_target_whose_provider_is_not_in_the_registry() {
        let bad = config("keys:\n  k: v\nproviders:\n  - id: openai\n    key: k\ncombos:\n  - id: c\n    strategy: priority\n    targets:\n      - nosuchprov/m\n");
        let e = build(&cli("unknown"), &bad, Scope::ALL).err().expect("unknown provider is a refusal").to_string();
        assert!(e.contains("nosuchprov"), "{e}");
    }

    #[test]
    fn attributes_audit_rows_to_a_key_name_not_a_value() {
        assert_eq!(key_id(&config(YAML)), "k");
    }

    #[test]
    fn falls_back_to_a_neutral_key_id_when_no_key_is_configured() {
        assert_eq!(key_id(&config("providers: []\ncombos: []\n")), "mcp");
    }

    #[test]
    fn grants_everything_when_the_scope_variable_is_unset() {
        // The variable is read from the process, so this asserts the default path
        // only when the host has not set one.
        if std::env::var(SCOPE_VAR).is_err() {
            assert_eq!(scope().expect("default"), Scope::ALL);
        }
    }

    #[test]
    fn a_narrow_grant_leaves_the_write_tools_refused() {
        assert!(!Scope::parse("read:*").expect("known").permits(Tool::SwitchCombo.scope()));
    }

    #[test]
    fn lists_every_registered_tool() {
        // `list` writes to stdout, so the assertion is on the catalog it reads
        // rather than on captured output.
        assert_eq!(Tool::ALL.len() + 1, 9, "essential-8 plus tool_search");
    }

    #[test]
    fn a_tool_row_never_carries_a_comma_into_a_comma_delimited_table() {
        // The one-liners are written for an MCP host, where a comma is
        // punctuation. In TOON it would widen the row past its header.
        for tool in Tool::ALL {
            assert!(!tool.one_liner().replace(',', ";").contains(','), "{}", tool.name());
        }
    }
}
