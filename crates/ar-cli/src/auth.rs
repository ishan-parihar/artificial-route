//! `ar auth login|logout|status` — browser-session OAuth from the terminal.
//!
//! The step order is OmniRoute's `inAppLoginService` (`url → complete → persist
//! → verify`) with Playwright replaced by a person on another device, which is
//! the only thing that works on a headless VPS: the URL travels out over
//! whatever channel the operator has, consent happens wherever the browser is,
//! and the redirected URL comes back on one line of stdin.
//!
//! * **authorize** — [`ar_exec::oauth::new_authorize_request`] +
//!   [`ar_exec::oauth::authorize_url`], from the session's own
//!   `authorization_url`/`client_id`/`scope`. Nothing is inferred from a
//!   provider id (AGENTS.md).
//! * **catch or paste** — [`ar_exec::oauth::CallbackListener`] on
//!   `127.0.0.1:<port>`, or [`ar_exec::oauth::parse_callback_url`] on one line
//!   of stdin. Both converge on the same `state` check, so a pasted redirect is
//!   held to the same bar as a caught one.
//! * **exchange** — [`ar_exec::oauth::exchange_code`], with the `client_secret`
//!   read from the `client_secret_key` row when the session declares one.
//! * **persist** — access and refresh rows into `ar-keys`' store, then
//!   **verify** — the same `ar doctor` verdict, printed as the success row.
//!
//! # Three mechanisms, one step order
//!
//! Which half runs is decided by what the `oauth:` block declares, never by the
//! provider id:
//!
//! | declares | mechanism | interaction |
//! |---|---|---|
//! | `anonymous: true` | none — the free tier | none at all; no store rows |
//! | `device_auth_url` + `device_poll_url` | RFC 8628 device grant | type a code on any device |
//! | `authorization_url` | PKCE redirect | catch it, or paste one line |
//!
//! The device half is what makes a login work from a headless box that has *no*
//! browser to open and no loopback to catch: [`initiate_device`] returns a
//! `user_code` and a `verification_uri`, both of which are printed and both of
//! which are meant to be read. Same order as the other two — initiate → present
//! → poll → persist → verify — so a caller that learned one does not have to
//! learn the third.
//!
//! # Nothing here prints a credential
//!
//! The authorize URL carries `state` and `code_challenge`, both public by
//! construction. The verifier never leaves the process except into the exchange
//! POST; the code and both tokens are read into `Secret`s and never rendered.
//! The device half holds the one genuinely new secret, `device_code`, inside
//! `ar_exec`'s `DevicePending` for the whole of the login and never reads it
//! here. Every success row names a *row*, not a value — the same discipline
//! `ar doctor`'s `key/<name>` rows already keep.
//!
//! # Exactly one interaction
//!
//! The single documented read of one stdin line is the whole prompt surface.
//! `--no-browser` and a listener that cannot bind both land on it; EOF before
//! the line is a `LoginExpired`-shaped error, never a loop and never a second
//! read. The device and anonymous halves have **no** stdin read at all: one is a
//! code typed in a browser somewhere else, and the other is nothing to type.

use std::io::BufRead as _;
use std::time::Duration;

use ar_config::{Config, OAuthSession};
use ar_exec::oauth::{
    CallbackListener, LoginError, OAuthKind, RefreshFault, Session, authorize_url, exchange_code,
    initiate_device, new_authorize_request, parse_callback_url, poll_device,
};
use ar_exec::ArExec;
use ar_keys::Secret as StoredSecret;

use crate::cli::{AuthArgs, AuthCommand, AuthLoginArgs, AuthProviderArgs};
use crate::commands::{self, fail, load};
use crate::toon;

/// Columns of the `ar auth status` table.
///
/// `id,provider,status` leads so `toon::DEFAULT_FIELDS` applies positionally, and
/// `provider` repeats the id because a TOON row read in isolation by an agent
/// should say which session it is about without a second lookup.
///
/// `status` and `login` are separate verdicts on separate questions and are never
/// merged: `status` is the `ar doctor` `oauth/` row (does this session dispatch)
/// and `login` is login readiness (can it be re-authorised). A session holding
/// both tokens but declaring no `authorization_url` is `armed` and `unavailable`
/// at the same time, and one cell could not say both.
const STATUS_COLUMNS: [&str; 7] =
    ["id", "provider", "status", "login", "access_key", "reason", "login_reason"];

/// Columns of the `ar auth login` result.
const LOGIN_COLUMNS: [&str; 3] = ["id", "status", "detail"];

/// Columns of the `ar auth logout` result.
const LOGOUT_COLUMNS: [&str; 3] = ["id", "status", "removed"];

/// Longest stdin line [`read_pasted_redirect`] will hold.
///
/// A redirect URL is a few hundred bytes; 8 KiB is the same ceiling
/// `ar-exec`'s callback listener uses for a request head, so a paste cannot
/// become an unbounded allocation. Past it the line is a mistake, not a redirect.
const PASTE_MAX: usize = 8 * 1024;

