//! End-to-end contract tests for the `aroute` binary.
//!
//! Each case spawns the real binary so the assertions cover the thing the AXI
//! gates actually promise — an exit code, a stdout shape, a stderr stream —
//! rather than an internal function's return value. The three named tests are
//! the `docs/06` gates; the `insta` goldens pin the TOON list shapes so a
//! column rename fails loudly instead of drifting.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// The binary under test, as built by cargo for this integration test.
const BIN: &str = env!("CARGO_BIN_EXE_aroute");

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
    assert!(text.contains("aroute doctor"), "{text}");
}

#[test]
fn exits_fast_when_version() {
    // An empty directory, no config: if the command graph or the config loader
    // were on this path the run would fail. That is the structural fast-path
    // assertion `docs/06` asks for — a timing threshold would only measure the
    // machine the suite happens to run on.
    let empty = empty_dir("version-fast-path");

    let out = Command::new(BIN)
        .arg("--version")
        .current_dir(&empty)
        .output()
        .expect("the ar binary runs");

    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    // The contract is that the banner agrees with the crate's own version and
    // prints it on one line — not that it equals some literal. Pinning the
    // number here meant the test failed on every release bump, which trains a
    // reader to ignore it instead of reading it as a real check.
    assert_eq!(
        stdout(&out),
        format!("aroute {}\n", env!("CARGO_PKG_VERSION"))
    );
    assert_eq!(stderr(&out), "", "the fast path must not emit diagnostics");
}

#[test]
fn fails_loud_when_unknown_flag() {
    let out = ar(&["models", "--nope"]);

    assert_eq!(code(&out), 2);
    let text = stdout(&out);
    assert!(
        text.contains("--nope"),
        "must name the offending flag: {text}"
    );
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
    assert!(
        text.contains("total\nmodels["),
        "{}",
        &text[..text.len().min(200)]
    );
    assert!(
        text.contains("{id,provider,status}:"),
        "{}",
        &text[..text.len().min(200)]
    );
    assert!(
        text.contains("openai/gpt-5.4-nano,openai,routable"),
        "the catalog model is listed"
    );
    assert!(
        text.contains("anthropic/claude-sonnet-5,anthropic,routable"),
        "the combo target is listed"
    );
    assert!(
        !text.contains("gpt-5.4-nope"),
        "a model outside the catalog is not listed"
    );
}

#[test]
fn renders_combo_list_shape() {
    insta::assert_snapshot!(stdout(&ar(&["combo"])));
}

#[test]
fn narrows_columns_when_fields_given() {
    let out = ar(&["providers", "--fields", "id,key"]);

    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    assert!(
        stdout(&out).contains("providers[2]{id,key}:"),
        "{}",
        stdout(&out)
    );
}

#[test]
fn fails_run_when_model_not_a_combo() {
    let out = ar(&["run", "--model", "nope", "-p", "hi"]);

    assert_eq!(code(&out), 1);
    assert!(
        stdout(&out).contains("no combo named \"nope\" is configured"),
        "{}",
        stdout(&out)
    );
}

#[test]
fn rejects_unknown_field_with_usage_error() {
    let out = ar(&["providers", "--fields", "nope"]);

    assert_eq!(code(&out), 2);
    assert!(
        stdout(&out).contains("unknown field \"nope\""),
        "{}",
        stdout(&out)
    );
}

#[test]
fn renders_check_rows_when_doctor() {
    let out = ar(&["doctor"]);

    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    insta::assert_snapshot!(frozen_age(stdout(&out)));
}

/// Rewrites the registry row's day figure to a fixed placeholder.
///
/// The row reports the age of the checked-in registry snapshot, so a raw
/// golden rots the day after it was recorded. Only the digit run between
/// `snapshot ` and `d old` is replaced; when the row ever stops matching,
/// the raw (un-normalized) text reaches the comparison and the golden fails
/// on the real change instead of passing silently.
fn frozen_age(text: String) -> String {
    const MARK: &str = "snapshot ";
    const TAIL: &str = "d old";
    let Some(mark) = text.find(MARK) else {
        return text;
    };
    let digits = mark + MARK.len();
    let Some(tail) = text[digits..].find(TAIL) else {
        return text;
    };
    let mut frozen = String::with_capacity(text.len() + 3);
    frozen.push_str(&text[..digits]);
    frozen.push_str("<N>");
    frozen.push_str(&text[digits + tail..]);
    frozen
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
    assert!(
        stdout(&out).contains("1 check(s) failed"),
        "{}",
        stdout(&out)
    );
}

#[test]
fn states_zero_definitively_when_no_combos() {
    let dir = empty_dir("no-combos");
    std::fs::write(dir.join("config.yaml"), "keys:\n  k: v\n").expect("the fixture writes");

    let out = Command::new(BIN)
        .args(["combo"])
        .current_dir(&dir)
        .output()
        .expect("the ar binary runs");

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
    Command::new(BIN)
        .args(args)
        .current_dir(dir)
        .output()
        .expect("the ar binary runs")
}

#[test]
fn passes_doctor_when_a_custom_provider_is_declared() {
    let dir = with_custom_config("custom-ok", CUSTOM_ONLY_CONFIG);

    let out = ar_in(&dir, &["doctor"]);

    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("custom/local-gateway,ok"), "{text}");
    assert!(
        text.contains("target/local-gateway/some-model,ok,routable"),
        "{text}"
    );
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
    assert!(
        stdout(&out).contains("local-gateway,openai,custom"),
        "{}",
        stdout(&out)
    );
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
    assert!(
        text.contains("custom,fail"),
        "the collision is the named row: {text}"
    );
    assert!(
        text.contains("\"openai\" is already in the registry"),
        "{text}"
    );
}

#[test]
fn fails_check_when_a_custom_providers_base_url_has_no_scheme() {
    let dir = with_custom_config(
        "custom-bad-url",
        &CUSTOM_ONLY_CONFIG.replace("https://api.example.invalid/v1", "api.example.invalid/v1"),
    );

    let out = ar_in(&dir, &["doctor"]);

    assert_eq!(code(&out), 1);
    assert!(
        stdout(&out).contains("not an http(s) URL"),
        "{}",
        stdout(&out)
    );
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
    std::fs::write(dir.join("credentials.db"), b"not a sqlite file at all")
        .expect("the fixture writes");

    let out = Command::new(BIN)
        .args(["doctor"])
        .current_dir(&dir)
        .output()
        .expect("the ar binary runs");

    let text = stdout(&out);
    assert!(text.contains("store,fail,"), "{text}");
    assert!(
        text.contains("AR_MASTER_KEY"),
        "the reason names the fix: {text}"
    );
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
    assert!(
        text.contains("sessions[1]{id,provider,status,login,access_key,reason,login_reason}:"),
        "{text}"
    );
    assert!(text.contains("codex,codex,armed,"), "{text}");
    // The two verdicts are separate columns: the `keys:` literals resolve, so the
    // session dispatches (`armed`), and the declared endpoint makes it loggable
    // (`armed` again — a different question, answered on its own terms).
    assert!(
        text.contains(",armed,armed,codex,"),
        "both verdicts have their own cell: {text}"
    );
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
        assert!(
            !text.contains("code_verifier"),
            "{args:?} leaked a verifier: {text}"
        );
    }
}

#[test]
fn reports_logout_as_a_no_op_when_the_store_holds_nothing_for_the_session() {
    let out = ar_auth(
        &auth_dir("auth-logout-empty"),
        &["auth", "logout", "--provider", "codex"],
    );

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
    let out = ar_auth(
        &auth_dir("auth-login-undeclared"),
        &["auth", "login", "--provider", "kilocode"],
    );

    assert_eq!(code(&out), 1);
    let text = stdout(&out);
    assert!(
        text.contains("no `oauth:` block declares kilocode"),
        "{text}"
    );
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
    assert!(
        text.contains("url: https://auth.example.invalid/authorize?"),
        "{text}"
    );
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
    assert!(
        text.contains("--prov"),
        "must name the offending flag: {text}"
    );
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
    assert!(
        text.contains("aroute auth login --provider codex"),
        "the row names the fix: {text}"
    );
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
    // `ar.env` too: a write verb that leaves one behind after a FAILED negative
    // control would otherwise be read by the next run as "the env file exists",
    // and a test that fails on another test's debris teaches the wrong lesson.
    let _ = std::fs::remove_file(dir.join("ar.env"));
    dir
}

#[cfg(unix)]
struct FakeSystemctl {
    bin: PathBuf,
    state: PathBuf,
    log: PathBuf,
    installed: bool,
}

