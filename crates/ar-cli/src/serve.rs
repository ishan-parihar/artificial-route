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

use std::sync::Arc;
use std::time::Duration;

use ar_config::Config;
use ar_route::ArExec;

use ar_server::{Components, HttpExec};
use ar_tokens::PricingTable;

use crate::cli::{Cli, RunArgs, ServeArgs};
use crate::commands;
use crate::toon;
use crate::webui;

/// Names the directory the observability write path uses: `ar-trace-*.jsonl`
/// and `audit.redb`, side by side.
pub const OBS_DIR_VAR: &str = "AR_OBS_DIR";

/// The panel's gate state, resolved by reading the store's gate row.
///
/// Three arms, and the arm is decided by *decrypting* rather than by finding
/// the row's name:
///
/// * **Unarmed** — no store, or no gate row in it. The socket's loopback bind
///   is the trust boundary, exactly as it is for `/v1` on the same build.
/// * **Armed** — the row decrypts. The panel's API then requires a token.
/// * **Broken** — the row exists and will not decrypt. The server's own boot
///   log already says "serving ungated", so the operator believes the gate
///   is armed while it is not. The panel refuses its writes in that state.
///
/// The three-arm split is the point: "the store is missing" and "the store
/// cannot be opened" are different problems, and collapsing them into
/// "unarmed" would silently reopen the panel on a box whose operator
/// believes it is closed.
///
/// The live gate that implements this is [`webui::livegate::LiveGate`].
///
/// Builds the server components from File-mode config.
///
/// `want = None` serves every configured combo; the listener resolves the
/// client's `model` against the combo ids. Failures name the exact thing that is
/// wrong — a target whose provider is not in the registry fails here rather than
/// as a 502 three layers down.
///
/// Returns the components **and** the revocation list, because the list has to
/// be shared: the server's `/v1` gate and the control panel's gate must
/// consult the same handle, or they are two sqlite connections with two
/// independent read snapshots and can disagree about a just-revoked `jti`.
fn components(
    cli: &Cli,
    cfg: &Config,
    port: Option<u16>,
    want: Option<&str>,
) -> anyhow::Result<(Components, Option<Arc<ar_keys::Revocations>>)> {
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

    // The revocation table, in the credential store's own file.
    //
    // It is a **sqlite** table, not a separate file, and that is the whole
    // point: the running proxy holds this handle open for the life of the
    // process, and `aroute keys revoke` has to open the same file while it
    // runs — because that is when a token needs revoking. A redb file could
    // not be opened twice (redb takes an exclusive lock), so the operator's
    // revoke command would fail for exactly as long as the proxy was up.
    //
    // The store is **probed first**, and the probe decides. `sqlite` creates a
    // file it opens, so opening the revocation table on a host that has no
    // store would leave an empty `credentials.db` behind — and the next
    // `aroute doctor` would then report a store that holds nothing, which is
    // a report about a file the doctor did not create. `StoreProbe` is the
    // one resolver for this, and its own reason for existing is that `doctor`
    // and `serve` must not disagree about whether the store is usable.
    //
    // This is a **second connection to the same file** as the credential
    // store's, not a second copy of it. Two connections are what sqlite
    // supports (concurrent readers, one writer); one shared `Mutex` around
    // the store's connection would serialise the credential reads against
    // the revocation reads for no gain. The split is also what lets
    // `aroute keys revoke` work when the master has been rotated and the
    // store's own rows will not decrypt — the list needs no master.
    let revocations = match commands::store_probe(&cli.config) {
        commands::StoreProbe::Absent(_) => {
            // No store file. Opening the revocation table would CREATE one
            // (sqlite creates a file it opens), and a host that never had a
            // store should not grow one because it started a proxy.
            None
        }
        commands::StoreProbe::Open { path, .. }
        | commands::StoreProbe::Live { path, .. }
        | commands::StoreProbe::Unreadable { path, .. } => {
            // The file is there. Whether the **credential store** can read it
            // is a different question from whether **sqlite** can open it:
            // the probe reports `Unreadable` when `$AR_MASTER_KEY` is unset
            // (the store needs it to decrypt) even though the file opens
            // perfectly.
            //
            // The revocation table needs no master, so it is opened whenever
            // the FILE is there, and its own result decides. A host that
            // arms the gate from `AR_HTTP_MASTER_KEY` with no credential
            // store still gets a working revocation list — and a host whose
            // master was rotated still has one, which is exactly the case
            // where the operator needs to revoke something.
            match ar_keys::Revocations::open(&path, ar_keys::DEFAULT_CAP) {
                Ok(list) => Some(Arc::new(list)),
                Err(e) => {
                    // A proxy that cannot open the revocation table still
                    // serves: the table is optional, and a token revoked by
                    // other means is still good for its lifetime. The warning
                    // says so, because an operator who believes revocation
                    // works and does not is worse off than one who knows it
                    // does not.
                    eprintln!(
                        "ar: revocation table not available — revoked tokens stay valid until they expire: {e}"
                    );
                    None
                }
            }
        }
    };

    Ok((
        Components {
            master_key,
            ledger,
            obs_dir,
            revocations: revocations.clone(),
            ..Components::with_exec(config, exec)
        },
        revocations,
    ))
}