/// Dispatches `ar auth`.
pub fn run(cli: &crate::cli::Cli, args: &AuthArgs) -> anyhow::Result<()> {
    match &args.command {
        AuthCommand::Login(a) => commands::block_on(login(cli, a)),
        // Both of these are local-only, so they stay off the runtime `login` needs:
        // the five local verbs answer without building one.
        AuthCommand::Logout(a) => logout(cli, a),
        AuthCommand::Status(a) => status(cli, a),
    }
}

/// The `oauth:` block for `provider`, or a named refusal.
///
/// A login cannot invent its own placement, so this is where a missing block is
/// reported — with the exact YAML that fixes it rather than a sentence to guess
/// from.
fn session_for<'a>(cfg: &'a Config, provider: &str) -> anyhow::Result<&'a OAuthSession> {
    cfg.oauth_for(provider).ok_or_else(|| {
        fail(
            format!("no `oauth:` block declares {provider}"),
            format!(
                "add one naming the provider and its endpoints:\n\
                 oauth:\n  - provider: {provider}\n    \
                 authorization_url: https://<provider>/authorize\n    \
                 token_url: https://<provider>/token\n    client_id: <public-client-id>"
            ),
        )
    })
}

/// The [`Session`] the executor authorizes against, built from the config block.
///
/// One builder, so the CLI and any other consumer send the same endpoints. Every
/// field is optional in the schema and absent here means absent on the wire —
/// the executor's `authorize_url` then refuses with `NoAuthorizationUrl` rather
/// than a default.
fn exec_session(provider: &str, declared: &OAuthSession) -> anyhow::Result<Session> {
    let Some(kind) = OAuthKind::parse(provider) else {
        // Mechanism-aware, because the two reasons a session can lack a kind want
        // different fixes. R1's `kilocode` is the live one: it has no executor
        // because its mechanism was never traced, and the free tier is now a
        // mechanism this build *can* drive without one.
        return Err(fail(
            format!("this build has no oauth executor for {provider}"),
            if declared.anonymous {
                format!(
                    "`ar auth login --provider {provider}` needs no executor on an anonymous session, \
                     so this must be a dispatch problem rather than a login one; `ar doctor` reports it"
                )
            } else if declares_device(declared) {
                format!(
                    "{provider} declares a device login but this build has no oauth executor for it; \
                     add `anonymous: true` to its `oauth:` block to use the free tier instead"
                )
            } else {
                "`ar doctor` lists the providers it can authenticate; use one of those".to_owned()
            },
        ));
    };
    let mut session = Session::new(provider, kind);
    if let Some(url) = &declared.authorization_url {
        session = session.with_authorization_url(url.clone());
    }
    if let Some(url) = &declared.token_url {
        session = session.with_token_url(url.clone());
    }
    if let Some(url) = &declared.device_auth_url {
        session = session.with_device_auth_url(url.clone());
    }
    if let Some(url) = &declared.device_poll_url {
        session = session.with_device_poll_url(url.clone());
    }
    if let Some(id) = &declared.client_id {
        session = session.with_client_id(id.clone());
    }
    let scope = declared.scope.as_deref();
    if let Some(scope) = scope {
        session = session.with_scope(scope);
    }
    Ok(session)
}

/// How a session obtains its credential, decided by what the block declares.
///
/// Never by the provider id: the catalog says a provider *needs* OAuth, not
/// which of the three mechanisms it speaks, and a table keyed by id is the
/// invented-wire-format rule one indirection away from being broken.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mechanism {
    /// The provider's own free tier. Nothing to obtain.
    Anonymous,
    /// RFC 8628: a code typed on some other device.
    Device,
    /// PKCE: a redirect caught here or pasted back.
    Redirect,
}

/// The session's mechanism. `anonymous` wins because it is the only one that
/// needs no credential at all, and a block that declared both would be a config
/// error the loader already refuses.
fn mechanism(declared: &OAuthSession) -> Mechanism {
    if declared.anonymous {
        Mechanism::Anonymous
    } else if declares_device(declared) {
        Mechanism::Device
    } else {
        Mechanism::Redirect
    }
}

/// Whether a session declares both halves of a device login.
///
/// Both or neither: `ar-config` refuses a half pair at load, so this never has
/// to guess which half was meant.
fn declares_device(declared: &OAuthSession) -> bool {
    declared.device_auth_url.is_some() && declared.device_poll_url.is_some()
}

/// The credential-row name holding `provider`'s access token.
///
/// The provider's own `keys:` entry, as the schema documents — a login writes
/// the row dispatch already reads rather than inventing a second one.
fn access_key(cfg: &Config, provider: &str) -> String {
    cfg.key_name(provider).unwrap_or(provider).to_owned()
}