#[cfg(unix)]
fn fake_systemctl(name: &str, installed: bool, initial_state: &str) -> FakeSystemctl {
    use std::os::unix::fs::PermissionsExt;

    let root = empty_dir(name);
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).expect("the fake bin directory writes");
    let command = bin.join("systemctl");
    std::fs::write(
        &command,
        r#"#!/bin/sh
printf '%s\n' "$*" >> "$AR_TEST_SYSTEMCTL_LOG"
verb=""
unit=""
for arg in "$@"; do
  case "$arg" in
    list-unit-files|show|is-enabled|enable|disable)
      verb="$arg"
      unit=""
      ;;
    *)
      if [ -n "$verb" ] && [ -z "$unit" ]; then unit="$arg"; fi
      ;;
  esac
done
case "$verb" in
  list-unit-files)
    if [ "$AR_TEST_SYSTEMCTL_INSTALLED" = yes ]; then printf '%s enabled\n' "$unit"; fi
    ;;
  show) printf 'active\n' ;;
  is-enabled) state=$(cat "$AR_TEST_SYSTEMCTL_STATE"); printf '%s\n' "$state"; [ "$state" = enabled ] ;;
  enable) printf 'enabled\n' > "$AR_TEST_SYSTEMCTL_STATE" ;;
  disable) printf 'disabled\n' > "$AR_TEST_SYSTEMCTL_STATE" ;;
  *) printf 'unexpected systemctl call: %s\n' "$*" >&2; exit 64 ;;
esac
"#,
    )
    .expect("the fake systemctl writes");
    let mut permissions = std::fs::metadata(&command)
        .expect("the fake systemctl metadata reads")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&command, permissions).expect("the fake systemctl becomes executable");

    let state = root.join("state");
    std::fs::write(&state, format!("{initial_state}\n")).expect("the fake service state writes");
    let log = root.join("calls.log");
    let _ = std::fs::remove_file(&log);
    FakeSystemctl {
        bin,
        state,
        log,
        installed,
    }
}

#[cfg(unix)]
fn ar_with_systemctl(args: &[&str], fake: &FakeSystemctl) -> Output {
    let path = std::env::join_paths(std::iter::once(fake.bin.clone()).chain(
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
    ))
    .expect("the isolated PATH joins");
    Command::new(BIN)
        .args(args)
        .current_dir(fixture_dir())
        .env("PATH", path)
        .env("AR_TEST_SYSTEMCTL_STATE", &fake.state)
        .env("AR_TEST_SYSTEMCTL_LOG", &fake.log)
        .env(
            "AR_TEST_SYSTEMCTL_INSTALLED",
            if fake.installed { "yes" } else { "no" },
        )
        .output()
        .expect("the ar binary runs with fake systemctl")
}

/// Parses `count: N of TOTAL` out of a rendered list.
///
/// Reads the printed header rather than the command's own internals, because
/// what an agent consumes is this line. A paging test asserts on this so it
/// fails if paging starts printing rows without fixing the count.
fn first_row_count(text: &str) -> usize {
    let line = text
        .lines()
        .find(|l| l.starts_with("count:"))
        .unwrap_or_else(|| panic!("no count line in: {text}"));
    line.split("count:")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("unparseable count line: {line}"))
}

/// Counts the indented data rows in a rendered list.
///
/// Not the same as `first_row_count`, which reads the header: this counts what
/// was actually printed, so a test can catch a renderer that emits the right
/// `count:` line over the wrong number of rows.
fn count_rows(text: &str) -> usize {
    text.lines().filter(|l| l.starts_with("  ")).count()
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
    let material =
        ar_keys::Secret::new(hex_to_bytes(TEST_MASTER_KEY).expect("the test key is hex"));
    let store =
        ar_keys::CredentialStore::open_with_material(&dir.join("credentials.db"), &material)
            .expect("the store opens under the test master key");
    // A row for an unrelated provider, so "logout removed the session's rows" can
    // be told apart from "logout removed every row".
    store
        .insert(
            "openai",
            "openai",
            &ar_keys::Secret::new(b"sk-unrelated".to_vec()),
        )
        .expect("the unrelated row writes");
    store
        .insert(
            "codex",
            "codex",
            &ar_keys::Secret::new(b"access-token-value".to_vec()),
        )
        .expect("the access row writes");
    store
        .insert(
            "codex",
            "codex_refresh",
            &ar_keys::Secret::new(b"refresh-token-value".to_vec()),
        )
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
    assert!(
        text.contains("codex,logged-out,codex+codex_refresh"),
        "{text}"
    );
    // The unrelated provider's row is the whole point of the assertion: a logout
    // that deleted every row would pass the check above.
    assert!(
        !text.contains("openai"),
        "another provider's row was removed: {text}"
    );
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

#[test]
fn dashboard_should_render_help_when_requested() {
    let out = ar(&["dashboard", "--help"]);

    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("web UI"), "{text}");
    assert!(text.contains("aroute dashboard"), "{text}");
}

#[test]
fn dashboard_should_name_the_missing_dist_when_path_absent() {
    // A path that cannot contain `server.js`: the command must name it and
    // point at the two ways to fix it, without spawning anything.
    let out = ar(&["dashboard", "--path", "/nonexistent-ar-dist"]);

    assert_eq!(code(&out), 1);
    let text = stdout(&out);
    assert!(text.contains("no dashboard at"), "{text}");
    assert!(text.contains("/nonexistent-ar-dist"), "{text}");
    assert!(text.contains("AR_DASHBOARD_DIR"), "{text}");
}

// --- keys list, and the arm-gate overwrite guard (docs/36 step 5 / B-5) ---
//
// These run as subprocesses, not in-process, for the reason `TEST_MASTER_KEY`
// gives: a process-global `set_var` is shared state between tests that run at
// the same time. `Command::env` scopes the credential to one child, so the
// refusal below can be asserted against a store this test owns alone.

/// A store directory already holding a gate master, which is the state the
/// guard has to detect.
fn gate_dir(name: &str) -> PathBuf {
    let dir = empty_dir(name);
    std::fs::write(dir.join("config.yaml"), AUTH_SESSION_CONFIG).expect("the fixture writes");
    let store = dir.join("credentials.db");
    let _ = std::fs::remove_file(&store);
    let out = Command::new(BIN)
        .args(["keys", "arm-gate"])
        .current_dir(&dir)
        .env("AR_MASTER_KEY", TEST_MASTER_KEY)
        .env("AR_CRED_STORE", &store)
        .output()
        .expect("the first arming runs");
    assert_eq!(
        out.status.code(),
        Some(0),
        "the store must start armed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    dir
}

/// `ar` with a store this test owns, the same shape as [`ar_auth`].
fn ar_gate(dir: &Path, args: &[&str]) -> Output {
    Command::new(BIN)
        .args(args)
        .current_dir(dir)
        .env("AR_MASTER_KEY", TEST_MASTER_KEY)
        .env("AR_CRED_STORE", dir.join("credentials.db"))
        .output()
        .expect("the ar binary runs")
}

#[test]
fn a_second_arming_is_refused_and_names_the_flag_that_allows_it() {
    let dir = gate_dir("gate-refusal");
    let store = std::fs::read(dir.join("credentials.db")).expect("read the store");

    let out = ar_gate(&dir, &["keys", "arm-gate"]);

    assert_eq!(code(&out), 1, "refusing is a failure, not a silent success");
    let text = format!("{}{}", stdout(&out), stderr(&out));
    assert!(
        text.contains("already armed"),
        "must name the reason: {text}"
    );
    assert!(text.contains("--force"), "must name the flag: {text}");
    // The store must be byte-identical: the refusal happened before any secret
    // was generated or written, which is what makes it safe to run unattended.
    assert_eq!(
        std::fs::read(dir.join("credentials.db")).expect("re-read"),
        store,
        "a refused arming must leave the store untouched"
    );
}

#[test]
fn forcing_an_arming_overwrites_and_says_what_it_invalidated() {
    let dir = gate_dir("gate-forced");

    let out = ar_gate(&dir, &["keys", "arm-gate", "--force"]);

    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    let text = stdout(&out);
    assert!(
        text.contains("previous master is replaced"),
        "the overwrite must say what it invalidated: {text}"
    );
}

#[test]
fn keys_list_reports_the_store_and_says_tokens_are_not_recorded() {
    let dir = gate_dir("keys-list");

    let out = ar_gate(&dir, &["keys", "list"]);

    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("gate: armed"), "{text}");
    assert!(
        text.contains("http-gate"),
        "the row name must appear: {text}"
    );
    // The honesty this verb exists for: no token material, and a plain statement
    // that none is knowable -- an empty table would read as "there are none".
    assert!(text.contains("minted tokens are not recorded"), "{text}");
    assert!(
        !text.contains("eyJ"),
        "no JWT material may be printed: {text}"
    );
}

