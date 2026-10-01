//! End-to-end contract tests for the `ar` binary.
//!
//! Each case spawns the real binary so the assertions cover the thing the AXI
//! gates actually promise — an exit code, a stdout shape, a stderr stream —
//! rather than an internal function's return value. The three named tests are
//! the `docs/06` gates; the `insta` goldens pin the TOON list shapes so a
//! column rename fails loudly instead of drifting.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// The binary under test, as built by cargo for this integration test.
const BIN: &str = env!("CARGO_BIN_EXE_ar");

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// Runs `ar` with the fixture config as the working directory.
fn ar(args: &[&str]) -> Output {
    Command::new(BIN)
        .args(args)
        .current_dir(fixture_dir())
        .output()
        .expect("the ar binary runs")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn code(out: &Output) -> i32 {
    out.status.code().expect("the process was not signalled")
}

#[test]
fn prints_home_when_no_args() {
    let out = ar(&[]);

    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("bin: "), "{text}");
    assert!(text.contains("one OpenAI-compatible endpoint"), "{text}");
    assert!(text.contains("listen: 127.0.0.1:20128"), "{text}");
    assert!(text.contains("combos[2]{id,provider,status}:"), "{text}");
    assert!(text.contains("ar doctor"), "{text}");
}

#[test]
fn exits_fast_when_version() {
    // An empty directory, no config: if the command graph or the config loader
    // were on this path the run would fail. That is the structural fast-path
    // assertion `docs/06` asks for — a timing threshold would only measure the
    // machine the suite happens to run on.
    let empty = empty_dir("version-fast-path");

    let out = Command::new(BIN).arg("--version").current_dir(&empty).output().expect("the ar binary runs");

    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "ar 0.1.1\n");
    assert_eq!(stderr(&out), "", "the fast path must not emit diagnostics");
}

#[test]
fn fails_loud_when_unknown_flag() {
    let out = ar(&["models", "--nope"]);

    assert_eq!(code(&out), 2);
    let text = stdout(&out);
    assert!(text.contains("--nope"), "must name the offending flag: {text}");
    assert!(text.contains("--fields"), "{text}");
    assert!(text.contains("--full"), "{text}");
    // The subcommand's own flag set, not the root's: `--prompt` belongs to `run`.
    assert!(!text.contains("--prompt"), "{text}");
}

#[test]
fn emits_error_with_help_on_stdout_when_config_absent() {
    let out = Command::new(BIN)
        .args(["doctor"])
        .current_dir(empty_dir("absent"))
        .output()
        .expect("the ar binary runs");

    assert_eq!(code(&out), 1);
    assert!(stdout(&out).contains("help: "), "{}", stdout(&out));
}

#[test]
fn renders_providers_list_shape() {
    insta::assert_snapshot!(stdout(&ar(&["providers"])));
}

#[test]
fn renders_models_list_shape() {
    // Not an `insta` golden: the catalog carries every provider's model list, so
    // the full output is >1500 rows and a golden of it would churn on every
    // upstream model addition while pinning nothing. The column header is the
    // contract; the count line proves the catalog is loaded rather than empty.
    let out = ar(&["models"]);
    let text = stdout(&out);
    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    // The `[N]` is the row count, so the header is matched in two parts; the
    // whole output is never interpolated into a failure message, because at 1559
    // rows that would bury the assertion.
    assert!(text.contains("total\nmodels["), "{}", &text[..text.len().min(200)]);
    assert!(text.contains("{id,provider,status}:"), "{}", &text[..text.len().min(200)]);
    assert!(text.contains("openai/gpt-5.4-nano,openai,routable"), "the catalog model is listed");
    assert!(text.contains("anthropic/claude-sonnet-5,anthropic,routable"), "the combo target is listed");
    assert!(!text.contains("gpt-5.4-nope"), "a model outside the catalog is not listed");
}

#[test]
fn renders_combo_list_shape() {
    insta::assert_snapshot!(stdout(&ar(&["combo"])));
}

#[test]
fn narrows_columns_when_fields_given() {
    let out = ar(&["providers", "--fields", "id,key"]);

    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    assert!(stdout(&out).contains("providers[2]{id,key}:"), "{}", stdout(&out));
}