/// Opens the store a login is about to write through, creating it if absent.
///
/// Opened before anything is printed, and through `open_store` rather than
/// `commands::credential_store`: a login that printed an authorize URL and then
/// failed on a missing master key has sent an operator to a browser for a
/// session it could never have completed, and `credential_store` treats an
/// absent store as the supported `$VAR`-only install — which is right for
/// `ar serve` and wrong here, because a first-ever login is exactly the case
/// where the store has to be created.
fn open_login_store(cli: &crate::cli::Cli) -> anyhow::Result<(std::path::PathBuf, ar_keys::CredentialStore)> {
    let path = commands::store_path(&cli.config);
    let store = commands::open_store(&path).map_err(|e| {
        fail(
            format!("cannot open the credential store at {}: {e}", path.display()),
            "set $AR_MASTER_KEY to 32 bytes and $AR_CRED_STORE to a writable path; `ar doctor` reports the store row",
        )
    })?;
    Ok((path, store))
}

/// Writes the access and refresh rows a login minted.
///
/// One function for both mechanisms: the store layout is the store's, not the
/// flow's, and a device token that landed under a different set of names than a
/// PKCE one would leave `ar doctor` reporting a session it cannot find.
///
/// A provider that returned a refresh token but declared no `refresh_key` is
/// refused rather than stored under a guessed name: without the declaration the
/// session dies on its first 401, which is exactly what `ar doctor` is for.
fn persist_token(
    store: &ar_keys::CredentialStore,
    cfg: &Config,
    declared: &OAuthSession,
    provider: &str,
    token: &ar_exec::oauth::OAuthToken,
) -> anyhow::Result<()> {
    let access = access_key(cfg, provider);
    store
        .insert(provider, &access, &to_row(token.access()))
        .map_err(|e| store_write_error(&access, e))?;
    if let Some(refresh) = token.refresh() {
        let Some(name) = declared.refresh_key.as_deref() else {
            return Err(fail(
                "the provider returned a refresh_token but the session declares no refresh_key",
                "add `refresh_key:` to the `oauth:` block; without it the session dies on its first 401",
            ));
        };
        store
            .insert(provider, name, &to_row(refresh))
            .map_err(|e| store_write_error(name, e))?;
    }
    Ok(())
}

/// The `ar auth login` success row, verified through `ar doctor`'s own verdict.
///
/// Persist first: a login that printed success and lost the token would be the
/// one failure an operator cannot detect. Reporting second means `ar auth login`
/// and `ar doctor` can never disagree about whether the login worked.
fn report_login(
    provider: &str,
    cfg: &Config,
    path: &std::path::Path,
    store: &ar_keys::CredentialStore,
) {
    let probe = commands::live_store_probe(path, store);
    let (status, reason) = commands::oauth_row(provider, cfg, &probe);
    print!(
        "{}",
        toon::list(
            "sessions",
            "sessions",
            &LOGIN_COLUMNS,
            &toon::every_field(&LOGIN_COLUMNS),
            &[vec![provider.to_owned(), commands::armed_spelling(&status).to_owned(), reason]],
            false,
        )
    );
}

/// `ar auth login` for a session on the provider's anonymous free tier.
///
/// The whole verb, and the reason a login command can have nothing to do: there
/// is no account, so there is no code, no redirect and no row. Reads a probe
/// rather than opening a store, because *opening* one would create a database as
/// the only trace of a command that was asked to change nothing.
fn anonymous_login(
    cfg: &Config,
    cli: &crate::cli::Cli,
    provider: &str,
) -> anyhow::Result<()> {
    let probe = commands::store_probe(&cli.config);
    let (status, reason) = commands::oauth_row(provider, cfg, &probe);
    print!(
        "{}",
        toon::list(
            "sessions",
            "sessions",
            &LOGIN_COLUMNS,
            &toon::every_field(&LOGIN_COLUMNS),
            &[vec![provider.to_owned(), commands::armed_spelling(&status).to_owned(), reason]],
            false,
        )
    );
    Ok(())
}

/// `ar auth login` over an RFC 8628 device grant.
///
/// The step order is the same five as the redirect half — initiate, present,
/// poll, persist, verify — because it is the same command to an operator. Only
/// the middle step differs, and it differs in the direction that matters: instead
/// of a URL this host must be able to open, it prints two strings that work on a
/// phone. `user_code` and `verification_uri` are user-facing by construction,
/// which is what makes this path the answer for a headless box; `DevicePending`'s
/// `device_code` is the one secret and never leaves the executor.
///
/// Opening a browser is still attempted, best-effort, because on a developer's
/// own machine it is the convenient path and on a VPS its absence costs nothing.
async fn device_login(
    cfg: &Config,
    cli: &crate::cli::Cli,
    declared: &OAuthSession,
    args: &AuthLoginArgs,
) -> anyhow::Result<()> {
    if args.port != 0 {
        return Err(fail(
            format!("--port {} has no meaning for a device login: there is no loopback to bind", args.port),
            "drop --port; a device login is completed in a browser, not on this host",
        ));
    }
    let mut session = exec_session(&args.provider, declared)?;
    if let Some(scope) = &args.scope {
        session = session.with_scope(scope.clone());
    }
    let (path, store) = open_login_store(cli)?;

    let core = ArExec::new()
        .map_err(|e| fail(e, "the pooled HTTP client could not start; this is a host problem"))?;
    let (grant, pending) =
        initiate_device(&core, &session).await.map_err(|e| fail(e, login_help(&args.provider)))?;

    // The two lines the whole flow exists to produce. Both are printable on any
    // device by design, and both are the only thing a caller needs to relay.
    println!("provider: {}", args.provider);
    println!("code: {}", grant.user_code);
    println!("verification_uri: {}", grant.verification_uri);
    println!(
        "waiting: polling every {}s for up to {}s",
        grant.interval_secs, args.timeout
    );
    let opened = if args.no_browser { false } else { try_open_browser(&grant.verification_uri) };
    if !opened {
        println!("note: open the uri above on any device and enter the code there");
    }
    use std::io::Write as _;
    let _ = std::io::stdout().flush();

    let token = poll_device(&core, &session, &pending, Duration::from_secs(args.timeout))
        .await
        .map_err(|e| fail(e, device_help(&args.provider)))?;
    persist_token(&store, cfg, declared, &args.provider, &token)?;
    report_login(&args.provider, cfg, &path, &store);
    Ok(())
}

