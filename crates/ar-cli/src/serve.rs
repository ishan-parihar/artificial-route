//! The two verbs that touch the network: `aroute serve` and `aroute run`.
//!
//! Both build the *same* `ar_server::Components` from the File-mode config and
//! hand it to `ar_server::server()`. `aroute run` then drives that router in-process
//! through `oneshot` rather than opening a socket, which is what makes it a
//! genuine single-shot completion: identical translation, identical attempt
//! loop, identical `x-ar-*` decision headers, no second code path to drift.
//!
//! The combo table and the price table are `ar-server`'s to shape
//! (`ServerConfig::from_ar_config`), so this file only supplies what the YAML
//! does not carry: the port override and the prices, which come from the
//! compiled-in registry rather than from a config the user hand-edits.

use ar_config::Config;
use ar_route::ArExec;

use ar_server::{Components, HttpExec};
use ar_tokens::PricingTable;

use crate::cli::{Cli, RunArgs, ServeArgs};
use crate::commands;
use crate::toon;

/// Names the directory the observability write path uses: `ar-trace-*.jsonl`
/// and `audit.redb`, side by side.
pub const OBS_DIR_VAR: &str = "AR_OBS_DIR";

/// Builds the server components from File-mode config.
///
/// `want = None` serves every configured combo; the listener resolves the
/// client's `model` against the combo ids. Failures name the exact thing that is
/// wrong — a target whose provider is not in the registry fails here rather than
/// as a 502 three layers down.
fn components(
    cli: &Cli,
    cfg: &Config,
    port: Option<u16>,
    want: Option<&str>,
) -> anyhow::Result<Components> {
    if let Some(id) = want
        && !cfg.combos.iter().any(|c| c.id == id)
    {
        return Err(commands::fail(
            format!("no combo named {id:?} is configured"),
            "use --model with one of: `aroute combo --fields id` lists them",
        ));
    }
    if cfg.combos.is_empty() {
        return Err(commands::fail(
            "no combo is configured, so there is nothing to serve",
            "add a `combos:` entry with at least one target, then run `aroute doctor`",
        ));
    }

    // Held rather than opened inline: `from_ar_config` resolves provider
    // credentials through it, and the gate master is the same store's
    // `http-gate` row — one connection, one source of truth.
    let store = commands::credential_store(cli);

    // Loopback only: P0 has no credential gate, so a routable bind would publish
    // an unauthenticated LLM proxy. `ar-server::bind_addr` enforces the same
    // rule for the socket this hands to.
    let config = ar_server::ServerConfig::from_ar_config(
        cfg,
        port,
        Some(PricingTable::global()),
        false,
        store.as_ref(),
    )
    .map_err(|e| {
        commands::fail(
            e,
            "run `aroute doctor`; a target whose provider is not in the compiled-in registry cannot be dispatched to",
        )
    })?;

    // `HttpExec::new` takes the list by value and `ServerConfig` needs its own
    // copy; one clone of a handful of four-field structs at boot is cheaper than
    // the seam it would take to share them.
    let exec = HttpExec::new(config.providers.clone())
        .map_err(|e| {
            commands::fail(
                e,
                "the HTTP client could not be built; check the TLS backend",
            )
        })
        .map(|e| std::sync::Arc::new(e) as std::sync::Arc<dyn ArExec>)?;

    // The usage ledger lives next to the credential store by construction, so
    // `serve` writing rows and `aroute mcp`'s `cost_report`/`check_quota` reading
    // them describe the same file without two derivations agreeing on a path.
    // An open failure demotes to header-only accounting: the response headers
    // are computed either way, and a proxy that refuses to boot because
    // bookkeeping could not open a file is a worse failure than a warned one.
    let ledger_path = commands::store_path(&cli.config).parent().map_or_else(
        || std::path::PathBuf::from("usage.sqlite"),
        |dir| dir.join("usage.sqlite"),
    );
    let ledger = match ar_tokens::Ledger::open(&ledger_path) {
        Ok(ledger) => Some(std::sync::Arc::new(std::sync::Mutex::new(ledger))),
        Err(e) => {
            eprintln!(
                "ar: usage accounting not persisted — could not open {}: {e}",
                ledger_path.display()
            );
            None
        }
    };

    // The observability write path: trace files plus the audit ledger, in one
    // directory the operator names. Off unless asked, like every other
    // persistence surface here — `/metrics` needs no directory and keeps
    // rendering either way.
    let obs_dir = std::env::var_os(OBS_DIR_VAR).map(std::path::PathBuf::from);

    // docs/15 phase 3: the master key `aroute keys arm-gate` stored, read at
    // boot so the shipped serve path arms the gate the library always had.
    // `Components.master_key` wins over `server.http_master_key` in
    // `into_state`, so an armed store row beats any stray env-derived key.
    // A row that exists but will not decrypt is a warning and an unarmed
    // gate: a proxy that refuses to boot because one credential is stale
    // would strand every credential that is fine (the same demotion
    // `credential_store` itself makes).
    let master_key = store.as_ref().and_then(|s| {
        commands::gate_master(s)
            .map_err(|e| {
                eprintln!("ar: gate row present but unreadable — serving ungated: {e}");
            })
            .ok()
            .flatten()
            .map(|secret| secret.as_bytes().to_vec())
    });

    Ok(Components {
        master_key,
        ledger,
        obs_dir,
        ..Components::with_exec(config, exec)
    })
}