#[test]
fn fails_run_when_model_not_a_combo() {
    let out = ar(&["run", "--model", "nope", "-p", "hi"]);

    assert_eq!(code(&out), 1);
    assert!(stdout(&out).contains("no combo named \"nope\" is configured"), "{}", stdout(&out));
}

#[test]
fn rejects_unknown_field_with_usage_error() {
    let out = ar(&["providers", "--fields", "nope"]);

    assert_eq!(code(&out), 2);
    assert!(stdout(&out).contains("unknown field \"nope\""), "{}", stdout(&out));
}

#[test]
fn renders_check_rows_when_doctor() {
    let out = ar(&["doctor"]);

    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    insta::assert_snapshot!(stdout(&out));
}

#[test]
fn reports_resolved_settings_when_configure() {
    let out = ar(&["configure"]);

    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("server.port"), "{text}");
    // Never a secret value, only the key name it is bound to.
    assert!(!text.contains("sk-test"), "{text}");
}

#[test]
fn passes_check_when_configure_valid() {
    let out = ar(&["configure", "--check"]);

    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
}

#[test]
fn fails_check_when_target_provider_absent() {
    let dir = empty_dir("bad-target");
    std::fs::write(
        dir.join("config.yaml"),
        "keys:\n  k: v\nproviders:\n  - id: openai\n    key: k\ncombos:\n  - id: c\n    strategy: priority\n    targets:\n      - groq/llama\n",
    )
    .expect("the fixture writes");

    let out = Command::new(BIN)
        .args(["configure", "--check"])
        .current_dir(&dir)
        .output()
        .expect("the ar binary runs");

    assert_eq!(code(&out), 1);
    assert!(stdout(&out).contains("1 check(s) failed"), "{}", stdout(&out));
}

#[test]
fn states_zero_definitively_when_no_combos() {
    let dir = empty_dir("no-combos");
    std::fs::write(dir.join("config.yaml"), "keys:\n  k: v\n").expect("the fixture writes");

    let out = Command::new(BIN).args(["combo"]).current_dir(&dir).output().expect("the ar binary runs");

    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "combo: 0 combos found\n");
}

/// A config whose only provider is a file-declared node: the id is in no
/// compiled-in catalog, so anything that works here proves the no-rebuild path.
const CUSTOM_ONLY_CONFIG: &str = r#"keys:
  local: sk-test-local

custom_providers:
  - id: local-gateway
    protocol: openai-compatible
    base_url: https://api.example.invalid/v1
    key_ref: local

combos:
  - id: default
    strategy: priority
    targets:
      - local-gateway/some-model
"#;

fn with_custom_config(name: &str, yaml: &str) -> PathBuf {
    let dir = empty_dir(name);
    std::fs::write(dir.join("config.yaml"), yaml).expect("the fixture writes");
    dir
}

fn ar_in(dir: &Path, args: &[&str]) -> Output {
    Command::new(BIN).args(args).current_dir(dir).output().expect("the ar binary runs")
}

#[test]
fn passes_doctor_when_a_custom_provider_is_declared() {
    let dir = with_custom_config("custom-ok", CUSTOM_ONLY_CONFIG);

    let out = ar_in(&dir, &["doctor"]);

    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("custom/local-gateway,ok"), "{text}");
    assert!(text.contains("target/local-gateway/some-model,ok,routable"), "{text}");
}

#[test]
fn routes_a_custom_provider_when_no_combo_target_names_a_catalog_id() {
    let dir = with_custom_config("custom-combo", CUSTOM_ONLY_CONFIG);

    let out = ar_in(&dir, &["combo"]);

    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    let text = stdout(&out);
    // The compiled-in-only grammar would render this row's provider as
    // `default`, which is the id no combo or provider here declares.
    assert!(text.contains("default,local-gateway,active"), "{text}");
}

#[test]
fn lists_a_custom_provider_when_it_is_declared() {
    let dir = with_custom_config("custom-providers", CUSTOM_ONLY_CONFIG);

    let out = ar_in(&dir, &["providers"]);

    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    assert!(stdout(&out).contains("local-gateway,openai,custom"), "{}", stdout(&out));
}

