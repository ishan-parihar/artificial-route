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

/// A directory under the crate's own target tree holding nothing but whatever the
/// caller writes into it.
///
/// Per-test rather than shared: `cargo test` runs these in parallel threads in one
/// process, and two tests sharing a `config.yaml` clobber each other. Hand-rolled
/// rather than a `tempfile` dev-dependency — it is one `mkdir`, and adding a crate
/// to assert `ar` will not read `config.yaml` is not a trade worth making.
fn empty_dir(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    std::fs::create_dir_all(&dir).expect("the temp dir is created");
    let _ = std::fs::remove_file(dir.join("config.yaml"));
    dir
}