#[test]
fn keys_list_on_a_host_with_no_store_does_not_invent_one() {
    let dir = empty_dir("keys-list-absent");
    std::fs::write(dir.join("config.yaml"), AUTH_SESSION_CONFIG).expect("the fixture writes");

    let out = Command::new(BIN)
        .args(["keys", "list"])
        .current_dir(&dir)
        .env("AR_MASTER_KEY", TEST_MASTER_KEY)
        .env("AR_CRED_STORE", dir.join("credentials.db"))
        .output()
        .expect("the ar binary runs");

    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    assert!(stdout(&out).contains("store: absent"), "{}", stdout(&out));
    assert!(
        !dir.join("credentials.db").exists(),
        "a read-only verb must not create the store it reports on"
    );
}

// ---------------------------------------------------------------------------
// G3: the query is narrowed on the server side, not by the caller grepping
// stdout. Every case below spawns the real binary against the fixture config
// and asserts the row set, because the filter's contract is which rows survive.
// ---------------------------------------------------------------------------

#[test]
fn models_routable_lists_the_configs_targets_not_the_registry() {
    // 1563 compiled-in catalog rows collapse to the 3 this fixture routes.
    // Before this flag an agent had to fetch all 1563 and grep client-side,
    // which is the 18k-token wall docs/36 measures.
    let out = ar(&["models", "--routable"]);

    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    let text = stdout(&out);
    assert_eq!(
        first_row_count(&text),
        3,
        "the fixture has exactly 3 distinct targets: {text}"
    );
    assert!(
        text.contains("count: 3 of 3 total"),
        "TOTAL is the full row count of the filtered set: {text}"
    );
    // A catalog row the fixture does not route must not appear: that is the
    // whole point of the filter.
    assert!(
        !text.contains("openai/gpt-4o-mini") || text.contains("-"),
        "no unroutable catalog row: {text}"
    );
}

#[test]
fn models_routable_keeps_targets_whose_status_is_not_routable() {
    // The first implementation filtered on `status == "routable"`, which
    // requires provider AND model to be in the compiled-in registry. A combo
    // target that cannot currently route is exactly the row an agent needs
    // ("why is my route failing?"), so it must survive with the status that
    // says why. This is the regression test for that: with the status-string
    // filter, this config's own targets all disappear.
    let dir = empty_dir("g3-custom-targets");
    // A custom provider is not in the compiled-in registry, so its target's
    // status is `unknown-provider` while still being a combo target.
    std::fs::write(
        dir.join("config.yaml"),
        "keys:\n  k: sk-test\ncombos:\n  - id: only\n    strategy: priority\n    \
         targets:\n      - ghost/mystery-model\n",
    )
    .expect("the fixture writes");

    let out = Command::new(BIN)
        .args(["models", "--routable"])
        .current_dir(&dir)
        .output()
        .expect("the ar binary runs");

    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    let text = stdout(&out);
    assert!(
        text.contains("ghost/mystery-model"),
        "a target of a configured combo is routable in substance, whatever its status: {text}"
    );
    assert!(
        text.contains("unknown-provider"),
        "the status must still name the half that failed: {text}"
    );
}

#[test]
fn models_limit_pages_and_total_names_the_unpaged_count() {
    // `count: N of TOTAL` was literally `N of N` before G3, so the field an
    // agent branches on said nothing. The contract: TOTAL is the count BEFORE
    // paging, so a caller can decide whether to run the query again.
    let out = ar(&["models", "--routable", "--limit", "1"]);

    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    let text = stdout(&out);
    assert_eq!(first_row_count(&text), 1, "one row is printed: {text}");
    assert!(
        text.contains("count: 1 of 3 total"),
        "TOTAL stays 3 so the caller knows there is more: {text}"
    );
}

#[test]
fn models_offset_past_the_end_is_definitely_empty_and_says_so() {
    // An empty list must be a definite "0 of 3", never a blank table an agent
    // could read as "there are none" or as a broken call.
    let out = ar(&["models", "--routable", "--offset", "99"]);

    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    let text = stdout(&out);
    assert!(
        text.contains("0 models found"),
        "definitively empty, with the count: {text}"
    );
}

#[test]
fn doctor_limit_pages_and_a_status_filter_cannot_launder_a_failure() {
    let out = ar(&["doctor", "--limit", "3"]);
    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    let text = stdout(&out);
    assert_eq!(first_row_count(&text), 3, "three checks print: {text}");
    assert!(
        text.contains("count: 3 of 12 total"),
        "the fixture has 12 checks; TOTAL must be the full count: {text}"
    );

    // The exit code is the invariant. Two directions, because the
    // laundering bug is directional:
    //
    // (a) `--status ok` on a config that DOES fail must still exit 1. This is
    //     the actual bug — proven by hand against a build where `failed` was
    //     computed from the filtered rows: it exits **0** and prints only
    //     passing checks, so a broken config sails through a build gate.
    // (b) `--status fail` on a clean config must exit 0. A filter must not be
    //     able to invent a failure either.
    //
    // An assertion on `--status fail` alone proves neither: it always matches
    // when there is a failure, so it cannot detect which set was counted.
    let bad = empty_dir("g3-doctor-fail");
    std::fs::write(
        bad.join("config.yaml"),
        "combos:\n  - id: only\n    strategy: priority\n    targets:\n      - openai/nope-not-real\n",
    )
    .expect("the fixture writes");

    let ok_out = Command::new(BIN)
        .args(["doctor", "--status", "ok"])
        .current_dir(&bad)
        .output()
        .expect("the ar binary runs");

    assert_eq!(
        code(&ok_out),
        1,
        "filtering to the passing rows must not launder a failure into exit 0: {}",
        stdout(&ok_out)
    );
    let filtered = stdout(&ok_out);
    assert!(
        filtered.contains("check(s) failed"),
        "the failure is still reported even though only `ok` rows print: {filtered}"
    );

    // And the same filter on the clean fixture exits 0.
    let clean = ar(&["doctor", "--status", "fail"]);
    assert_eq!(
        code(&clean),
        0,
        "a clean config with no `fail` rows passes: {}",
        stdout(&clean)
    );
}

#[test]
fn doctor_unknown_status_names_the_valid_ones() {
    // `--status` is a `ValueEnum`, not a free `String`, so a typo exits 2 with
    // the valid set — the same contract as `--fields bogus`. The alternative,
    // a free string matching nothing, would print `0 checks` and look like a
    // clean config: the failure mode this whole CLI is built to refuse.
    let out = ar(&["doctor", "--status", "sucess"]);

    assert_eq!(
        code(&out),
        2,
        "a bad --status value is a usage error like every other bad flag value: {}",
        stderr(&out)
    );
    let text = format!("{}{}", stdout(&out), stderr(&out));
    assert!(
        text.contains("possible values") && text.contains("ok") && text.contains("fail"),
        "clap names the valid statuses inline: {text}"
    );
}

// ---------------------------------------------------------------------------
// G6: the lifecycle verb. `status` is read-only and is what an agent runs
// before it sends traffic, so the whole test is about what it claims and when
// it exits non-zero. Nothing here touches the real services: the state-changing
// verbs need root-visible units, so they are not asserted against here.
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn service_status_should_report_both_units_read_only_when_units_are_not_installed() {
    // The one verb that is safe to run at any time, and the one an agent needs
    // before routing: two rows, one per unit, each naming systemd's verdict
    // AND this process's own probe. Those are two different claims, and the
    // incident `docs/25` records was a service systemd called active while its
    // port did not answer.
    let fake = fake_systemctl("service-status", false, "disabled");
    let out = ar_with_systemctl(&["service", "status"], &fake);
    let text = stdout(&out);
    let log = std::fs::read_to_string(&fake.log).expect("the systemctl calls are logged");
    assert_eq!(
        (
            code(&out),
            text.contains("aroute.service"),
            text.contains("aroute-dashboard.service"),
            count_rows(&text),
            text.matches("not installed").count(),
            log.lines()
                .all(|call| !["enable", "disable", "start", "stop", "restart"]
                    .iter()
                    .any(|verb| call.split_whitespace().nth(1) == Some(verb)))
        ),
        (0, true, true, 2, 2, true),
        "status should be read-only and report both absent units once: {text}; calls: {log}"
    );
}

#[test]
fn service_status_reports_an_uninstalled_unit_without_calling_it_a_failure() {
    // `docs/plans/2026-10-19` documents a CLI-only host: the binary is kept,
    // the daemon is not installed. A `status` that exited 1 there would make
    // the verb unusable on exactly the machines that run the CLI.
    //
    // `--system` is the scope that has no aroute units on a user host, which is
    // the same state as a CLI-only box without needing a special fixture.
    let out = ar(&["service", "status", "--system"]);

    let text = stdout(&out);
    assert!(text.contains("aroute.service"), "{text}");
    assert!(
        text.contains("not installed"),
        "an absent unit is reported as absent, not as broken: {text}"
    );
}