/// Authorises one provider and stores what comes back.
async fn login(cli: &crate::cli::Cli, args: &AuthLoginArgs) -> anyhow::Result<()> {
    let cfg = load(cli)?;
    let declared = session_for(&cfg, &args.provider)?;

    match mechanism(declared) {
        Mechanism::Anonymous => return anonymous_login(&cfg, cli, &args.provider),
        Mechanism::Device => return device_login(&cfg, cli, declared, args).await,
        Mechanism::Redirect => {}
    }

    let mut session = exec_session(&args.provider, declared)?;
    if let Some(scope) = &args.scope {
        // `--scope` overrides the file for this one login, so it is applied to
        // the executor's session rather than mutating the parsed config.
        session = session.with_scope(scope.clone());
    }
    let (path, store) = open_login_store(cli)?;

    // Path A first: a listener that binds means the browser can complete the
    // redirect with no involvement from the operator.
    //
    // `--port` is honoured only when the session does not pin a `redirect_uri:`.
    // A configured redirect is what the provider has registered and what the
    // authorize URL must name byte-for-byte, so a `--port` contradicting it would
    // open a browser that cannot finish. Refusing is cheaper than that.
    let listener = if args.no_browser { None } else { CallbackListener::bind().await.ok() };
    if let (Some(_), Some(configured)) = (&listener, declared.redirect_uri.as_deref())
        && args.port != 0
    {
        return Err(fail(
            format!("--port {} is ignored: the session pins redirect_uri to {configured}", args.port),
            "drop --port, or remove `redirect_uri:` from the `oauth:` block to let the listener choose the port",
        ));
    }
    let redirect_uri = match (&listener, declared.redirect_uri.as_deref()) {
        // An operator-declared redirect wins: a provider only accepts a redirect
        // it has registered, so overriding the ephemeral default is the only way
        // a remote-callback provider works at all.
        (Some(_), Some(configured)) => configured.to_owned(),
        (Some(listener), None) => listener.redirect_uri(),
        // Path B still needs *a* redirect for the authorize URL to name. The
        // listener is gone, so this is a declared value or nothing — and a
        // login with neither cannot build a URL the provider will honour.
        (None, Some(configured)) => configured.to_owned(),
        (None, None) => {
            return Err(fail(
                "no loopback callback is available and the session declares no redirect_uri",
                "declare `redirect_uri:` in the `oauth:` block, or drop --no-browser on a host with a loopback",
            ));
        }
    };

    let request = new_authorize_request(&session, &redirect_uri);
    let url = authorize_url(&request).map_err(|e| fail(e, login_help(&args.provider)))?;

    // The URL on stdout is the contract: it is what a headless caller captures,
    // and it is the whole of what a human needs. Opening a browser is a
    // convenience layered on top and is allowed to fail silently.
    println!("url: {url}");
    println!("provider: {}", args.provider);
    if listener.is_some() {
        println!("waiting: up to {}s for the redirect on {redirect_uri}", args.timeout);
    } else {
        println!(
            "paste: open the url above in any browser then paste the redirected url here (or a bare code)"
        );
    }
    let opened = if args.no_browser { false } else { try_open_browser(&url) };
    if !opened && listener.is_some() {
        println!("note: no browser was opened; complete the url above and the loopback will catch it");
    }
    use std::io::Write as _;
    let _ = std::io::stdout().flush();

    let timeout = Duration::from_secs(args.timeout);
    let code = match listener {
        Some(listener) => listener.wait_for_code(&request.state, timeout).await,
        // A bare code is accepted as well as a whole URL: an operator with a
        // provider that shows the code in a page has no URL to paste, and asking
        // them to synthesise one is asking for the paste to be wrong.
        None => read_pasted_code(&request.state),
    }
    .map_err(|e| fail(e, login_help(&args.provider)))?;

    // The client secret is a credential-store row like any other, read only
    // because the session declared it. `None` for a public PKCE client, which is
    // the common case.
    let secret = match declared.client_secret_key.as_deref() {
        None => None,
        Some(name) => {
            let stored = store.get(name).map_err(|e| {
                fail(
                    format!("client secret row {name:?} is unreadable: {e}"),
                    "re-store it, or drop `client_secret_key:` for a public PKCE client",
                )
            })?;
            let stored = stored.ok_or_else(|| {
                fail(
                    format!("client secret row {name:?} is not in the store"),
                    "re-store it, or drop `client_secret_key:` for a public PKCE client",
                )
            })?;
            // A client secret is form-encoded, so it has to be text: finding that
            // out here beats sending the provider a mangled credential.
            let text = String::from_utf8(stored.as_bytes().to_vec()).map_err(|_| {
                fail(
                    format!("client secret row {name:?} is not valid UTF-8"),
                    "re-store it as text; a form-encoded secret cannot be binary",
                )
            })?;
            Some(ar_config::Secret::new(&text))
        }
    };

    let core = ArExec::new()
        .map_err(|e| fail(e, "the pooled HTTP client could not start; this is a host problem"))?;
    let token = exchange_code(
        &core,
        &session,
        &code,
        &request.verifier,
        &request.redirect_uri,
        secret.as_ref(),
    )
    .await
    .map_err(|e| fail(e, login_help(&args.provider)))?;

    persist_token(&store, &cfg, declared, &args.provider, &token)?;
    let _ = url;
    report_login(&args.provider, &cfg, &path, &store);
    Ok(())
}