/// Boots the listener and blocks until interrupted.
pub async fn serve(cli: &Cli, args: &ServeArgs) -> anyhow::Result<()> {
    let cfg = commands::load(cli)?;
    let server = ar_server::server(components(cli, &cfg, args.port, None)?);
    let count = server.state.config.providers.len();
    let port = server.state.config.port;
    let addr = ar_server::app::bind_addr(&cfg.server.host, port, false).map_err(|e| {
        commands::fail(
            e,
            "set `server.host` to a loopback address; P0 has no credential gate, so a routable bind would publish an unauthenticated proxy",
        )
    })?;

    let listener = tokio::net::TcpListener::bind(addr).await.map_err(|e| {
        commands::fail(
            format!("cannot bind {addr}: {e}"),
            "another process may hold the port; pass --port <N>",
        )
    })?;

    // stderr, not stdout: stdout is the data channel (`docs/06`), and a banner
    // there would sit in front of whatever a caller pipes out of this proxy.
    eprintln!(
        "{} {} listening on http://{addr} ({count} provider(s), {} combo(s))",
        crate::version::BIN,
        crate::version::VERSION,
        server.state.config.combos.len()
    );

    // Started after the bind, so a boot that cannot reach models.dev has already
    // produced a listening socket and a diagnostic: the proxy is up and serving
    // its configured models while discovery warms. A failure inside the loop is
    // logged and retried on the next tick, never fatal — see
    // `ar_server::app::Server::spawn_discovery_refresh`.
    let discovery_task = server.spawn_discovery_refresh();
    if discovery_task.is_some() {
        eprintln!("model discovery armed ({})", ar_server::DISCOVERY_VAR);
    }
    if server.state.obs.is_some() {
        let dir = std::env::var(OBS_DIR_VAR).unwrap_or_default();
        eprintln!("observability write path armed ({OBS_DIR_VAR}={dir})");
    }

    axum::serve(listener, server.router)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(|e| commands::fail(e, "the listener stopped unexpectedly"))?;

    // The loop outlives the listener otherwise: it is an infinite interval task,
    // so the process would not return after a clean shutdown without this.
    if let Some(task) = discovery_task {
        task.abort();
    }
    Ok(())
}

/// Resolves on SIGINT/SIGTERM so a Ctrl-C drains rather than cutting a streamed
/// answer in half.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {}
        () = terminate => {}
    }
}

