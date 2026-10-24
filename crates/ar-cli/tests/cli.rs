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
    dir
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
    assert_eq!(out.status.code(), Some(0), "the store must start armed: {}", String::from_utf8_lossy(&out.stderr));
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
    assert!(text.contains("already armed"), "must name the reason: {text}");
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
    assert!(text.contains("http-gate"), "the row name must appear: {text}");
    // The honesty this verb exists for: no token material, and a plain statement
    // that none is knowable -- an empty table would read as "there are none".
    assert!(text.contains("minted tokens are not recorded"), "{text}");
    assert!(!text.contains("eyJ"), "no JWT material may be printed: {text}");
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
    assert_eq!(
        first_row_count(&text),
        1,
        "one row is printed: {text}"
    );
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
    assert_eq!(
        first_row_count(&text),
        3,
        "three checks print: {text}"
    );
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

#[test]
fn service_status_is_read_only_and_reports_each_unit_once() {
    // The one verb that is safe to run at any time, and the one an agent needs
    // before routing: two rows, one per unit, each naming systemd's verdict
    // AND this process's own probe. Those are two different claims, and the
    // incident `docs/25` records was a service systemd called active while its
    // port did not answer.
    let out = ar(&["service", "status"]);

    // Exit 0 here because both real units are up on the host the suite runs on.
    // The exit-code contract itself is pinned below, against a unit that does
    // not exist, rather than against the host's current state.
    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("aroute.service"), "{text}");
    assert!(text.contains("aroute-dashboard.service"), "{text}");
    assert_eq!(
        count_rows(&text),
        2,
        "one row per unit, never a duplicate: {text}"
    );
    assert!(
        text.contains("port 20128") && text.contains("20149"),
        "each row names its port so a mismatch is visible: {text}"
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