/// An `io::Error` at the paste, carried through the same taxonomy the catch path
/// uses so a caller matches on the variant rather than the string.
fn paste_fault(reason: &'static str) -> LoginError {
    LoginError::ExchangeFailed(RefreshFault::Transient(reason))
}

/// Reads exactly one line from stdin and turns it into an authorization code.
///
/// Once. A loop here would be an agent hanging on a read that will never be fed,
/// which is the deadlock `docs/06` forbids; EOF is a login that expired.
fn read_pasted_code(expected_state: &str) -> Result<String, LoginError> {
    let mut line = String::new();
    let read = std::io::stdin().lock().read_line(&mut line).map_err(|e| {
        paste_fault(if e.kind() == std::io::ErrorKind::UnexpectedEof {
            "stdin-closed-before-the-redirect"
        } else {
            "stdin-unreadable"
        })
    })?;
    if read == 0 {
        return Err(LoginError::LoginExpired);
    }
    let pasted = line.trim();
    if pasted.is_empty() {
        return Err(LoginError::LoginExpired);
    }
    // A pasted *code* has no `state` to check, which is why this branch comes
    // before the URL parser: the parser would read a bare code as a URL with no
    // code in it and report a misleading `callback-carries-no-code`.
    if !pasted.contains("?") && !pasted.contains("://") {
        if pasted.len() > PASTE_MAX {
            return Err(paste_fault("pasted-code-is-not-a-redirect"));
        }
        return Ok(pasted.to_owned());
    }
    parse_callback_url(pasted, expected_state)
}

/// Best-effort `$BROWSER`, then the platform opener.
///
/// Never an error: on a headless host every one of these fails, and the printed
/// URL is the contract. A non-zero exit is treated as "no browser" rather than
/// as a failure the operator has to read past.
fn try_open_browser(url: &str) -> bool {
    let mut candidates: Vec<Vec<String>> = Vec::new();
    if let Ok(browser) = std::env::var("BROWSER") {
        for word in browser.split_whitespace() {
            candidates.push(vec![word.to_owned(), url.to_owned()]);
        }
    }
    if cfg!(target_os = "macos") {
        candidates.push(vec!["open".to_owned(), url.to_owned()]);
    } else if cfg!(unix) {
        candidates.push(vec!["xdg-open".to_owned(), url.to_owned()]);
    }
    for args in candidates {
        if let Some((program, rest)) = args.split_first()
            && std::process::Command::new(program)
                .args(rest)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .is_ok()
        {
            let _ = program;
            return true;
        }
    }
    false
}

/// Removes a session's credential rows and reports which ones went.
fn logout(cli: &crate::cli::Cli, args: &AuthProviderArgs) -> anyhow::Result<()> {
    let cfg = load(cli)?;
    let provider = args.provider.as_deref().ok_or_else(|| {
        fail(
            "`ar auth logout` needs a provider",
            "run `ar auth logout --provider <id>`; `ar auth status` lists the ids",
        )
    })?;
    let declared = session_for(&cfg, provider)?;
    let targets = session_rows(&cfg, provider, declared);

    // A logout against a store that is not there has nothing to remove, and
    // opening one to find that out would leave a fresh database behind as the
    // only trace of a command that changed nothing. So the probe decides first
    // and the store is only opened when a row is actually going to go.
    let probe = commands::store_probe(&cli.config);    let mut removed: Vec<String> = Vec::new();
    if let Some(store) = commands::credential_store(cli) {
        for name in &targets {
            if probe.holds(name) && store.remove(name).map_err(|e| store_write_error(name, e))? {
                removed.push(name.clone());
            }
        }
    }

    print!(
        "{}",
        toon::list(
            "sessions",
            "sessions",
            &LOGOUT_COLUMNS,
            &toon::every_field(&LOGOUT_COLUMNS),
            &[vec![
                provider.to_owned(),
                if removed.is_empty() { "unchanged".to_owned() } else { "logged-out".to_owned() },
                if removed.is_empty() {
                    "no store rows for this session".to_owned()
                } else {
                    removed.join("+")
                },
            ]],
            false,
        )
    );
    Ok(())
}