#[test]
fn fails_check_when_a_custom_provider_id_collides_with_the_catalog() {
    // The colliding node is not the combo's provider, so the collision is the
    // only thing wrong and the single `fail` row can be named.
    let dir = with_custom_config(
        "custom-collision",
        "keys:\n  openai: sk-test\ncustom_providers:\n  - id: openai\n    protocol: openai-compatible\n    base_url: https://api.example.invalid/v1\n    key_ref: openai\ncombos:\n  - id: default\n    strategy: priority\n    targets:\n      - openai/gpt-5.4\n",
    );

    let out = ar_in(&dir, &["doctor"]);

    assert_eq!(code(&out), 1, "stdout: {}", stdout(&out));
    let text = stdout(&out);
    assert!(text.contains("1 check(s) failed"), "{text}");
    assert!(text.contains("custom,fail"), "the collision is the named row: {text}");
    assert!(text.contains("\"openai\" is already in the registry"), "{text}");
}

#[test]
fn fails_check_when_a_custom_providers_base_url_has_no_scheme() {
    let dir = with_custom_config(
        "custom-bad-url",
        &CUSTOM_ONLY_CONFIG.replace("https://api.example.invalid/v1", "api.example.invalid/v1"),
    );

    let out = ar_in(&dir, &["doctor"]);

    assert_eq!(code(&out), 1);
    assert!(stdout(&out).contains("not an http(s) URL"), "{}", stdout(&out));
}

#[test]
fn reports_the_credential_store_as_skipped_when_there_is_none() {
    // No store is the pre-store configuration: `$VAR` alone still works, so this
    // must not turn `ar doctor` red.
    let out = ar(&["doctor"]);
    let text = stdout(&out);
    assert!(text.contains("store,skip,"), "{text}");
    assert!(text.contains("credentials.db"), "{text}");
    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
}

#[test]
fn fails_check_when_the_credential_store_cannot_be_read() {
    // A store on disk that `ar` cannot open is a silent loss of every credential
    // it holds, which is the one credential state worth failing on.
    let dir = empty_dir("bad-store");
    std::fs::write(
        dir.join("config.yaml"),
        "keys:\n  k: v\nproviders:\n  - id: openai\n    key: k\ncombos:\n  - id: c\n    strategy: priority\n    targets:\n      - openai/gpt-5.4\n",
    )
    .expect("the fixture writes");
    std::fs::write(dir.join("credentials.db"), b"not a sqlite file at all").expect("the fixture writes");

    let out = Command::new(BIN).args(["doctor"]).current_dir(&dir).output().expect("the ar binary runs");

    let text = stdout(&out);
    assert!(text.contains("store,fail,"), "{text}");
    assert!(text.contains("AR_MASTER_KEY"), "the reason names the fix: {text}");
    assert_eq!(code(&out), 1);
}

#[test]
fn names_the_credential_source_without_printing_any_value() {
    // `ar doctor` output is something people paste into issues, so the source of a
    // credential is reportable and its value is not.
    let text = stdout(&ar(&["doctor"]));
    assert!(text.contains("key/openai,ok,resolved from keys:"), "{text}");
    assert!(!text.contains("sk-test-openai"), "{text}");
}

/// A config declaring one browser-loginable OAuth session, and where to store
/// the rows a login writes.
///
/// The endpoints are `.invalid` so nothing here can reach a real IdP: these
/// tests exercise the store round trip and the redacted rendering, never the
/// network. `codex` is one of the five executors `ar_exec::oauth::OAuthKind`
/// names, so the session is one `ar auth login` can actually drive. The keys are
/// literals rather than `$VAR` so `ar` needs no environment to load the file.
const AUTH_SESSION_CONFIG: &str = r#"keys:
  codex: sk-test-codex-unresolved
  codex_refresh: sk-test-codex-refresh-unresolved

providers:
  - id: codex
    key: codex

oauth:
  - provider: codex
    refresh_key: codex_refresh
    token_url: https://auth.example.invalid/token
    authorization_url: https://auth.example.invalid/authorize
    redirect_uri: http://127.0.0.1:1455/callback
    client_id: synthetic-client