#[test]
fn service_requires_naming_the_unit_before_it_will_stop_one() {
    // The other half of the safety story. `aroute service restart proxy` names
    // the target; `aroute service restart` must be a usage error, because a
    // CLI that can stop a production listener on a guess is a foot-gun.
    for verb in ["start", "stop", "restart"] {
        let out = ar(&["service", verb]);
        assert_eq!(
            code(&out),
            2,
            "`aroute service {verb}` must require the unit: {}",
            stdout(&out)
        );
    }
}

#[test]
fn service_rejects_a_unit_that_does_not_exist_by_name() {
    let out = ar(&["service", "restart", "proxey"]);
    assert_eq!(code(&out), 2, "stderr: {}", stderr(&out));
    let text = format!("{}{}", stdout(&out), stderr(&out));
    assert!(
        text.contains("proxy") && text.contains("dashboard"),
        "the valid units are named inline: {text}"
    );
}

#[cfg(unix)]
#[test]
fn service_disable_should_only_change_boot_persistence_when_unit_is_installed() {
    // `enable`/`disable` answer "will it come back after a reboot", which is a
    // different question from "is it running". The A/B that proved the split:
    // `disable dashboard` left the unit `active` and answering on its port
    // while `is-enabled` flipped to `disabled` — the two states are
    // independent and both must stay observable in `status`.
    //
    // This test drives the real binary against a per-test systemctl shim.
    // It proves the CLI sends only the boot-persistence verb and never touches
    // a developer's or CI runner's actual services.
    let fake = fake_systemctl("service-disable", true, "enabled");
    let out = ar_with_systemctl(&["service", "disable", "dashboard"], &fake);
    let text = stdout(&out);
    let state_after_disable = std::fs::read_to_string(&fake.state).expect("state reads");

    // `status` now shows both states, which is how an agent sees the split.
    let status = ar_with_systemctl(&["service", "status", "dashboard"], &fake);
    let st = stdout(&status);
    let log = std::fs::read_to_string(&fake.log).expect("the systemctl calls are logged");
    assert_eq!(
        (
            code(&out),
            text.contains("disabled at boot"),
            text.contains("boot persistence only"),
            state_after_disable.trim(),
            st.contains("disabled at boot"),
            log.lines().all(|call| !["start", "stop", "restart"]
                .iter()
                .any(|verb| call.split_whitespace().nth(1) == Some(verb)))
        ),
        (0, true, true, "disabled", true, true),
        "disable changes boot persistence only: {text}; status: {st}; calls: {log}"
    );
}

#[cfg(unix)]
#[test]
fn service_enable_should_be_idempotent_when_unit_is_already_enabled() {
    // Boot persistence is a state, not an action: `enable` on an enabled unit
    // is the same outcome, not an error. An agent that cannot tell whether its
    // `enable` was needed would have to check first, which is a second call
    // the verb can absorb by just succeeding.
    let fake = fake_systemctl("service-enable", true, "enabled");
    let out = ar_with_systemctl(&["service", "enable", "proxy"], &fake);
    let text = stdout(&out);
    let state = std::fs::read_to_string(&fake.state).expect("state reads");
    let log = std::fs::read_to_string(&fake.log).expect("the systemctl calls are logged");
    assert_eq!(
        (
            code(&out),
            text.contains("enabled at boot"),
            state.trim(),
            log.lines()
                .any(|call| call == "--user --no-pager enable aroute.service"),
            log.lines()
                .all(|call| !["disable", "start", "stop", "restart"]
                    .iter()
                    .any(|verb| call.split_whitespace().nth(1) == Some(verb)))
        ),
        (0, true, "enabled", true, true),
        "enable should be idempotent without changing running state: {text}; calls: {log}"
    );
}

#[test]
fn run_reaches_the_router_instead_of_being_refused_by_the_loopback_gate() {
    // The DNS-rebinding gate (`loopback_only`, ar-server/src/app.rs:722) judges
    // every request by its Host header. `aroute run` builds an in-process
    // one-shot request that had NO Host header, so the gate — correct for the
    // network path — rejected `aroute run` itself with 403 non_loopback_request
    // since the day it landed. The fix sets the loopback Host header the gate
    // accepts; the gate stays exactly as strict for every real request.
    //
    // This test pins the consumer-visible half: the error an agent sees must be
    // an UPSTREAM answer (auth, connection, timeout), never the gate's 403.
    // A fake key means the only network call is an auth refusal — no spend.
    let dir = empty_dir("run-loopback-gate");
    std::fs::write(
        dir.join("config.yaml"),
        "keys:\n  local: sk-test-aroute\nproviders:\n  - id: openai\n    key: local\ncombos:\n  \
         - id: t\n    strategy: priority\n    targets:\n      - openai/gpt-5.4\n",
    )
    .expect("the fixture writes");

    let out = Command::new(BIN)
        .args(["run", "--prompt", "hi"])
        .current_dir(&dir)
        .output()
        .expect("the ar binary runs");

    let text = format!("{}{}", stdout(&out), stderr(&out));
    assert!(
        !text.contains("non_loopback_request"),
        "the loopback gate must not refuse aroute run's own in-process request: {text}"
    );
    // The decision line is the proof the request reached the router's
    // failover machinery rather than dying in middleware.
    assert!(
        text.contains("decision:") || text.contains("error:"),
        "an upstream answer (even a refusal) proves the gate passed it: {text}"
    );
}

/// A throwaway directory with `name` guaranteed empty of generated files.
fn import_dir(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    std::fs::create_dir_all(&dir).expect("the temp dir is created");
    for f in [
        "config.yaml",
        "registry.json",
        "freeBudgets.json",
        "modelLifecycle.json",
    ] {
        let _ = std::fs::remove_file(dir.join(f));
    }
    dir
}

/// A LiteLLM document the importer can read offline.
fn litellm_source(dir: &Path) -> PathBuf {
    let src = dir.join("litellm.yaml");
    std::fs::write(
        &src,
        "model_list:\n  - model_name: gpt\n    litellm_params:\n      model: openai/gpt-5.4\n\
         api_key: os.environ/OPENAI_API_KEY\n",
    )
    .expect("the litellm source writes");
    src
}

/// An `import` over a target whose `config.yaml` carries an `oauth:` block must
/// refuse and leave the file alone.
///
/// This is the consumer-visible bug: `render_yaml` cannot emit `oauth:`, so the
/// wholesale write dropped the operator's live sessions with a zero exit code and
/// no line of output naming the loss. The assertions are on the file's bytes and
/// the exit code, because both are what an agent saw.
#[test]
fn import_should_refuse_when_the_target_config_carries_a_section_it_cannot_emit() {
    let out = import_dir("import-refuses-section-loss");
    let src = litellm_source(&out);
    let held = out.join("config.yaml");
    std::fs::write(
        &held,
        "keys:\n  openai: $AR_KEY_OPENAI\nproviders:\n  - id: openai\n    key: openai\n\
         combos:\n  - id: default\n    strategy: lkgp\n    targets:\n      - openai/gpt-5.4\n\
         oauth:\n  - provider: codex\n    refresh_key: codex_refresh\n\
         client_id: app_abc123\n    token_endpoint: https://auth.openai.com/oauth/token\n",
    )
    .expect("the seed config writes");
    let before = std::fs::read_to_string(&held).expect("the seed is readable");

    let res = Command::new(BIN)
        .args([
            "import",
            "--from",
            "litellm",
            "--path",
            src.to_str().expect("utf-8 source path"),
            "--out-dir",
            out.to_str().expect("utf-8 out dir"),
        ])
        .output()
        .expect("the ar binary runs");

    assert_eq!(code(&res), 1, "stderr: {}", stderr(&res));
    // Errors are data for an agent, so they print on stdout (`docs/06`); stderr
    // stays for tracing alone.
    assert!(
        stdout(&res).contains("oauth:") && stdout(&res).contains("--allow-section-loss"),
        "the refusal must name the section at risk and its escape hatch: {}",
        stdout(&res)
    );
    assert_eq!(
        std::fs::read_to_string(&held).expect("the config is still readable"),
        before,
        "a refused import must leave the file byte-for-byte intact"
    );
    // The refusal precedes every other write, so a second import's outputs are
    // not half-regenerated either.
    assert!(
        !out.join("registry.json").exists(),
        "a refused import must write nothing else either"
    );
}