/// One completion, printed to stdout.
///
/// The routing verdict goes to stdout too: for an agent, which provider served
/// the answer is the first thing it wants, and burying it in a header the caller
/// would have to remember to print is how it gets lost.
pub async fn run(cli: &Cli, args: &RunArgs) -> anyhow::Result<()> {
    let cfg = commands::load(cli)?;
    let model = match args.model.as_deref() {
        Some(id) => {
            anyhow::ensure!(
                cfg.combos.iter().any(|c| c.id == id),
                "{}",
                commands::fail(
                    format!("no combo named {id:?} is configured"),
                    "use --model with one of: `aroute combo --fields id` lists them",
                )
            );
            id.to_owned()
        }
        None => cfg.combos.first().map(|c| c.id.clone()).ok_or_else(|| {
            commands::fail(
                "no combo is configured",
                "pass --model <ID>, or add a combo to the config",
            )
        })?,
    };

    let server = ar_server::server(components(cli, &cfg, None, Some(&model))?);
    let payload = serde_json::json!({
        "model": model,
        "stream": false,
        "messages": [{ "role": "user", "content": args.prompt }],
    });
    let request = axum::http::Request::builder()
        .method(axum::http::Method::POST)
        .uri("/v1/chat/completions")
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .body(axum::body::Body::from(payload.to_string()))
        .map_err(|e| {
            commands::fail(
                format!("cannot build the request: {e}"),
                "this is a bug in `aroute run`",
            )
        })?;

    let response = tower::ServiceExt::oneshot(server.router, request).await.map_err(|e| {
        commands::fail(e, "the router failed mid-request; `aroute serve` on a port will show the same failure with logs")
    })?;
    let status = response.status();
    let decision = response
        .headers()
        .get(ar_server::routes::DECISION_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("-")
        .to_owned();
    let body = http_body_util::BodyExt::collect(response.into_body())
        .await
        .map_err(|e| commands::fail(e, "the upstream body ended early; retry the request"))?
        .to_bytes();

    println!("decision: {decision}");
    println!();
    print!("{}", toon::body(&String::from_utf8_lossy(&body), args.full));

    if !status.is_success() {
        return Err(commands::fail(
            format!("upstream returned {status}"),
            "the body above is the provider's own answer; `aroute doctor` checks config, keys and registry",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicU64;

    use ar_route::{ProviderId, Strategy};

    use super::*;

    /// The sample config's `cheap` combo: two openai tiers, both priced, the
    /// nano an order of magnitude under the flagship.
    const CHEAP: &str = "keys:\n  k: v\nproviders:\n  - id: openai\n    key: k\ncombos:\n  - id: cheap\n    strategy: cost-optimized\n    targets:\n      - openai/gpt-5.4\n      - openai/gpt-5.4-nano\n";

    fn candidates_for(yaml: &str, combo: &str) -> Vec<ar_route::Candidate> {
        let cfg = Config::parse(yaml, |_| Ok(Some("v".to_owned()))).expect("the fixture parses");
        let config = ar_server::ServerConfig::from_ar_config(
            &cfg,
            None,
            Some(PricingTable::global()),
            false,
            None,
        )
        .expect("every target is in the registry");
        let combo = config.combo(combo).expect("the combo exists").clone();
        config.candidates(Some(&combo))
    }

    #[test]
    fn picks_cheapest_priced_target_for_cost_optimized() {
        let candidates = candidates_for(CHEAP, "cheap");
        let picked = ar_route::pick(
            Strategy::CostOptimized,
            None,
            &candidates,
            &AtomicU64::new(0),
            None,
        )
        .expect("two candidates, one pick");
        assert_eq!(
            picked,
            ProviderId::new("openai"),
            "both tiers sit on one provider"
        );

        let chosen = candidates
            .iter()
            .find(|c| c.model.as_ref() == "gpt-5.4-nano")
            .expect("nano is a candidate");
        let rejected = candidates
            .iter()
            .find(|c| c.model.as_ref() == "gpt-5.4")
            .expect("the flagship is a candidate");
        let cheap = chosen.input_usd_per_mtok.expect("the nano tier is priced");
        let pricey = rejected.input_usd_per_mtok.expect("the flagship is priced");
        assert!(
            cheap < pricey,
            "the cheaper tier must be the one cost-optimized prefers: {cheap} vs {pricey}"
        );
    }

    #[test]
    fn leaves_a_target_unpriced_when_the_catalog_has_no_row() {
        let candidates = candidates_for(
            "keys:\n  k: v\nproviders:\n  - id: openai\n    key: k\ncombos:\n  - id: c\n    strategy: cost-optimized\n    targets:\n      - openai/gpt-5.4\n      - openai/gpt-5.5\n",
            "c",
        );
        let priced: Vec<&str> = candidates
            .iter()
            .filter(|c| c.input_usd_per_mtok.is_some())
            .map(|c| c.model.as_ref())
            .collect();
        assert_eq!(
            priced,
            vec!["gpt-5.4"],
            "a model with no catalog row stays unpriced: {candidates:?}"
        );
    }
}