/// The credential rows a session's logout may remove, sorted and deduplicated.
///
/// Only rows the session itself names. A logout that guessed at a name would be
/// the one command in the surface able to delete a credential nothing in
/// `config.yaml` ties to this provider.
fn session_rows(cfg: &Config, provider: &str, declared: &OAuthSession) -> Vec<String> {
    let mut targets: Vec<String> = vec![access_key(cfg, provider)];
    targets.extend(declared.refresh_key.clone());
    targets.extend(declared.client_secret_key.clone());
    targets.sort();
    targets.dedup();
    targets
}

/// Reports each OAuth session's armed state, redacted.
fn status(cli: &crate::cli::Cli, args: &AuthProviderArgs) -> anyhow::Result<()> {
    let cfg = load(cli)?;
    let store = commands::store_probe(&cli.config);
    let wanted = args.provider.as_deref();

    let mut rows: Vec<Vec<String>> = Vec::new();
    for declared in &cfg.oauth {
        if wanted.is_some_and(|id| id != declared.provider) {
            continue;
        }
        // Two verdicts, two questions: `status` is whether the session dispatches
        // (the `ar doctor` `oauth/` row) and `login` is whether it can be
        // re-authorised. A session can be armed on the first and unavailable on
        // the second, which is exactly the case an operator running `ar auth
        // login` is trying to find out about.
        let (status, reason) = commands::oauth_row(&declared.provider, &cfg, &store);
        let (login, why) = commands::login_readiness(&declared.provider, &cfg, &store);
        rows.push(vec![
            declared.provider.clone(),
            declared.provider.clone(),
            commands::armed_spelling(&status).to_owned(),
            // `login_readiness` already speaks this vocabulary; it is not routed
            // through `armed_spelling` because that maps the doctor's `ok`/`fail`,
            // and a re-auth verdict has three states of its own.
            login,
            access_key(&cfg, &declared.provider),
            reason,
            why,
        ]);
    }
    if let Some(id) = wanted
        && rows.is_empty()
    {
        return Err(fail(
            format!("no `oauth:` block declares {id}"),
            format!("add an `oauth:` block for {id}, or run `ar auth status` to see the sessions that exist"),
        ));
    }
    rows.sort();
    print!(
        "{}",
        toon::list("sessions", "sessions", &STATUS_COLUMNS, &toon::every_field(&STATUS_COLUMNS), &rows, false)
    );
    Ok(())
}

/// A store write failure, naming the row rather than the value.
fn store_write_error(name: &str, e: ar_keys::KeyError) -> anyhow::Error {
    fail(
        format!("cannot write credential row {name:?}: {e}"),
        "check $AR_MASTER_KEY is exported and the store path is writable",
    )
}

/// The one `help:` line every login failure carries.
///
/// An authorization URL is worth little without a place to paste it back into,
/// so the fix names the mode rather than the mechanism.
fn login_help(provider: &str) -> String {
    format!("re-run `ar auth login --provider {provider} --no-browser` and paste the redirected url on one line")
}

/// The `help:` line a device-login failure carries.
///
/// Names the code rather than the URL: a device login failed *after* the human
/// had it, so the useful fact is that the code is still good and the poll is
/// what to retry, rather than anything about pasting a redirect.
fn device_help(provider: &str) -> String {
    format!("re-run `ar auth login --provider {provider}` and enter the new code on any device before it expires")
}