/// The escape hatch is a conscious decision, so naming the section works and
/// naming a *different* one does not.
#[test]
fn import_should_allow_named_section_loss_only_when_the_name_matches() {
    let out = import_dir("import-allow-section-loss");
    let src = litellm_source(&out);
    let held = out.join("config.yaml");

    let seed = |body: &str| {
        std::fs::write(&held, body).expect("the seed config writes");
    };
    let seeded = "keys:\n  openai: $AR_KEY_OPENAI\nproviders:\n  - id: openai\n    key: openai\n\
                  combos:\n  - id: default\n    strategy: lkgp\n    targets:\n      - openai/gpt-5.4\n\
                  oauth:\n  - provider: codex\n    refresh_key: codex_refresh\n";

    let run = |allow: &str| {
        Command::new(BIN)
            .args([
                "import",
                "--from",
                "litellm",
                "--path",
                src.to_str().expect("utf-8 source path"),
                "--out-dir",
                out.to_str().expect("utf-8 out dir"),
                "--allow-section-loss",
                allow,
            ])
            .output()
            .expect("the ar binary runs")
    };

    // A section the file does not carry is not permission to lose the one it does.
    seed(seeded);
    let wrong = run("limits");
    assert_eq!(code(&wrong), 1, "stderr: {}", stderr(&wrong));
    assert!(
        std::fs::read_to_string(&held)
            .expect("readable")
            .contains("oauth:"),
        "a mismatched section name must not unlock the loss"
    );

    // `all` is the blanket the operator reaches for after reading the refusal.
    seed(seeded);
    let all = run("all");
    assert_eq!(code(&all), 0, "stderr: {}", stderr(&all));
    assert!(
        !std::fs::read_to_string(&held)
            .expect("readable")
            .contains("oauth:"),
        "the blanket overwrite is a real overwrite, not a second refusal"
    );

    // Naming the section itself is enough, and is the narrower answer.
    seed(seeded);
    let named = run("oauth");
    assert_eq!(code(&named), 0, "stderr: {}", stderr(&named));
}

/// A target whose sections are all emittable imports as before: the guard must
/// not become a wall that refuses every overwrite.
#[test]
fn import_should_still_write_a_target_that_carries_no_unemittable_section() {
    let out = import_dir("import-clean-target");
    let src = litellm_source(&out);
    let held = out.join("config.yaml");
    std::fs::write(
        &held,
        "keys:\n  openai: $AR_KEY_OPENAI\nproviders:\n  - id: openai\n    key: openai\n\
         combos:\n  - id: default\n    strategy: lkgp\n    targets:\n      - openai/gpt-5.4\n",
    )
    .expect("the seed config writes");

    let res = Command::new(BIN)
        .args([
            "import",
            "--from",
            "litellm",
            "--path",
            src.to_str().expect("utf-8 source path"),
            "--out-dir",
            out.to_str().expect("utf-8 out dir"),
        ])
        .output()
        .expect("the ar binary runs");

    assert_eq!(code(&res), 0, "stderr: {}", stderr(&res));
    let body = std::fs::read_to_string(&held).expect("the config is readable");
    assert!(
        body.contains("# Generated by `aroute import`"),
        "the import really did rewrite the file: {body}"
    );
}

/// The refusal message must name **every** section at risk, each quoted, or an
/// agent reading it learns about one loss and not the other.
///
/// The join was originally `", `"`, which produced `` `server, `limits:` `` — one
/// name quoted after the comma and the other not. A malformed message is worse
/// than no message here, because the operator's next question ("what else did I
/// lose?") has no answer in the output.
#[test]
fn import_should_name_every_unemittable_section_with_each_one_quoted() {
    let out = import_dir("import-message-quotes-every-section");
    let src = litellm_source(&out);
    let held = out.join("config.yaml");
    std::fs::write(
        &held,
        "server:\n  host: 127.0.0.1\n  port: 20128\n\nkeys:\n  openai: $AR_KEY_OPENAI\n\
         combos:\n  - id: default\n    strategy: lkgp\n    targets:\n      - openai/gpt-5.4\n\
         limits:\n  default:\n    rpm: 60\n\noauth:\n  - provider: codex\n    refresh_key: cr\n",
    )
    .expect("the seed config writes");

    let res = Command::new(BIN)
        .args([
            "import",
            "--from",
            "litellm",
            "--path",
            src.to_str().expect("utf-8 source path"),
            "--out-dir",
            out.to_str().expect("utf-8 out dir"),
        ])
        .output()
        .expect("the ar binary runs");

    assert_eq!(code(&res), 1, "stderr: {}", stderr(&res));
    let text = stdout(&res);
    for section in ["`server:`", "`limits:`", "`oauth:`"] {
        assert!(
            text.contains(section),
            "the refusal must quote {section} so the operator sees the whole loss: {text}"
        );
    }
    // The malformed form: an opening backtick with no close before the next name.
    assert!(
        !text.contains("`server, `"),
        "a backtick must not leak before the separator: {text}"
    );
    assert!(
        !text.contains("which an import cannot write back`"),
        "the sentence must not end inside a quote: {text}"
    );
}

/// The three contract tests Phase 1 of the config-write surface names.
/// `aroute server show` answers the listen address, and the two questions that
/// follow it: is this the default, and what *is* the default.
#[test]
fn server_show_should_print_the_effective_listen_address() {
    let out = empty_dir("server-show");
    std::fs::write(
        out.join("config.yaml"),
        concat!(
            "server:\n  host: 127.0.0.1\n  port: 20128\n",
            "keys:\n  openai: sk-x\n",
            "combos:\n  - id: c\n    strategy: priority\n    targets:\n",
            "      - openai/gpt-5.4\n",
        ),
    )
    .expect("the fixture writes");

    let res = ar_in(&out, &["server", "show"]);
    assert_eq!(code(&res), 0, "stderr: {}", stderr(&res));
    let text = stdout(&res);
    assert!(text.contains("127.0.0.1"), "{text}");
    assert!(text.contains("20128"), "{text}");
    // The default is reported, not assumed: a caller comparing the address
    // against the default should not have to re-derive it from the source.
    assert!(
        text.contains("127.0.0.1:20128"),
        "the default must be named, not implied: {text}"
    );
    // Both fields are at the default in this fixture, so neither is flagged.
    assert!(
        text.contains("false,false"),
        "an unmodified address must not claim a change: {text}"
    );
}

/// The write moves the address and touches nothing else — asserted on the
/// bytes, because that is what makes it safe to hand to an agent.
#[test]
fn server_set_should_write_the_listen_address_and_leave_the_rest_byte_identical() {
    let out = empty_dir("server-set-byte-identical");
    let held = out.join("config.yaml");
    let body = "server:\n  host: 127.0.0.1\n  port: 20128\n\nkeys:\n  openai: sk-x\n\
                # A comment the section does not own, kept either way.\n\
                combos:\n  - id: c\n    strategy: priority\n    targets:\n      - openai/gpt-5.4\n";
    std::fs::write(&held, body).expect("the fixture writes");
    let before = body.to_owned();

    let res = ar_in(&out, &["server", "set", "--port", "20129"]);
    assert_eq!(code(&res), 0, "stderr: {}", stderr(&res));
    assert!(stdout(&res).contains("20129"), "{}", stdout(&res));
    // The restart note is the B-3 contract: the address is bound at boot.
    assert!(
        stdout(&res).contains("aroute service restart"),
        "a write that does not name the restart leaves an operator believing it is live: {}",
        stdout(&res)
    );

    let after = std::fs::read_to_string(&held).expect("readable");
    assert_eq!(
        after.lines().count(),
        before.lines().count(),
        "the section replacement must not add or drop lines:\n{after}"
    );
    assert_eq!(
        after.lines().filter(|l| l.trim() == "port: 20129").count(),
        1,
        "{after}"
    );
    assert!(
        after.contains("# A comment the section does not own, kept either way."),
        "a comment after the replaced section must survive: {after}"
    );
    // Per-field merge: only the port was passed, so the host is untouched.
    assert!(
        after.contains("host: 127.0.0.1"),
        "an unpassed flag must not change its field: {after}"
    );
}