combos:
  - id: default
    strategy: priority
    targets:
      - codex/gpt-5.4-codex
"#;

/// A 32-byte master key as hex, which is the encoding [`ar_keys`] tries first.
///
/// The bytes are the ASCII of `ar-cli-test-master-key-32-bytes!` — readable in
/// a hex dump, so a leaked test key is obvious rather than anonymous.
///
/// Fixed rather than drawn so a failure is reproducible, and hex rather than raw
/// so the length is unambiguous — the decoder reads a 64-character hex string as
/// 32 bytes and anything else as base64, and a raw 32-character passphrase would
/// be decoded as base64 and come out the wrong width.
///
/// Passed per-command instead of through the environment because `cargo test`
/// runs these in parallel threads in one process: a process-global `set_var`
/// would be one test's key becoming another's, which is precisely the class of
/// flake the per-test temp directory exists to prevent.
const TEST_MASTER_KEY: &str = "61722d636c692d746573742d6d61737465722d6b65792d33322d627974657321";

/// [`TEST_MASTER_KEY`] as the 32 bytes it encodes.
///
/// `ar-keys` takes material as bytes and the `ar` runs take the same value as the
/// hex string it decodes from, so both sides of the round trip are pinned to one
/// constant rather than two that could drift.
fn hex_to_bytes(hex: &str) -> Option<Vec<u8>> {
    hex.as_bytes()
        .chunks(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok())
        .collect()
}

/// A store directory with the config written and `AR_CRED_STORE` pointed at it.
///
/// `AR_CRED_STORE` is named per-command for the same reason [`TEST_MASTER_KEY`]
/// is: a process-global environment variable is shared state between tests that
/// run at the same time. Reading the store must therefore also be told where it
/// is, which is what `AR_CRED_STORE` is for.
fn auth_dir(name: &str) -> PathBuf {
    let dir = empty_dir(name);
    std::fs::write(dir.join("config.yaml"), AUTH_SESSION_CONFIG).expect("the fixture writes");
    let _ = std::fs::remove_file(dir.join("credentials.db"));
    dir
}

/// `ar` in an `auth_dir`, with the store variables this test needs.
fn ar_auth(dir: &Path, args: &[&str]) -> Output {
    Command::new(BIN)
        .args(args)
        .current_dir(dir)
        .env("AR_MASTER_KEY", TEST_MASTER_KEY)
        .env("AR_CRED_STORE", dir.join("credentials.db"))
        .output()
        .expect("the ar binary runs")
}

#[test]
fn renders_a_session_row_when_status_is_asked_for() {
    let out = ar_auth(&auth_dir("auth-status"), &["auth", "status"]);

    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("sessions[1]{id,provider,status,login,access_key,reason,login_reason}:"), "{text}");
    assert!(text.contains("codex,codex,armed,"), "{text}");
    // The two verdicts are separate columns: the `keys:` literals resolve, so the
    // session dispatches (`armed`), and the declared endpoint makes it loggable
    // (`armed` again — a different question, answered on its own terms).
    assert!(text.contains(",armed,armed,codex,"), "both verdicts have their own cell: {text}");
}

#[test]
fn states_zero_definitively_when_status_finds_no_session() {
    let out = ar(&["auth", "status"]);

    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "sessions: 0 sessions found\n");
}

#[test]
fn never_renders_a_credential_value_in_auth_output() {
    // The invariant the whole surface exists to keep: a token, a code, or a
    // client secret never reaches stdout, on any verb, in any state.
    let dir = auth_dir("auth-no-leak");
    for args in [
        &["auth", "status"][..],
        &["auth", "logout", "--provider", "codex"][..],
        &["auth", "login", "--provider", "codex", "--no-browser"][..],
    ] {
        let text = stdout(&ar_auth(&dir, args));
        assert!(!text.contains("sk-test"), "{args:?} leaked a key: {text}");
        assert!(!text.contains("code_verifier"), "{args:?} leaked a verifier: {text}");
    }
}

