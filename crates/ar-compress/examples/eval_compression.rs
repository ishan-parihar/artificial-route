//! `eval:compression` — score a compression plan's fidelity against its savings.
//!
//! ```sh
//! cargo run -p ar-compress --release --example eval_compression
//! cargo run -p ar-compress --release --example eval_compression -- stacked --budget 200
//! ```
//!
//! Prints the fidelity-vs-savings table for [`SEED_CORPUS`] under the resolved
//! plan. The run is offline and deterministic: no network, no model call, no
//! clock, no RNG, so two runs of the same revision print byte-identical
//! numbers.
//!
//! An example rather than a `[[bin]]`: the `ar` binary belongs to `ar-cli`, and
//! a second compression entry point there is a merge waiting to happen.

use std::process::ExitCode;

use ar_compress::eval::{self, SEED_CORPUS};
use ar_compress::{Combo, Engine, Layers, Plan, Source, Step, registered};

/// Engines available to a named combo, in the order they run.
const STACKED: &[Step] = &[
    Step::new(Engine::Lite),
    Step::new(Engine::Rtk),
    Step::new(Engine::Caveman),
];

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let name = args.next().unwrap_or_else(|| "off".to_owned());
    // `--budget 200` and a bare `200` are both accepted: an example that
    // rejects the flag a reader is likely to type is a worse example.
    let budget_arg = match args.next().as_deref() {
        Some("--budget") => args.next(),
        other => other.map(str::to_owned),
    };
    let budget = budget_arg.map(|raw| {
        raw.parse::<u32>().unwrap_or_else(|_| {
            eprintln!("eval:compression: budget must be a whole number of tokens, got {raw:?}");
            std::process::exit(2);
        })
    });

    let combos = [Combo {
        id: "stacked",
        name: Some("Stacked"),
        steps: STACKED,
    }];
    let plan = match name.as_str() {
        "off" => Plan::off(Source::Default),
        "lite" => single(Engine::Lite),
        "rtk" => single(Engine::Rtk),
        "caveman" => single(Engine::Caveman),
        "stacked" => ar_compress::plan_resolution(
            &combos,
            &Layers {
                header: Some("stacked"),
                ..Layers::default()
            },
        ),
        other => {
            eprintln!("eval:compression: unknown plan {other:?}; expected off|lite|rtk|caveman|stacked");
            return ExitCode::from(2);
        }
    };

    println!("# eval:compression");
    println!();
    println!("- plan: {}", describe(&plan));
    println!("- corpus: {} cases, synthetic seed corpus", SEED_CORPUS.len());
    println!(
        "- hard budget: {}",
        budget.map_or_else(|| "off (engines only)".to_owned(), |b| format!("{b} tokens"))
    );
    println!();

    match eval::run(SEED_CORPUS, &plan, registered(), budget) {
        Ok(report) => {
            print!("{}", eval::report_table(&report));
            println!();
            println!(
                "{} tokens -> {} tokens; mean fidelity {:.4}.",
                report.before_total(),
                report.after_total(),
                report.fidelity_mean
            );
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("eval:compression: {err}");
            ExitCode::FAILURE
        }
    }
}

fn describe(plan: &Plan) -> String {
    if plan.is_off() {
        return format!("off (source={})", plan.source);
    }
    let steps = plan
        .steps
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(" -> ");
    format!("{steps} (source={})", plan.source)
}

fn single(engine: Engine) -> Plan {
    Plan {
        steps: vec![Step::new(engine)],
        source: Source::Default,
    }
}