/// A write `ar serve` would refuse to load is worse than a refusal, so both bad
/// values are refused *before* the file is touched.
#[test]
fn server_set_should_refuse_an_empty_host_and_name_the_fix() {
    let out = empty_dir("server-set-refusals");
    let held = out.join("config.yaml");
    std::fs::write(
        &held,
        "server:\n  host: 127.0.0.1\n  port: 20128\nkeys:\n  openai: sk-x\n\
         combos:\n  - id: c\n    strategy: priority\n    targets:\n      - openai/gpt-5.4\n",
    )
    .expect("the fixture writes");
    let before = std::fs::read_to_string(&held).expect("readable");

    // An empty host is refused, and the refusal says what a host looks like.
    let empty = ar_in(&out, &["server", "set", "--host", "   "]);
    assert_eq!(code(&empty), 1, "stderr: {}", stderr(&empty));
    assert!(
        stdout(&empty).contains("127.0.0.1"),
        "the help must name a valid value: {}",
        stdout(&empty)
    );

    // Port 0 is the one value `ar serve` will not bind.
    let zero = ar_in(&out, &["server", "set", "--port", "0"]);
    assert_eq!(code(&zero), 1, "stderr: {}", stderr(&zero));
    assert!(
        stdout(&zero).contains("1 and 65535"),
        "the help must give the range: {}",
        stdout(&zero)
    );

    // No field at all: `set` with nothing to set is a no-op that reads as
    // success, which is the shape this project refuses everywhere else.
    let none = ar_in(&out, &["server", "set"]);
    assert_eq!(code(&none), 1, "stderr: {}", stderr(&none));

    assert_eq!(
        std::fs::read_to_string(&held).expect("readable"),
        before,
        "a refused write must leave the file untouched"
    );
}

/// A section replacement must not swallow the blank line and comment block that
/// documents the *next* section.
///
/// Both are column-0 and neither is a section header, so the skip that removes
/// the replaced section's body ran past them. The repo's own `config.yaml`
/// carries those blocks, so a `server:` splice silently deleted the two comments
/// introducing `keys:`. Indentation cannot tell them apart — YAML allows a
/// column-0 comment anywhere — so `splice_section` has to make the distinction
/// explicitly, and this is the test that keeps it.
///
/// Driven through the real verb rather than `splice_section` directly: the
/// function is private to the bin crate, and an integration test asserting on it
/// would pass while the command still destroyed the comments.
#[test]
fn server_set_should_not_swallow_the_comment_block_after_the_section() {
    let out = empty_dir("server-set-keeps-comments");
    let held = out.join("config.yaml");
    // Not a line-continued literal: `\` inside a Rust string keeps the next
    // source line's indentation, which silently indents the YAML and changes
    // what this test is testing. `concat!` keeps each fragment's bytes exact.
    let body = concat!(
        "server:\n  host: 127.0.0.1\n  port: 20128\n\n",
        "# The credentials block: declared names, secrets stay in env.\n",
        "keys:\n  a: sk-x\n",
    );
    std::fs::write(&held, body).expect("the fixture writes");

    let res = ar_in(&out, &["server", "set", "--port", "20129"]);
    assert_eq!(
        code(&res),
        0,
        "stdout: {} stderr: {}",
        String::from_utf8_lossy(&res.stdout),
        stderr(&res)
    );

    let after = std::fs::read_to_string(&held).expect("readable");
    assert!(
        after.contains("# The credentials block: declared names, secrets stay in env."),
        "the comment block documenting the next section was eaten: {after}"
    );
    assert!(
        after.contains("keys:\n  a: sk-x"),
        "keys: was eaten: {after}"
    );
    assert!(
        after.contains("port: 20129"),
        "the write did not land: {after}"
    );
}

/// The contract tests Phase 2 of the config-write surface names.
///
/// `aroute providers add` is three writes that must land together — the
/// `providers:` row, the `keys:` reference and the `$AR_KEY_*` value in the env
/// file the service loads — because a config missing any one of the three is a
/// config `ar serve` refuses at boot. `remove` is the same set in reverse, with a
/// reference check standing in front of it.
///
/// Every case writes into a scratch directory beside the target's own tmpdir,
/// so the repo's `config.yaml` and its `ar.env` are never touched, and the
/// scratch directory is removed at the end of each test.
///
/// One `providers add`: the row, the `keys:` reference and the env line.
#[test]
fn providers_add_should_write_the_row_the_key_ref_and_the_env_line_together() {
    let out = empty_dir("providers-add");
    let held = out.join("config.yaml");
    std::fs::write(out.join("cred"), "sk-test-credential\n").expect("the credential file writes");
    std::fs::write(
        &held,
        concat!(
            "providers:\n",
            "  - id: openai\n",
            "    key: openai\n",
            "\n",
            "# The credentials block: declared names, secrets stay in env.\n",
            "keys:\n",
            "  openai: $AR_KEY_OPENAI\n",
        ),
    )
    .expect("the fixture writes");

    let res = ar_in(
        &out,
        &["providers", "add", "anthropic", "--credential-file", "cred"],
    );
    assert_eq!(
        code(&res),
        0,
        "stdout: {} stderr: {}",
        String::from_utf8_lossy(&res.stdout),
        stderr(&res)
    );

    let after = std::fs::read_to_string(&held).expect("readable");
    assert!(
        after.contains("  - id: anthropic\n    key: anthropic\n"),
        "the row was not written: {after}"
    );
    assert!(
        after.contains("  anthropic: $AR_KEY_ANTHROPIC\n"),
        "the keys: reference was not written: {after}"
    );
    assert!(
        after.contains("  openai: $AR_KEY_OPENAI\n"),
        "the existing keys: row was lost: {after}"
    );
    assert!(
        after.contains("# The credentials block: declared names, secrets stay in env."),
        "the comment block was eaten: {after}"
    );

    // The env file is the third write, and it holds the credential itself.
    let env = out.join("ar.env");
    let env_text = std::fs::read_to_string(&env).expect("the env file was written");
    assert!(
        env_text.contains("AR_KEY_ANTHROPIC="),
        "the env line was not written: {env_text}"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&env).expect("stat").permissions().mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "a file holding a credential must be owner-only, was {mode:o}"
        );
    }

    let _ = std::fs::remove_dir_all(&out);
}

/// An id the registry does not know is refused, and nothing is written.
#[test]
fn providers_add_should_refuse_an_id_the_registry_does_not_know() {
    let out = empty_dir("providers-add-unknown");
    let held = out.join("config.yaml");
    std::fs::write(out.join("cred"), "sk-test-credential\n").expect("the credential file writes");
    let before = "providers:\n  - id: openai\n    key: openai\n\nkeys:\n  openai: $AR_KEY_OPENAI\n";
    std::fs::write(&held, before).expect("the fixture writes");

    let res = ar_in(
        &out,
        &[
            "providers",
            "add",
            "definitely-not-a-provider",
            "--credential-file",
            "cred",
        ],
    );
    assert_eq!(code(&res), 1, "stdout: {}", stdout(&res));
    assert!(
        stdout(&res).contains("unknown provider"),
        "the refusal must name what was unknown: {}",
        stdout(&res)
    );
    assert_eq!(
        std::fs::read_to_string(&held).expect("readable"),
        before,
        "a refused add leaves the config alone"
    );
    assert!(
        !out.join("ar.env").exists(),
        "a refused add must not write the env file either"
    );

    let _ = std::fs::remove_dir_all(&out);
}

/// A combo that still references the provider refuses the removal.
#[test]
fn providers_remove_should_refuse_while_a_combo_references_the_provider() {
    let out = empty_dir("providers-remove-referenced");
    let held = out.join("config.yaml");
    let before = concat!(
        "providers:\n",
        "  - id: openai\n",
        "    key: openai\n",
        "  - id: anthropic\n",
        "    key: anthropic\n",
        "\n",
        "keys:\n",
        "  openai: $AR_KEY_OPENAI\n",
        "  anthropic: $AR_KEY_ANTHROPIC\n",
        "\n",
        "combos:\n",
        "  - id: c\n",
        "    strategy: priority\n",
        "    targets:\n",
        "      - openai/gpt-5.4\n",
    );
    std::fs::write(&held, before).expect("the fixture writes");

    let res = ar_in(&out, &["providers", "remove", "openai"]);
    assert_eq!(code(&res), 1, "stdout: {}", stdout(&res));
    assert!(
        stdout(&res).contains("still referenced by: c"),
        "the refusal must name the combo blocking it: {}",
        stdout(&res)
    );
    assert_eq!(
        std::fs::read_to_string(&held).expect("readable"),
        before,
        "a refused remove leaves the config alone"
    );

    let _ = std::fs::remove_dir_all(&out);
}