#[test]
fn reports_logout_as_a_no_op_when_the_store_holds_nothing_for_the_session() {
    let out = ar_auth(&auth_dir("auth-logout-empty"), &["auth", "logout", "--provider", "codex"]);

    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("codex,unchanged,"), "{text}");
    assert!(text.contains("no store rows"), "{text}");
}

#[test]
fn refuses_logout_without_a_provider() {
    let out = ar_auth(&auth_dir("auth-logout-noid"), &["auth", "logout"]);

    assert_eq!(code(&out), 1);
    assert!(stdout(&out).contains("--provider"), "{}", stdout(&out));
}

#[test]
fn refuses_a_login_for_a_provider_no_session_declares() {
    let out = ar_auth(&auth_dir("auth-login-undeclared"), &["auth", "login", "--provider", "kilocode"]);

    assert_eq!(code(&out), 1);
    let text = stdout(&out);
    assert!(text.contains("no `oauth:` block declares kilocode"), "{text}");
    // The refusal hands over the exact block that fixes it, because "add one" is
    // not an instruction an agent can act on.
    assert!(text.contains("authorization_url"), "{text}");
}

#[test]
fn treats_a_closed_stdin_as_an_expired_login_rather_than_hanging() {
    // The one interaction has to terminate: a caller that pipes nothing gets an
    // expiry and exit 1, not a blocked read an agent cannot interrupt.
    let out = ar_auth(
        &auth_dir("auth-login-eof"),
        &["auth", "login", "--provider", "codex", "--no-browser"],
    );

    assert_eq!(code(&out), 1);
    assert!(stdout(&out).contains("expired"), "{}", stdout(&out));
}

#[test]
fn prints_the_authorize_url_before_reading_stdin() {
    // Printing first is the contract: the URL has to be on stdout *before* the
    // paste is read, so a headless caller can capture it and still feed the
    // redirect back in.
    let out = ar_auth(
        &auth_dir("auth-login-url"),
        &["auth", "login", "--provider", "codex", "--no-browser"],
    );

    let text = stdout(&out);
    assert!(text.contains("url: https://auth.example.invalid/authorize?"), "{text}");
    // The PKCE halves are public by construction and belong on the URL; the
    // verifier never does.
    assert!(text.contains("code_challenge_method=S256"), "{text}");
    assert!(!text.contains("code_verifier="), "{text}");
}

#[test]
fn exits_zero_when_auth_help_is_requested() {
    // The `docs/06` gate, for the new subtree: every level of it answers.
    for args in [
        &["auth", "--help"][..],
        &["auth", "login", "--help"][..],
        &["auth", "logout", "--help"][..],
        &["auth", "status", "--help"][..],
    ] {
        let out = ar(args);
        assert_eq!(code(&out), 0, "{args:?} stderr: {}", stderr(&out));
    }
}

#[test]
fn lists_the_login_flags_when_one_of_them_is_misspelled() {
    let out = ar(&["auth", "login", "--prov", "codex"]);

    assert_eq!(code(&out), 2);
    let text = stdout(&out);
    assert!(text.contains("--prov"), "must name the offending flag: {text}");
    assert!(text.contains("--provider"), "and the valid set: {text}");
    assert!(text.contains("--no-browser"), "{text}");
}

#[test]
fn names_login_readiness_in_the_doctor_rows() {
    // The fold `ar doctor` owes `ar auth`: a session that is armed for dispatch
    // but unloggable is invisible without it.
    let out = ar_auth(&auth_dir("auth-doctor"), &["doctor"]);

    let text = stdout(&out);
    assert!(text.contains("auth/codex,armed,"), "{text}");
    assert!(text.contains("ar auth login --provider codex"), "the row names the fix: {text}");
}

/// A directory under the crate's own target tree holding nothing but whatever the
/// caller writes into it.
///
/// Per-test rather than shared: `cargo test` runs these in parallel threads in one
/// process, and two tests sharing a `config.yaml` clobber each other. Hand-rolled
/// rather than a `tempfile` dev-dependency — it is one `mkdir`, and adding a crate
/// to assert `ar` will not read `config.yaml` is not a trade worth making.
///
/// `credentials.db` goes too, for the same reason: it is the credential store
/// `ar doctor` now probes for, and a leftover from a neighbouring test would turn
/// this one's `store` row into someone else's verdict.
fn empty_dir(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    std::fs::create_dir_all(&dir).expect("the temp dir is created");
    let _ = std::fs::remove_file(dir.join("config.yaml"));
    let _ = std::fs::remove_file(dir.join("credentials.db"));
    dir
}

