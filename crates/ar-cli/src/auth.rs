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
//! # Nothing here prints a credential
//!
//! The authorize URL carries `state` and `code_challenge`, both public by
//! construction. The verifier never leaves the process except into the exchange
//! POST; the code and both tokens are read into `Secret`s and never rendered.
//! Every success row names a *row*, not a value — the same discipline
//! `ar doctor`'s `key/<name>` rows already keep.
//!
//! # Exactly one interaction
//!
//! The single documented read of one stdin line is the whole prompt surface.
//! `--no-browser` and a listener that cannot bind both land on it; EOF before
//! the line is a `LoginExpired`-shaped error, never a loop and never a second
//! read.

use std::io::BufRead as _;
use std::time::Duration;

use ar_config::{Config, OAuthSession};
use ar_exec::oauth::{
    CallbackListener, LoginError, OAuthKind, RefreshFault, Session, authorize_url, exchange_code,
    new_authorize_request, parse_callback_url,
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
        return Err(fail(
            format!("this build has no oauth executor for {provider}"),
            "`ar doctor` lists the providers it can authenticate; use one of those",
        ));
    };
    let mut session = Session::new(provider, kind);
    if let Some(url) = &declared.authorization_url {
        session = session.with_authorization_url(url.clone());
    }
    if let Some(url) = &declared.token_url {
        session = session.with_token_url(url.clone());
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

/// The credential-row name holding `provider`'s access token.
///
/// The provider's own `keys:` entry, as the schema documents — a login writes
/// the row dispatch already reads rather than inventing a second one.
fn access_key(cfg: &Config, provider: &str) -> String {
    cfg.key_name(provider).unwrap_or(provider).to_owned()
}

/// Authorises one provider and stores what comes back.
async fn login(cli: &crate::cli::Cli, args: &AuthLoginArgs) -> anyhow::Result<()> {
    let cfg = load(cli)?;
    let declared = session_for(&cfg, &args.provider)?;
    let mut session = exec_session(&args.provider, declared)?;
    if let Some(scope) = &args.scope {
        // `--scope` overrides the file for this one login, so it is applied to
        // the executor's session rather than mutating the parsed config.
        session = session.with_scope(scope.clone());
    }
    // Opened before anything is printed, and through `open_store` rather than
    // `commands::credential_store`: a login that printed an authorize URL and
    // then failed on a missing master key has sent an operator to a browser for a
    // session it could never have completed, and `credential_store` treats an
    // absent store as the supported `$VAR`-only install — which is right for
    // `ar serve` and wrong here, because a first-ever login is exactly the case
    // where the store has to be created.
    let path = commands::store_path(&cli.config);
    let store = commands::open_store(&path).map_err(|e| {
        fail(
            format!("cannot open the credential store at {}: {e}", path.display()),
            "set $AR_MASTER_KEY to 32 bytes and $AR_CRED_STORE to a writable path; `ar doctor` reports the store row",
        )
    })?;
    let probe = commands::live_store_probe(&path, &store);

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

    // Persist before reporting: a login that printed success and lost the token
    // would be the one failure an operator cannot detect.
    //
    // The two `Secret` types are deliberately distinct — `ar_config`'s is the
    // parsed-config wrapper, `ar_keys`' is the zeroizing store wrapper — so the
    // crossing happens here, at the one call site that writes, rather than as a
    // `From` impl that would hide the boundary everywhere else.
    let access = access_key(&cfg, &args.provider);
    store
        .insert(&args.provider, &access, &to_row(token.access()))
        .map_err(|e| store_write_error(&access, e))?;
    if let Some(refresh) = token.refresh() {
        let Some(name) = declared.refresh_key.as_deref() else {
            return Err(fail(
                "the provider returned a refresh_token but the session declares no refresh_key",
                "add `refresh_key:` to the `oauth:` block; without it the session dies on its first 401",
            ));
        };
        store
            .insert(&args.provider, name, &to_row(refresh))
            .map_err(|e| store_write_error(name, e))?;
    }

    // Verify through the same verdict `ar doctor` uses, so `ar auth login` and
    // `ar doctor` can never disagree about whether the login worked.
    let (status, reason) = commands::oauth_row(&args.provider, &cfg, &probe);
    print!(
        "{}",
        toon::list(
            "sessions",
            "sessions",
            &LOGIN_COLUMNS,
            &toon::every_field(&LOGIN_COLUMNS),
            &[vec![args.provider.clone(), commands::armed_spelling(&status).to_owned(), reason]],
            false,
        )
    );
    let _ = url;
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
}