/// The entry the removed row alone held goes with it; a shared one stays.
#[test]
fn providers_remove_should_drop_the_keys_row_only_it_held() {
    let out = empty_dir("providers-remove-shared");
    let held = out.join("config.yaml");
    let before = concat!(
        "keys:\n",
        "  shared: $AR_KEY_SHARED\n",
        "\n",
        "providers:\n",
        "  - id: openai\n",
        "    key: shared\n",
        "  - id: anthropic\n",
        "    key: shared\n",
    );
    std::fs::write(&held, before).expect("the fixture writes");

    let res = ar_in(&out, &["providers", "remove", "openai"]);
    assert_eq!(
        code(&res),
        0,
        "stdout: {} stderr: {}",
        String::from_utf8_lossy(&res.stdout),
        stderr(&res)
    );

    let after = std::fs::read_to_string(&held).expect("readable");
    assert!(
        after.contains("  shared: $AR_KEY_SHARED\n"),
        "a keys: row a surviving provider still uses must stay: {after}"
    );
    assert!(
        after.contains("  - id: anthropic\n    key: shared\n"),
        "the survivor must stay: {after}"
    );
    assert!(
        !after.contains("  - id: openai\n"),
        "the removed row must be gone: {after}"
    );
    assert!(
        !after.contains("custom_providers:"),
        "a writer must not invent a section the operator did not have: {after}"
    );

    let _ = std::fs::remove_dir_all(&out);
}

/// A writer must be able to open a config whose other keys are not exported.
///
/// The proxy expands `$VAR` and refuses to boot on a missing one, so a reader
/// that expands cannot help the operator who is editing *because* a credential
/// is missing. The write verbs parse the file without expanding, which is also
/// the shape the panel uses.
#[test]
fn providers_add_should_work_on_a_config_whose_other_keys_are_unexported() {
    let out = empty_dir("providers-add-unexported");
    let held = out.join("config.yaml");
    std::fs::write(out.join("cred"), "sk-test-credential\n").expect("the credential file writes");
    std::fs::write(
        &held,
        concat!(
            "providers:\n",
            "  - id: openai\n",
            "    key: openai\n",
            "\n",
            "keys:\n",
            "  openai: $AR_KEY_OPENAI\n",
            "  groq: $AR_KEY_GROQ_NOT_SET_ANYWHERE\n",
        ),
    )
    .expect("the fixture writes");

    let res = ar_in(
        &out,
        &["providers", "add", "anthropic", "--credential-file", "cred"],
    );
    assert_eq!(
        code(&res),
        0,
        "a write must open a config with an unexported key: stdout {} stderr {}",
        String::from_utf8_lossy(&res.stdout),
        stderr(&res)
    );
    let after = std::fs::read_to_string(&held).expect("readable");
    assert!(
        after.contains("  anthropic: $AR_KEY_ANTHROPIC\n"),
        "the new binding landed: {after}"
    );
    assert!(
        after.contains("  groq: $AR_KEY_GROQ_NOT_SET_ANYWHERE\n"),
        "the untouched row with its unexported reference survived: {after}"
    );

    let _ = std::fs::remove_dir_all(&out);
}

/// The contract tests Phase 3 of the config-write surface names: an endpoint
/// the compiled-in registry does not carry, added and removed from the CLI with
/// the panel's own semantics.
///
/// `webui.rs`'s `provider_custom` is the tested shape and every rule asserted
/// here is that handler's rule — the id's character class, the two-protocol
/// vocabulary, the base-url scheme, the relative-path rule for `endpoint`, and
/// the catalog-collision refusal. The two differences the CLI carries are both
/// about the credential: it cannot be an argument (the process table), and a new
/// row must carry one because a keyless node fails every request.
#[test]
fn providers_add_custom_should_write_the_row_the_key_ref_and_the_env_line() {
    let out = empty_dir("providers-add-custom");
    let held = out.join("config.yaml");
    std::fs::write(out.join("cred"), "sk-gateway-credential\n")
        .expect("the credential file writes");
    std::fs::write(
        &held,
        concat!(
            "keys:\n",
            "  openai: $AR_KEY_OPENAI\n",
            "\n",
            "combos:\n",
            "  - id: c\n",
            "    strategy: priority\n",
            "    targets:\n",
            "      - openai/gpt-5.4\n",
        ),
    )
    .expect("the fixture writes");

    let res = ar_in(
        &out,
        &[
            "providers",
            "add-custom",
            "--id",
            "my-gateway",
            "--protocol",
            "openai-compatible",
            "--base-url",
            "https://gw.example/v1",
            "--endpoint",
            "chat/completions2",
            "--header",
            "X-Tenant=acme",
            "--credential-file",
            "cred",
        ],
    );
    assert_eq!(
        code(&res),
        0,
        "stdout: {} stderr: {}",
        String::from_utf8_lossy(&res.stdout),
        stderr(&res)
    );

    let after = std::fs::read_to_string(&held).expect("readable");
    // The row is in `custom_providers:`, not `providers:` — which is what makes
    // it an ad-hoc node rather than a catalog one.
    assert!(
        after.contains("custom_providers:\n  - id: my-gateway\n"),
        "the row was not written: {after}"
    );
    assert!(
        after.contains("protocol: openai-compatible"),
        "the protocol was not written: {after}"
    );
    assert!(
        after.contains("base_url: https://gw.example/v1"),
        "the base_url was not written: {after}"
    );
    assert!(
        after.contains("endpoint: chat/completions2"),
        "the endpoint override was not written: {after}"
    );
    assert!(
        after.contains("X-Tenant:\n      acme") || after.contains("X-Tenant: acme"),
        "the header was not written: {after}"
    );
    assert!(
        after.contains("  my-gateway: $AR_KEY_MY_GATEWAY\n"),
        "the keys: reference was not written: {after}"
    );
    assert!(
        after.contains("  openai: $AR_KEY_OPENAI\n"),
        "the existing keys: row was lost: {after}"
    );
    // The `keys:` row the operator wrote by hand keeps its place: the written
    // order is the section's contract, and a sorted re-render would move it.
    let keys_line = after
        .lines()
        .position(|l| l == "  openai: $AR_KEY_OPENAI")
        .expect("the original row survives");
    let new_line = after
        .lines()
        .position(|l| l == "  my-gateway: $AR_KEY_MY_GATEWAY")
        .expect("the new row is appended");
    assert!(
        new_line > keys_line,
        "the new row must be appended after the operator's own: {after}"
    );
    // No blank line or comment block that introduced `combos:` was consumed.
    assert!(
        after.contains("X-Tenant: acme\ncombos:\n"),
        "the row and `combos:` are adjacent: the block carries no separator that the removal could not undo: {after}"
    );
    // And the operator's own separator survived, above the block that took
    // its place. Which blank is whose is the insert's question and it is
    // answered by geometry, not by taste.
    assert!(
        after.contains("  my-gateway: $AR_KEY_MY_GATEWAY\n\ncustom_providers:\n"),
        "the paragraph break the operator wrote above `combos:` was eaten: {after}"
    );

    let env_text = std::fs::read_to_string(out.join("ar.env")).expect("the env file was written");
    assert!(
        env_text.contains("AR_KEY_MY_GATEWAY="),
        "the env line was not written: {env_text}"
    );

    let _ = std::fs::remove_dir_all(&out);
}

/// An id the catalog already uses is refused, because `Registry::merge` refuses
/// to shadow a compiled-in entry and a row written anyway is a row the service
/// refuses at boot.
#[test]
fn providers_add_custom_should_refuse_a_registry_id() {
    let out = empty_dir("providers-add-custom-collide");
    let held = out.join("config.yaml");
    std::fs::write(out.join("cred"), "sk-x\n").expect("the credential file writes");
    std::fs::write(
        &held,
        concat!("keys:\n", "  openai: $AR_KEY_OPENAI\n", "\n", "combos:\n"),
    )
    .expect("the fixture writes");

    let res = ar_in(
        &out,
        &[
            "providers",
            "add-custom",
            "--id",
            "openai",
            "--protocol",
            "openai-compatible",
            "--base-url",
            "https://gw.example/v1",
            "--credential-file",
            "cred",
        ],
    );
    assert_eq!(code(&res), 1, "must refuse, not write: {}", stderr(&res));
    let text = stdout(&res);
    assert!(
        text.contains("registry catalog id"),
        "the refusal names the collision: {text}"
    );
    let after = std::fs::read_to_string(&held).expect("readable");
    assert!(
        !after.contains("custom_providers:"),
        "nothing was written on a refusal: {after}"
    );

    let _ = std::fs::remove_dir_all(&out);
}