/// Writes rows into a store the way a completed login would, so `status` and
/// `logout` can be observed against a populated one.
///
/// This is the half of the round trip that needs no IdP: the network half is
/// `ar_ex`'s, and what is under test here is that a populated store reads back as
/// `armed` and that a logout then removes exactly the rows the session declared —
/// and nothing else.
fn seed_rows(dir: &Path) {
    // `open_with_material` rather than the env-key variant: these tests run in
    // parallel threads in one process, and a process-global `AR_MASTER_KEY` would
    // be one test's key becoming another's. The material is passed explicitly
    // here and per-command for the `ar` runs, so both sides read one constant.
    let material = ar_keys::Secret::new(hex_to_bytes(TEST_MASTER_KEY).expect("the test key is hex"));
    let store = ar_keys::CredentialStore::open_with_material(&dir.join("credentials.db"), &material)
        .expect("the store opens under the test master key");
    // A row for an unrelated provider, so "logout removed the session's rows" can
    // be told apart from "logout removed every row".
    store
        .insert("openai", "openai", &ar_keys::Secret::new(b"sk-unrelated".to_vec()))
        .expect("the unrelated row writes");
    store
        .insert("codex", "codex", &ar_keys::Secret::new(b"access-token-value".to_vec()))
        .expect("the access row writes");
    store
        .insert("codex", "codex_refresh", &ar_keys::Secret::new(b"refresh-token-value".to_vec()))
        .expect("the refresh row writes");
}

#[test]
fn reports_a_populated_session_as_armed() {
    let dir = auth_dir("auth-status-armed");
    seed_rows(&dir);

    let out = ar_auth(&dir, &["auth", "status"]);

    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("codex,codex,armed,armed,"), "{text}");
}

#[test]
fn removes_exactly_the_session_rows_on_logout() {
    let dir = auth_dir("auth-logout-round-trip");
    seed_rows(&dir);

    let out = ar_auth(&dir, &["auth", "logout", "--provider", "codex"]);

    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("codex,logged-out,codex+codex_refresh"), "{text}");
    // The unrelated provider's row is the whole point of the assertion: a logout
    // that deleted every row would pass the check above.
    assert!(!text.contains("openai"), "another provider's row was removed: {text}");
}

#[test]
fn falls_back_to_the_config_keys_after_its_rows_are_logged_out() {
    // The round trip: status → logout → status. The third answer is what proves
    // the logout changed the *store* rather than just printing a table.
    //
    // It stays `armed` afterwards because the fixture declares literal `keys:`
    // values, which is exactly the precedence `ar doctor` documents — the store
    // first, then `keys:`. So the logout moves the *source* without making the
    // session unresolvable, and a test asserting `unarmed` here would be pinning
    // a claim that is wrong about how resolution actually works.
    let dir = auth_dir("auth-round-trip");
    seed_rows(&dir);
    let before = stdout(&ar_auth(&dir, &["auth", "status"]));
    assert!(before.contains("codex,codex,armed,armed,"), "{before}");

    let out = ar_auth(&dir, &["auth", "logout", "--provider", "codex"]);

    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    let after = stdout(&ar_auth(&dir, &["auth", "status"]));
    assert!(after.contains("codex,codex,armed,armed,"), "{after}");
}

#[test]
fn never_renders_a_stored_token_value_when_a_session_is_armed() {
    // The rows above hold literal token values; none may reach stdout.
    let dir = auth_dir("auth-armed-no-leak");
    seed_rows(&dir);

    let text = stdout(&ar_auth(&dir, &["auth", "status"]));

    assert!(!text.contains("access-token-value"), "{text}");
    assert!(!text.contains("refresh-token-value"), "{text}");
    assert!(!text.contains("sk-unrelated"), "{text}");
}