/// Boots the listener and blocks until interrupted.
/// K5: an API-key provider whose `$VAR` key is empty would send every request
/// unauthenticated. Refuses by key name, never by value. `doctor` still reports
/// the same finding, because it loads the config without this check.
fn refuse_empty_api_keys(cfg: &Config) -> anyhow::Result<()> {
    let empty = cfg.empty_api_keys();
    anyhow::ensure!(
        empty.is_empty(),
        "key(s) {} resolved to an empty value, so their providers would go upstream with no credential. \
         Set the variable(s) in the environment, or write \"\" in config.yaml if the provider is meant to be keyless",
        empty.join(", ")
    );
    Ok(())
}

pub async fn serve(cli: &Cli, args: &ServeArgs) -> anyhow::Result<()> {
    // Before anything binds: a malformed compiled-in table is a build defect, and
    // refusing here keeps it from degrading a running proxy at first use.
    ar_registry::check_embedded()?;
    let cfg = commands::load(cli)?;
    refuse_empty_api_keys(&cfg)?;
    let built = components(cli, &cfg, args.port, None)?;
    let (built, revocations) = built;

    // The control page (docs/16 Option B): the ledger Arc is cloned before the
    // server consumes `Components`, and the credential store is a second
    // sqlite connection — `components` keeps its own, and sqlite takes
    // concurrent readers.
    //
    // The gate is resolved **per request** (A8), and both the badge and the
    // guard read that one value — the old shape computed the badge from a
    // separate `gate_master` call and the gate from another, and the two could
    // disagree, which is the exact confusion this panel exists to remove.
    let mut ui_ctx = webui::Ctx::new(
        cli.config.clone(),
        built.ledger.clone(),
        commands::credential_store(cli),
    );

    // The panel's own gate, resolved by *decrypting* the gate row rather than
    // by finding its name. A row that will not decrypt under the current
    // master is a broken gate: the server's own boot log says "serving
    // ungated", so the operator believes the gate is on while it is not. The
    // panel's answer to that is what `GateState::Broken` is for — reads
    // degrade, writes refuse.
    //
    // The LIVE gate resolves the same state **per request**: arming the gate
    // while the proxy runs turns the panel's gate on, and breaking the master
    // turns it `Broken`. The master is re-derived only when the gate row's
    // ciphertext changes, so the cost of a request that changes nothing is
    // one sqlite point read.
    //
    // The env master is the **server's own resolved** one —
    // `built.config.http_master_key` — not a second read of the variable.
    // The server's decoder owns the encoding rules, and a second decoder that
    // disagreed with it would be a gate that works on one surface and not the
    // other. An environment variable cannot change under a running process,
    // so it is captured once.
    let env_master = built
        .config
        .http_master_key
        .as_ref()
        .map(|s| s.to_owned_secret());
    if let Some(live) = ui_ctx.live_gate(revocations.clone(), env_master) {
        ui_ctx.set_live_gate(live);
    }

    let server = ar_server::server(built);
    let count = server.state.config.providers.len();
    let port = server.state.config.port;

    // The panel reports the *running* proxy, not the file: its resilience
    // table, its provider list and its resolved settings are the live ones.
    // The combo table is deliberately NOT threaded through here: the panel
    // reads combos from the config file, which is the file it edits, and a
    // second copy of the combo table in the server's shape would be a second
    // source of truth for the same thing.
    ui_ctx.attach_server(
        Arc::clone(&server.state.resilience),
        server
            .state
            .config
            .providers
            .iter()
            .map(|p| p.id.as_str().to_owned())
            .collect(),
        webui::ServerSettings {
            request_timeout_secs: server.state.config.max_deadline().as_secs(),
            timeouts: server
                .state
                .config
                .timeouts
                .iter()
                .map(|(k, v)| (k.clone(), v.as_secs()))
                .collect(),
            model_discovery: server.state.config.model_discovery,
            auth_mode: server.state.auth_mode.as_str().to_owned(),
        },
    );
    let addr = ar_server::app::bind_addr(&cfg.server.host, port, false).map_err(|e| {
        commands::fail(
            e,
            "set `server.host` to a loopback address; `aroute serve` binds loopback only in this build",
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
    // The revocation table's sweep. Silent when there is no table — the
    // default install has none, and a line about it would be noise.
    let _sweep_task = server.spawn_revocation_sweep();
    if server.state.obs.is_some() {
        let dir = std::env::var(OBS_DIR_VAR).unwrap_or_default();
        eprintln!("observability write path armed ({OBS_DIR_VAR}={dir})");
    }

    // Mounted last: `mount` takes the router by value, and `spawn_discovery_refresh`
    // borrows the whole `Server`, so every borrow of `server` happens above this
    // line. The page is loopback-only by inheritance — `bind_addr` above refuses
    // a routable bind, so nothing outside this machine reaches `/ui`.
    let router = webui::mount(server.router, ui_ctx);

    serve_until_drained(listener, router, shutdown_signal(), DRAIN_WINDOW)
        .await
        .map_err(|e| commands::fail(e, "the listener stopped unexpectedly"))?;

    // The loop outlives the listener otherwise: it is an infinite interval task,
    // so the process would not return after a clean shutdown without this.
    if let Some(task) = discovery_task {
        task.abort();
    }
    Ok(())
}

/// How long a signal-initiated shutdown gives in-flight responses to finish
/// before the process drops the connections they ride on and exits.
///
/// The window runs from the signal. It must stay under the unit's
/// `TimeoutStopSec` (20 s in `aroute.service`), or systemd escalates to SIGKILL.
/// The proxy's request timeout bounds the request, not the response body
/// (`ar-server`'s `TimeoutLayer`), so a streamed generation can outlive any
/// window: an unbounded drain would never end under load.
const DRAIN_WINDOW: Duration = Duration::from_secs(10);

/// Serves `router` on `listener` until `stop` resolves, then waits at most
/// `drain` for in-flight connections before returning.
///
/// `axum::serve`'s own graceful shutdown waits for every connection to close,
/// with no upper bound, so it cannot be the only exit. This races it against a
/// deadline that starts when `stop` resolves. Past the deadline the listener and
/// any connections still open are dropped, and the process can exit.
async fn serve_until_drained(
    listener: tokio::net::TcpListener,
    router: axum::Router,
    stop: impl std::future::Future<Output = ()> + Send + 'static,
    drain: Duration,
) -> std::io::Result<()> {
    use std::future::IntoFuture;

    let (stopping_tx, stopping_rx) = tokio::sync::oneshot::channel::<()>();
    let graceful = axum::serve(listener, router).with_graceful_shutdown(async move {
        stop.await;
        // The receiver is only dropped once `serve_until_drained` has returned,
        // so a failed send means nobody is waiting on the deadline any more.
        let _ = stopping_tx.send(());
    });
    let deadline = async {
        if stopping_rx.await.is_ok() {
            tokio::time::sleep(drain).await;
        } else {
            std::future::pending::<()>().await;
        }
    };
    tokio::select! {
        result = graceful.into_future() => result,
        () = deadline => {
            eprintln!("drain window of {}s elapsed with connections still open; dropping them", drain.as_secs());
            Ok(())
        }
    }
}

/// Resolves on SIGINT/SIGTERM so a stop drains rather than cutting a streamed
/// answer in half. The drain bound lives in [`serve_until_drained`].
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
    refuse_empty_api_keys(&cfg)?;
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

    let (built, _revocations) = components(cli, &cfg, None, Some(&model))?;
    let server = ar_server::server(built);
    let payload = serde_json::json!({
        "model": model,
        "stream": false,
        "messages": [{ "role": "user", "content": args.prompt }],
    });
    let request = axum::http::Request::builder()
        .method(axum::http::Method::POST)
        .uri("/v1/chat/completions")
        // The one-shot request never crosses a socket, but the router cannot
        // know that: `loopback_only` judges every request by its `Host` and
        // this one had none, so the DNS-rebinding gate — correct for the
        // network path — rejected `aroute run` with 403 since the day it
        // landed (`crates/ar-server/src/app.rs:722`). Setting the header the
        // gate looks for, to the loopback authority the gate accepts, is the
        // whole fix: the gate stays exactly as strict for every real request.
        .header(axum::http::header::HOST, "127.0.0.1:20128")
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

    /// `sqlite` creates a file it opens, so the revocation table must not be
    /// opened on a host that has no credential store — otherwise `serve` on an
    /// env-only install leaves an empty `credentials.db` behind, and the next
    /// `aroute doctor` reports a store that holds nothing: a report about a
    /// file the doctor did not create.
    ///
    /// The house rule is already written down at `commands.rs`: "Not opened
    /// when absent: `open` would *create* the file, and a read-only check that
    /// leaves a database behind is a check that lies about what it found."
    /// This test is that rule, on the serve path.
    #[test]
    fn an_absent_store_leaves_no_file_behind() {
        let dir = std::env::temp_dir().join(format!("ar-serve-nostore-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let config = dir.join("config.yaml");
        std::fs::write(&config, CHEAP).expect("config");
        let store_path = commands::store_path(&config);
        assert!(
            !store_path.exists(),
            "precondition: there is no store to begin with"
        );

        // The probe is what decides, and it must not create the file either.
        let probe = commands::store_probe(&config);
        assert!(
            matches!(probe, commands::StoreProbe::Absent(_)),
            "the probe reports an absent store"
        );
        assert!(
            !store_path.exists(),
            "the probe must not leave a database behind"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

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

#[cfg(test)]
mod drain_tests {
    use std::time::{Duration, Instant};

    use axum::Router;
    use axum::routing::get;

    use super::serve_until_drained;

    /// A handler that never answers, so its request is in flight for the test.
    async fn hang() -> &'static str {
        std::future::pending::<&'static str>().await
    }

    #[tokio::test]
    async fn serve_until_drained_should_return_within_the_window_when_a_request_hangs() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("binds");
        let addr = listener.local_addr().expect("address");
        let router = Router::new().route("/hang", get(hang));
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(serve_until_drained(
            listener,
            router,
            async move {
                let _ = stop_rx.await;
            },
            Duration::from_millis(200),
        ));

        // Keep one request in flight, then ask the server to stop.
        let mut stream = tokio::net::TcpStream::connect(addr)
            .await
            .expect("connects");
        tokio::io::AsyncWriteExt::write_all(&mut stream, b"GET /hang HTTP/1.1\r\nHost: t\r\n\r\n")
            .await
            .expect("writes");
        tokio::time::sleep(Duration::from_millis(100)).await;
        let started = Instant::now();
        stop_tx.send(()).expect("server is listening");

        let result = tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .expect("the drain must end; an unbounded drain would hang here")
            .expect("server task joins");
        assert!(result.is_ok(), "drain returned an error: {result:?}");
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "drain took {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn serve_until_drained_should_return_at_once_when_idle() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("binds");
        let router = Router::new().route("/hang", get(hang));
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(serve_until_drained(
            listener,
            router,
            async move {
                let _ = stop_rx.await;
            },
            Duration::from_secs(10),
        ));
        stop_tx.send(()).expect("server is listening");
        let started = Instant::now();
        tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .expect("an idle server stops without waiting for the window")
            .expect("server task joins")
            .expect("serves cleanly");
        assert!(started.elapsed() < Duration::from_secs(3));
    }
}