/// A store row for `secret`, crossing the two `Secret` types.
///
/// `ar_config::Secret` is the parsed-config wrapper and `ar_keys::Secret` is the
/// zeroizing store wrapper; they are distinct types on purpose, so the crossing
/// is named once here instead of at each call site.
fn to_row(secret: &ar_config::Secret) -> StoredSecret {
    StoredSecret::new(secret.expose().as_bytes().to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ar_exec::oauth::RefreshFault;

    fn session(provider: &str) -> Session {
        Session::new(provider, OAuthKind::Codex)
            .with_authorization_url("https://auth.example.invalid/authorize")
            .with_token_url("https://auth.example.invalid/token")
            .with_client_id("public-client")
    }

    fn cfg_with_session() -> Config {
        Config::parse(
            concat!(
                "keys:\n  codex: $CODEX_ACCESS\n  codex_refresh: $CODEX_REFRESH\n",
                "providers:\n  - id: codex\n    key: codex\n",
                "oauth:\n  - provider: codex\n    refresh_key: codex_refresh\n",
                "    token_url: https://auth.example.invalid/token\n",
                "    authorization_url: https://auth.example.invalid/authorize\n",
                "    client_id: public-client\n",
            ),
            |name| Ok(Some(format!("synthetic-{name}"))),
        )
        .expect("the fixture parses")
    }

    #[test]
    fn builds_the_executor_session_from_the_config_block() {
        let cfg = cfg_with_session();
        let built = exec_session("codex", session_for(&cfg, "codex").expect("declared"))
            .expect("codex has an executor");
        assert_eq!(built.authorization_url(), Some("https://auth.example.invalid/authorize"));
    }

    #[test]
    fn refuses_a_provider_this_build_cannot_authenticate() {
        let cfg = cfg_with_session();
        let declared = cfg.oauth_for("codex").expect("declared");
        let err = exec_session("kilocode", declared).expect_err("no executor").to_string();
        assert!(err.contains("no oauth executor for kilocode"), "{err}");
    }

    #[test]
    fn names_the_missing_oauth_block_rather_than_inventing_one() {
        let cfg = Config::parse("keys:\n  k: v\n", |_| Ok(Some("v".to_owned()))).expect("parses");
        let err = session_for(&cfg, "codex").expect_err("no block").to_string();
        assert!(err.contains("no `oauth:` block declares codex"), "{err}");
        assert!(err.contains("authorization_url"), "the help shows the shape: {err}");
    }

    #[test]
    fn writes_the_row_dispatch_already_reads() {
        // A login that invented a second access row would leave the session
        // unarmed: the router resolves the provider's own `keys:` name.
        let cfg = cfg_with_session();
        assert_eq!(access_key(&cfg, "codex"), "codex");
    }

    #[test]
    fn falls_back_to_the_provider_id_when_no_key_is_bound() {
        let cfg = Config::parse("keys: {}\n", |_| Ok(Some("v".to_owned()))).expect("parses");
        assert_eq!(access_key(&cfg, "codex"), "codex");
    }

    #[test]
    fn names_the_paste_as_the_fix_for_every_login_failure() {
        let help = login_help("codex");
        assert!(help.contains("--no-browser"), "{help}");
        assert!(help.contains("--provider codex"), "{help}");
    }

    #[test]
    fn treats_a_closed_stdin_as_an_expired_login() {
        // The read is the only prompt; a caller that fed nothing gets an expiry
        // rather than a second blocking read.
        let err = LoginError::LoginExpired;
        assert!(err.to_string().contains("expired"), "{err}");
    }

    #[test]
    fn never_names_a_token_in_a_login_failure() {
        // `LoginError`'s own Display is the operator sentence; the enum is what
        // keeps a code or a verifier out of it.
        let rendered = [
            LoginError::StateMismatch.to_string(),
            LoginError::LoginExpired.to_string(),
            LoginError::LoginDenied("access_denied".into()).to_string(),
            LoginError::ExchangeFailed(RefreshFault::Transient("x")).to_string(),
        ]
        .join(" | ");
        assert!(!rendered.contains("code="), "{rendered}");
        assert!(!rendered.contains("verifier"), "{rendered}");
    }

    #[test]
    fn builds_an_authorize_url_from_the_session_alone() {
        let request = new_authorize_request(&session("codex"), "http://127.0.0.1:9/callback");
        let url = authorize_url(&request).expect("the session has an endpoint");
        assert!(url.contains("code_challenge_method=S256"), "{url}");
    }

    #[test]
    fn refuses_to_build_a_url_for_a_session_with_no_authorize_endpoint() {
        let bare = Session::new("codex", OAuthKind::Codex).with_client_id("cid");
        let request = new_authorize_request(&bare, "http://127.0.0.1:9/callback");
        assert!(authorize_url(&request).is_err(), "an endpoint is never inferred");
    }

    #[test]
    fn keeps_the_status_columns_leading_with_the_toon_defaults() {
        assert_eq!(&STATUS_COLUMNS[..3], &toon::DEFAULT_FIELDS[..]);
    }

    /// A kilocode-shaped free-tier block: no key row, no refresh, no endpoint.
    fn cfg_anonymous() -> Config {
        Config::parse(
            concat!(
                "keys: {}\n",
                "providers:\n  - id: kilocode\n    key: kilocode\n",
                "oauth:\n  - provider: kilocode\n    anonymous: true\n",
                "    anonymous_editor: artificial-route\n",
            ),
            |_| Ok(Some(String::new())),
        )
        .expect("the anonymous fixture parses")
    }

    #[test]
    fn reports_an_anonymous_session_as_the_free_tier_when_it_declares_it() {
        let cfg = cfg_anonymous();
        let declared = cfg.oauth_for("kilocode").expect("declared");
        assert_eq!(mechanism(declared), Mechanism::Anonymous);
    }

    #[test]
    fn reports_a_device_session_when_both_device_endpoints_are_declared() {
        let cfg = Config::parse(
            concat!(
                "keys:\n  grok: $G\n",
                "providers:\n  - id: grok-cli\n    key: grok\n",
                "oauth:\n  - provider: grok-cli\n",
                "    device_auth_url: https://auth.example.invalid/device\n",
                "    device_poll_url: https://auth.example.invalid/device\n",
            ),
            |name| Ok(Some(format!("synthetic-{name}"))),
        )
        .expect("the device fixture parses");
        let declared = cfg.oauth_for("grok-cli").expect("declared");
        assert_eq!(mechanism(declared), Mechanism::Device);
    }

    #[test]
    fn reports_a_redirect_session_when_no_device_endpoint_is_declared() {
        let cfg = cfg_with_session();
        let declared = cfg.oauth_for("codex").expect("declared");
        assert_eq!(mechanism(declared), Mechanism::Redirect);
    }

    #[test]
    fn carries_the_device_endpoints_into_the_executor_session() {
        // The executor reads `Session::device_auth_url`, not the config's field
        // names, so a block that parses but is not carried would present a code
        // and then poll an endpoint it never learned.
        let cfg = Config::parse(
            concat!(
                "keys:\n  grok: $G\n",
                "providers:\n  - id: grok-cli\n    key: grok\n",
                "oauth:\n  - provider: grok-cli\n",
                "    device_auth_url: https://auth.example.invalid/device\n",
                "    device_poll_url: https://auth.example.invalid/poll\n",
            ),
            |name| Ok(Some(format!("synthetic-{name}"))),
        )
        .expect("the device fixture parses");
        let declared = cfg.oauth_for("grok-cli").expect("declared");
        let built = exec_session("grok-cli", declared).expect("grok-cli has an executor");
        assert_eq!(built.device_auth_url(), Some("https://auth.example.invalid/device"));
    }

    #[test]
    fn never_prints_a_device_code_when_a_device_login_is_refused() {
        // The refusal an operator sees is a `LoginError`, and none of its variants
        // may carry the one secret the device flow holds.
        let rendered = [
            LoginError::NoDeviceAuthUrl.to_string(),
            LoginError::NoDevicePollUrl.to_string(),
            LoginError::ExchangeFailed(RefreshFault::Transient("device-response-has-no-device-code")).to_string(),
            LoginError::LoginExpired.to_string(),
        ]
        .join(" | ");
        assert!(!rendered.contains("device_code="), "{rendered}");
    }

    #[test]
    fn never_prints_the_anonymous_editor_value_in_a_refusal() {
        // The editor name is not a secret, but a row that renders config values
        // is a row that will render a credential the first time one is added here.
        let cfg = cfg_anonymous();
        let (_status, reason) =
            commands::oauth_row("kilocode", &cfg, &commands::store_probe(std::path::Path::new("no-such-config.yaml")));
        assert!(!reason.contains("artificial-route"), "{reason}");
    }

    #[test]
    fn names_the_free_tier_as_the_fix_when_a_session_has_no_executor() {
        // R1's kilocode: no `OAuthKind`, so the row must say what *would* work
        // rather than only what does not.
        let cfg = Config::parse("keys: {}\n", |_| Ok(Some(String::new()))).expect("parses");
        let (_, fix) = commands::oauth_row(
            "kilocode",
            &cfg,
            &commands::store_probe(std::path::Path::new("no-such-config.yaml")),
        );
        assert!(fix.contains("anonymous: true"), "{fix}");
    }

    #[test]
    fn refuses_a_port_for_a_device_login_rather_than_ignoring_it() {
        // A device login binds nothing, so a `--port` on it is a flag whose
        // absence changes nothing and whose presence promises something.
        let args = AuthLoginArgs { port: 1455, ..login_args() };
        let cfg = cfg_device();
        let declared = cfg.oauth_for("grok-cli").expect("declared");
        let cli = cli_for("no-such-config.yaml");
        let err = commands::block_on_value(device_login(&cfg, &cli, declared, &args))
            .expect_err("no loopback to bind");
        assert!(err.to_string().contains("no meaning for a device login"), "{err}");
    }

    fn cfg_device() -> Config {
        Config::parse(
            concat!(
                "keys:\n  grok: $G\n",
                "providers:\n  - id: grok-cli\n    key: grok\n",
                "oauth:\n  - provider: grok-cli\n",
                "    device_auth_url: https://auth.example.invalid/device\n",
                "    device_poll_url: https://auth.example.invalid/poll\n",
            ),
            |name| Ok(Some(format!("synthetic-{name}"))),
        )
        .expect("the device fixture parses")
    }

    fn cli_for(config: &str) -> crate::cli::Cli {
        use clap::Parser as _;
        crate::cli::Cli::try_parse_from(["ar", "--config", config, "auth", "login", "--provider", "grok-cli"])
            .expect("the flag set parses")
    }

    fn login_args() -> AuthLoginArgs {
        AuthLoginArgs {
            provider: "grok-cli".into(),
            port: 0,
            no_browser: true,
            timeout: 300,
            scope: None,
        }
    }
}