/// A custom endpoint is not a chat path by default: `--endpoint` is relative
/// because it is joined onto the base URL, and an absolute one is a wire that
/// cannot be reached.
#[test]
fn providers_add_custom_should_refuse_an_absolute_endpoint() {
    let out = empty_dir("providers-add-custom-endpoint");
    let held = out.join("config.yaml");
    std::fs::write(out.join("cred"), "sk-x\n").expect("the credential file writes");
    std::fs::write(&held, "keys:\n  openai: $AR_KEY_OPENAI\n\ncombos:\n")
        .expect("the fixture writes");

    let res = ar_in(
        &out,
        &[
            "providers",
            "add-custom",
            "--id",
            "my-gateway",
            "--protocol",
            "anthropic-compatible",
            "--base-url",
            "https://gw.example/v1",
            "--endpoint",
            "/v1/messages",
            "--credential-file",
            "cred",
        ],
    );
    assert_eq!(
        code(&res),
        1,
        "must refuse an absolute endpoint: {}",
        stderr(&res)
    );
    let text = stdout(&res);
    assert!(
        text.contains("relative path"),
        "the refusal names the rule: {text}"
    );

    let _ = std::fs::remove_dir_all(&out);
}

/// A new row with no credential is a node every request fails against, so the
/// refusal is the cheaper answer — the same argument `providers add` makes for
/// a row with no binding.
#[test]
fn providers_add_custom_should_refuse_a_new_row_with_no_credential() {
    let out = empty_dir("providers-add-custom-nokey");
    let held = out.join("config.yaml");
    std::fs::write(&held, "keys:\n  openai: $AR_KEY_OPENAI\n\ncombos:\n")
        .expect("the fixture writes");
    std::fs::write(out.join("cred"), "\n").expect("an empty credential file");

    let res = ar_in(
        &out,
        &[
            "providers",
            "add-custom",
            "--id",
            "my-gateway",
            "--protocol",
            "openai-compatible",
            "--base-url",
            "https://gw.example/v1",
            "--credential-file",
            "cred",
        ],
    );
    assert_eq!(code(&res), 1, "must refuse: {}", stderr(&res));
    let text = stdout(&res);
    assert!(
        text.contains("credential"),
        "the refusal names what is missing: {text}"
    );
    let after = std::fs::read_to_string(&held).expect("readable");
    assert!(
        !after.contains("custom_providers:"),
        "nothing was written on a refusal: {after}"
    );

    let _ = std::fs::remove_dir_all(&out);
}

/// An edit of an existing row keeps its own variable: re-pointing a hand-wired
/// `$AR_KEY_MINE` to the generated name would silently move where the operator
/// stores the secret.
#[test]
fn providers_add_custom_should_keep_the_binding_the_row_already_names() {
    let out = empty_dir("providers-add-custom-rebind");
    let held = out.join("config.yaml");
    // `my-gateway` is bound to a variable the canonical name would not produce,
    // so the row's binding must survive the base-url edit unchanged.
    std::fs::write(
        &held,
        concat!(
            "keys:\n",
            "  openai: $AR_KEY_OPENAI\n",
            "  my-gateway: $AR_KEY_MINE\n",
            "\n",
            "custom_providers:\n",
            "  - id: my-gateway\n",
            "    protocol: openai-compatible\n",
            "    base_url: https://old.example/v1\n",
            "    key_ref: my-gateway\n",
            "\n",
            "combos:\n",
        ),
    )
    .expect("the fixture writes");
    std::fs::write(out.join("cred"), "sk-rotated\n").expect("the credential file writes");

    let res = ar_in(
        &out,
        &[
            "providers",
            "add-custom",
            "--id",
            "my-gateway",
            "--protocol",
            "openai-compatible",
            "--base-url",
            "https://new.example/v1",
            "--credential-file",
            "cred",
        ],
    );
    assert_eq!(
        code(&res),
        0,
        "an edit must rotate the credential: stdout {} stderr {}",
        String::from_utf8_lossy(&res.stdout),
        stderr(&res)
    );
    let after = std::fs::read_to_string(&held).expect("readable");
    assert!(
        after.contains("base_url: https://new.example/v1"),
        "the base_url moved: {after}"
    );
    assert!(
        after.contains("  my-gateway: $AR_KEY_MINE\n"),
        "the binding the row already names was re-pointed: {after}"
    );
    let env_text = std::fs::read_to_string(out.join("ar.env")).expect("the env file was written");
    assert!(
        env_text.contains("AR_KEY_MINE="),
        "the credential went to the variable the row names: {env_text}"
    );

    let _ = std::fs::remove_dir_all(&out);
}

/// The full round trip on a hand-written config: add an ad-hoc endpoint with
/// an endpoint override and a header, then remove it, and the file must come
/// back byte-identical.
///
/// This is the test the naive implementations fail. Three separate edits left
/// residue: a bare `custom_providers:` header when the last row went, a
/// re-sort of the operator's own `keys:` rows, and a trailing newline the file
/// never had. Each was found by running the verbs against a copy of the repo's
/// real `config.yaml` and reading the diff.
#[test]
fn custom_providers_round_trip_must_come_back_byte_identical() {
    let out = empty_dir("custom-round-trip");
    let held = out.join("config.yaml");
    // The operator's own order, deliberately non-alphabetical, and no final
    // newline — both facts a naive writer normalises away.
    let original = concat!(
        "keys:\n",
        "  zzz-last: $AR_KEY_ZZZ\n",
        "  aaa-first: $AR_KEY_AAA\n",
        "\n",
        "combos:\n",
        "  - id: c\n",
        "    strategy: priority\n",
        "    targets:\n",
        "      - other/model\n",
    );
    std::fs::write(&held, original).expect("the fixture writes");
    std::fs::write(out.join("cred"), "sk-round-trip\n").expect("the credential file writes");

    let res = ar_in(
        &out,
        &[
            "providers",
            "add-custom",
            "--id",
            "relay",
            "--protocol",
            "anthropic-compatible",
            "--base-url",
            "https://relay.example/anthropic",
            "--endpoint",
            "chat/completions2",
            "--header",
            "X-Tenant=acme",
            "--credential-file",
            "cred",
        ],
    );
    assert_eq!(
        code(&res),
        0,
        "the add must succeed: stdout {} stderr {}",
        String::from_utf8_lossy(&res.stdout),
        stderr(&res)
    );
    // The row is really there, so the remove below is doing the work.
    let added = std::fs::read_to_string(&held).expect("readable");
    assert!(
        added.contains("  - id: relay\n"),
        "the row must be written for the round trip to prove anything: {added}"
    );

    let res = ar_in(&out, &["providers", "remove", "relay"]);
    assert_eq!(
        code(&res),
        0,
        "the remove must succeed: stdout {} stderr {}",
        String::from_utf8_lossy(&res.stdout),
        stderr(&res)
    );

    let after = std::fs::read_to_string(&held).expect("readable");
    assert_eq!(
        after,
        original,
        "a round trip must return the file to its exact bytes; leftover: {}",
        after
            .lines()
            .zip(original.lines())
            .find(|(a, b)| a != b)
            .map(|(a, b)| format!("{b:?} became {a:?}"))
            .unwrap_or_else(|| "a length difference only".to_owned())
    );

    let _ = std::fs::remove_dir_all(&out);
}

/// A `custom_providers:` section that is not there must not appear: the renderer
/// emits its header even for an empty row list, so an unconditional splice
/// writes a section the operator never had.
#[test]
fn custom_providers_remove_the_last_row_must_take_the_section() {
    let out = empty_dir("custom-remove-last");
    let held = out.join("config.yaml");
    std::fs::write(out.join("cred"), "sk-x\n").expect("the credential file writes");
    let original = concat!(
        "keys:\n",
        "  relay: $AR_KEY_RELAY\n",
        "\n",
        "# Providers the compiled-in registry does not carry: a self-hosted\n",
        "# gateway, a vendor compat shim, a local model server.\n",
        "custom_providers:\n",
        "  - id: relay\n",
        "    protocol: openai-compatible\n",
        "    base_url: https://relay.example/v1\n",
        "    key_ref: relay\n",
        "\n",
        "combos:\n",
    );
    std::fs::write(&held, original).expect("the fixture writes");

    let res = ar_in(&out, &["providers", "remove", "relay"]);
    assert_eq!(
        code(&res),
        0,
        "the remove must succeed: stdout {} stderr {}",
        String::from_utf8_lossy(&res.stdout),
        stderr(&res)
    );
    let after = std::fs::read_to_string(&held).expect("readable");
    assert!(
        !after.contains("custom_providers:\n  - id:"),
        "the row must be gone: {after}"
    );
    assert!(
        !after.lines().any(|l| l == "custom_providers:"),
        "an empty section header must not be left behind: {after}"
    );
    // The comment block documenting the section, and everything after it, is
    // the reason this is not `remove_section`: that helper skips until the next
    // header and would take 62 lines of documentation with it.
    assert!(
        after.contains("# Providers the compiled-in registry does not carry"),
        "the comment block was eaten: {after}"
    );
    assert!(after.contains("combos:\n"), "combos: was eaten: {after}");

    let _ = std::fs::remove_dir_all(&out);
}
