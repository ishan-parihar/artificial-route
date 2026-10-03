//! Pipeline strategy — chained stages with a reflect judge.
//!
//! A 1:1 port of `../OmniRoute/src/domain/pipeline.ts` plus the prompt table
//! from `../OmniRoute/src/domain/prompts.ts`, which is the `pipeline` half of
//! the `fusion|pipeline` row in the `docs/02` deferred table. The reference is
//! a pure engine: it makes no network calls and delegates every LLM call to a
//! caller-supplied [`StageExecutor`], and so does this.
//!
//! The stages chain through a small context object, not through a message
//! array: `plan` writes `plan_context`, `execute` reads it and writes
//! `execution_response`, `reflect` judges that and writes `reflection_response`,
//! `fix` applies the judgement. The `reflect` stage is the judge — it returns
//! structured JSON (`{"status":"pass"|"fail", …}`) and the engine's only
//! decision is whether to run `fix`.
//!
//! Provider selection is not this module's job: [`crate::pick`] ranks
//! candidates for [`Strategy::Pipeline`](crate::Strategy::Pipeline), and each
//! stage's [`FitnessTier`] is the hint it hands the caller.
//!
//! # Two deliberate deviations from the reference
//!
//! 1. **No clock.** The reference stamps `latencyMs` with `Date.now()` around
//!    the executor call. The engine here is pure and takes the measurement from
//!    the executor's [`StageExecutorResult`] instead, so a unit test needs no
//!    clock and the engine stays as testable as
//!    [`crate::attempt::classify_status`]. `None` means "not measured".
//! 2. **Borrowed stage args.** The reference builds a fresh two-element
//!    `messages` array per stage. [`StageExecutorArgs`] carries the same two
//!    turns as `&str`s, so a four-stage pipeline allocates no message vectors.
//!
//! # Example
//!
//! ```
//! use ar_route::{StageExecutor, StageExecutorArgs, StageExecutorResult, build_pipeline_config, execute_pipeline, TaskType};
//!
//! struct Echo;
//! impl StageExecutor for Echo {
//!     fn run(&self, args: &StageExecutorArgs<'_>) -> Result<StageExecutorResult, String> {
//!         Ok(StageExecutorResult::new(args.user))
//!     }
//! }
//!
//! let config = build_pipeline_config("2 + 2", TaskType::Math);
//! let result = execute_pipeline(&config, &Echo);
//!
//! assert!(!result.fallback);
//! assert_eq!(result.stages.len(), 2);
//! ```

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// One stage of a pipeline. Named as in `prompts.ts`; the order is not fixed,
/// only [`build_pipeline_config`] fixes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StageName {
    /// Produce a step-by-step plan for the request.
    Plan,
    /// Carry the request out, following any plan.
    Execute,
    /// Judge the execution output against the request.
    Reflect,
    /// Apply the reflect judgement.
    Fix,
}

impl StageName {
    /// The config/wire spelling of this stage.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Plan => "plan",
            Self::Execute => "execute",
            Self::Reflect => "reflect",
            Self::Fix => "fix",
        }
    }
}

/// Which model class a stage wants. A hint for the caller's provider
/// selection, not a selection: the engine never picks a provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FitnessTier {
    /// Strongest reasoning available.
    BestReasoning,
    /// Cheapest that can plausibly answer.
    Cheapest,
    /// Middle of the road.
    Moderate,
}

impl FitnessTier {
    /// The config/wire spelling of this tier.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::BestReasoning => "best-reasoning",
            Self::Cheapest => "cheapest",
            Self::Moderate => "moderate",
        }
    }
}

/// What kind of work the request is, which picks the stage template.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TaskType {
    /// Code generation: plan, execute, reflect, fix.
    Code,
    /// Math: execute, reflect.
    Math,
    /// General reasoning: execute, reflect.
    Reasoning,
    /// Creative writing: execute, reflect.
    Creative,
    /// Ordinary work: execute only.
    Medium,
    /// Trivial work: execute only, cheapest tier.
    Simple,
}

/// One configured stage: what it is, what model class it wants, and an optional
/// system-prompt override.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PipelineStage {
    /// Which stage this is.
    pub stage: StageName,
    /// Model class hint for the caller's provider selection.
    pub fitness_tier: FitnessTier,
    /// Replaces the rendered system prompt wholesale.
    pub system_override: Option<String>,
}

impl PipelineStage {
    /// A stage with no override.
    #[must_use]
    pub const fn new(stage: StageName, fitness_tier: FitnessTier) -> Self {
        Self {
            stage,
            fitness_tier,
            system_override: None,
        }
    }
}

/// A whole pipeline: the stages, and the request they all serve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PipelineConfig {
    /// Stages to execute, in order.
    pub stages: Vec<PipelineStage>,
    /// The original user request.
    pub request: String,
    /// The task type the stages were derived from.
    pub task_type: TaskType,
}

/// The verdict the `reflect` judge returned, or `None` when the stage did not
/// run or its output was unparseable.
///
/// Unparseable is deliberately folded into the same `None` as "not
/// applicable": the engine treats a parse failure as a conservative *fail*
/// (see [`execute_pipeline`]), so an unparsed judge is never read as approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The output satisfies the request; skip `fix`.
    Pass,
    /// The output has issues; run `fix`.
    Fail,
}

/// What one stage produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageResult {
    /// Which stage this was.
    pub stage: StageName,
    /// Stage output text. Empty when skipped or errored.
    pub text: String,
    /// Provider that served it, when the executor reported one.
    pub provider: Option<String>,
    /// Wall time in ms, when the executor measured it. The engine has no clock.
    pub latency_ms: Option<u64>,
    /// Input tokens, when reported.
    pub input_tokens: Option<u32>,
    /// Output tokens, when reported.
    pub output_tokens: Option<u32>,
    /// Whether the stage was skipped (only ever `fix` after a `pass`).
    pub skipped: bool,
    /// Failure text, when the stage errored. Its presence sets `fallback`.
    pub error: Option<String>,
}

impl StageResult {
    /// A stage that did not run.
    fn skipped(stage: StageName) -> Self {
        Self {
            stage,
            text: String::new(),
            provider: None,
            latency_ms: None,
            input_tokens: None,
            output_tokens: None,
            skipped: true,
            error: None,
        }
    }
}

/// The outcome of a whole pipeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PipelineResult {
    /// Final output: the best text any stage produced.
    pub text: String,
    /// Per-stage results, in execution order.
    pub stages: Vec<StageResult>,
    /// Whether any stage failed.
    pub fallback: bool,
    /// The judge's verdict, when one was reached.
    pub reflect_verdict: Option<Verdict>,
}

/// What the caller is asked to run for one stage.
///
/// Borrowed throughout: the engine renders the prompt and hands over slices of
/// it, so no per-stage message vector is allocated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StageExecutorArgs<'a> {
    /// System turn: the rendered template, or the stage's override.
    pub system: &'a str,
    /// User turn: the rendered template with the context interpolated.
    pub user: &'a str,
    /// Always `false`: a stage is one complete answer, not a stream.
    pub stream: bool,
    /// Model class hint for provider selection.
    pub fitness_tier: FitnessTier,
}

/// What one stage call returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageExecutorResult {
    /// The model's answer.
    pub text: String,
    /// Provider that served it.
    pub provider: Option<String>,
    /// Input tokens consumed.
    pub input_tokens: Option<u32>,
    /// Output tokens produced.
    pub output_tokens: Option<u32>,
    /// Wall time in ms, measured by the caller.
    pub latency_ms: Option<u64>,
}

impl StageExecutorResult {
    /// Just the text, nothing else reported.
    #[must_use]
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            provider: None,
            input_tokens: None,
            output_tokens: None,
            latency_ms: None,
        }
    }
}

/// Runs one stage. Implemented by the caller, which owns provider selection and
/// the network.
///
/// Generic at the call site (`execute_pipeline` takes `&E`), so dispatch is
/// static and there is no vtable — the engine is a hot path and ch.6 reserves
/// `dyn` for heterogeneous lists.
pub trait StageExecutor {
    /// Runs one stage.
    ///
    /// # Errors
    /// Returns the failure text to record on the [`StageResult`]. Any error
    /// ends the pipeline: later stages are not run, because a chain built on a
    /// stage that produced nothing is not a chain.
    fn run(&self, args: &StageExecutorArgs<'_>) -> Result<StageExecutorResult, String>;
}

// ---------------------------------------------------------------------------
// Pipeline templates per task type
// ---------------------------------------------------------------------------

/// Stage template per task type, ported from `TASK_STAGES` in `pipeline.ts`.
///
/// A const slice rather than a `HashMap<TaskType, …>`: a lookup table keyed by a
/// six-variant enum is a linear scan over six items, and a const needs no
/// allocator and no `LazyLock`.
const TASK_STAGES: &[(TaskType, &[(StageName, FitnessTier)])] = &[
    (
        TaskType::Code,
        &[
            (StageName::Plan, FitnessTier::BestReasoning),
            (StageName::Execute, FitnessTier::Cheapest),
            (StageName::Reflect, FitnessTier::Moderate),
            (StageName::Fix, FitnessTier::Cheapest),
        ],
    ),
    (
        TaskType::Math,
        &[
            (StageName::Execute, FitnessTier::BestReasoning),
            (StageName::Reflect, FitnessTier::Moderate),
        ],
    ),
    (
        TaskType::Reasoning,
        &[
            (StageName::Execute, FitnessTier::BestReasoning),
            (StageName::Reflect, FitnessTier::Moderate),
        ],
    ),
    (
        TaskType::Creative,
        &[
            (StageName::Execute, FitnessTier::Moderate),
            (StageName::Reflect, FitnessTier::BestReasoning),
        ],
    ),
    (
        TaskType::Medium,
        &[(StageName::Execute, FitnessTier::Moderate)],
    ),
    (
        TaskType::Simple,
        &[(StageName::Execute, FitnessTier::Cheapest)],
    ),
];

/// Builds the default pipeline for a task type and request.
#[must_use]
pub fn build_pipeline_config(request: impl Into<String>, task_type: TaskType) -> PipelineConfig {
    let template = TASK_STAGES
        .iter()
        .find(|(t, _)| *t == task_type)
        .map_or(&[][..], |(_, stages)| *stages);

    PipelineConfig {
        stages: template
            .iter()
            .map(|&(stage, fitness_tier)| PipelineStage::new(stage, fitness_tier))
            .collect(),
        request: request.into(),
        task_type,
    }
}

// ---------------------------------------------------------------------------
// Prompt rendering
// ---------------------------------------------------------------------------

/// The two rendered prompt turns for one stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StagePrompt<'a> {
    /// System turn.
    pub system: &'a str,
    /// User turn.
    pub user: &'a str,
}

/// The context one stage threads into the next, as the reference's
/// `Record<string, string>` does — but with the three known keys as fields
/// rather than a hash map, so a missing key is a `None` and not a lookup.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct StageContext {
    plan: Option<String>,
    execution: Option<String>,
    reflection: Option<String>,
}

/// System prompt per stage, ported verbatim from `STAGE_PROMPTS`.
const PLAN_SYSTEM: &str = "You are a planning assistant. Analyze the user's request and produce \
                           a clear, step-by-step execution plan. Break complex tasks into atomic \
                           steps. Identify dependencies, constraints, and potential failure \
                           points. Output the plan as numbered steps with brief explanations.";
const PLAN_USER: &str = "Create a detailed execution plan for the following request.\nRequest: \
                         {original_request}";
const EXECUTE_SYSTEM: &str = "You are a capable assistant. Execute the given task accurately and \
                              completely. Follow any provided plan precisely. Produce clear, \
                              well-structured output.";
const EXECUTE_USER: &str = "{plan_context}\nRequest: {original_request}";
const REFLECT_SYSTEM: &str = "You are a quality reviewer. Evaluate the execution output against \
                              the original request. You MUST respond with a JSON object in \
                              exactly this format:\n{\"status\":\"pass\",\"confirmation\":\"<brief \
                              explanation of why the output satisfies the request>\"}\nOR\n\
                              {\"status\":\"fail\",\"issues\":[\"<issue 1>\",\"<issue \
                              2>\"],\"corrected\":\"<corrected output>\"}\nBe strict: only \
                              mark pass if the output fully satisfies the request. If there are \
                              any issues, omissions, or errors, mark as fail and provide a \
                              corrected version.";
const REFLECT_USER: &str = "Original request: {original_request}\n\nExecution output:\n\
                            {execution_response}\n\nEvaluate the output and respond with the \
                            required JSON format.";
const FIX_SYSTEM: &str = "You are a corrective assistant. The previous execution had issues \
                          identified during review. Apply the corrections and improvements \
                          specified in the reflection. Produce a final, polished output that \
                          addresses all identified issues.";
const FIX_USER: &str = "Original request: {original_request}\n\nReflection feedback:\n\
                        {reflection_response}\n\nProduce the corrected output.";

/// Renders one stage's prompt, interpolating `request` and whatever context
/// earlier stages produced.
///
/// An unknown or unset placeholder is left verbatim, matching the reference's
/// `key in variables ? variables[key] : match`. That matters for `fix`: when the
/// judge never ran, the literal `{reflection_response}` reaches the model and
/// the stage visibly did not get its input, instead of silently receiving an
/// empty turn.
fn render_prompt(stage: StageName, request: &str, context: &StageContext) -> (String, String) {
    let (system, user) = match stage {
        StageName::Plan => (PLAN_SYSTEM, PLAN_USER),
        StageName::Execute => (EXECUTE_SYSTEM, EXECUTE_USER),
        StageName::Reflect => (REFLECT_SYSTEM, REFLECT_USER),
        StageName::Fix => (FIX_SYSTEM, FIX_USER),
    };
    (
        interpolate(system, request, context),
        interpolate(user, request, context),
    )
}

/// Substitutes `{name}` for the matching context value, or leaves it in place.
fn interpolate(template: &str, request: &str, context: &StageContext) -> String {
    let mut out = String::with_capacity(template.len() + request.len());
    let mut rest = template;

    while let Some(start) = rest.find('{') {
        let Some(end_offset) = rest[start + 1..].find('}') else {
            break;
        };
        let end = start + 1 + end_offset;
        out.push_str(&rest[..start]);

        let replacement = match &rest[start + 1..end] {
            "original_request" => Some(request),
            "plan_context" => context.plan.as_deref(),
            "execution_response" => context.execution.as_deref(),
            "reflection_response" => context.reflection.as_deref(),
            _ => None,
        };
        match replacement {
            // An empty value still substitutes: the placeholder resolved, and
            // the stage is told it has no prior output rather than being handed
            // a literal that would look like content.
            Some(value) => out.push_str(value),
            None => out.push_str(&rest[start..=end]),
        }
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    out
}

// ---------------------------------------------------------------------------
// Reflect JSON parsing
// ---------------------------------------------------------------------------

/// The judge's structured answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReflectResult {
    /// The output satisfies the request.
    Pass {
        /// Why, in the judge's words.
        confirmation: String,
    },
    /// The output has issues.
    Fail {
        /// What is wrong, one entry per issue.
        issues: Vec<String>,
        /// The judge's corrected output.
        corrected: String,
    },
}

impl ReflectResult {
    /// The verdict this answer carries.
    #[must_use]
    pub const fn verdict(&self) -> Verdict {
        match self {
            Self::Pass { .. } => Verdict::Pass,
            Self::Fail { .. } => Verdict::Fail,
        }
    }
}

/// Parses the `reflect` stage's output as structured JSON.
///
/// Returns `None` when the output cannot be parsed, which
/// [`execute_pipeline`] reads as a conservative *fail*: a judge whose answer
/// cannot be read is not a judge that approved the work.
///
/// Two shapes are accepted, as in the reference: a fenced ```` ```json ```` block
/// and a bare object. Anything else — a bare string, an array, a truncated
/// object — is `None`.
#[must_use]
pub fn parse_reflect_json(text: &str) -> Option<ReflectResult> {
    let json_str = strip_code_fence(text.trim())?;
    let parsed: serde_json::Value = serde_json::from_str(&json_str).ok()?;
    let obj = parsed.as_object()?;

    match obj.get("status")?.as_str()? {
        "pass" => Some(ReflectResult::Pass {
            confirmation: obj
                .get("confirmation")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        }),
        "fail" => Some(ReflectResult::Fail {
            issues: obj
                .get("issues")
                .and_then(serde_json::Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(serde_json::Value::as_str)
                        .map(ToOwned::to_owned)
                        .collect()
                })
                .unwrap_or_default(),
            corrected: obj
                .get("corrected")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        }),
        _ => None,
    }
}

/// Isolates the JSON inside `text`, from either a fenced block or the first
/// `{` to the last `}`.
///
/// A port of `JSON_BLOCK_RE` / `JSON_OBJECT_RE` without a regex dependency: the
/// fence is "starts with ``` then everything up to the next ```", which is what
/// the optional `json` tag and `\s*` in the reference amount to.
fn strip_code_fence(text: &str) -> Option<String> {
    let candidate = if let Some(body) = text.strip_prefix("```") {
        // Drop the fence's info string (`json` or nothing) up to the first
        // newline, then everything before the closing fence.
        let body = match body.find('\n') {
            Some(nl) => &body[nl + 1..],
            None => body,
        };
        match body.find("```") {
            Some(end) => body[..end].trim(),
            None => body.trim(),
        }
    } else {
        // First `{` through last `}`, greedy — the reference's `[\s\S]*` is.
        let start = text.find('{')?;
        let end = text.rfind('}')?;
        text.get(start..=end)?.trim()
    };

    (!candidate.is_empty()).then(|| candidate.to_owned())
}

// ---------------------------------------------------------------------------
// Pipeline execution
// ---------------------------------------------------------------------------

/// Executes a multi-stage pipeline.
///
/// After the `reflect` stage the engine parses the judge's JSON:
///
/// * `pass` -> `fix` is skipped.
/// * `fail` -> `fix` runs, seeded with the judge's `corrected` output.
/// * unparseable -> treated as `fail`, so the chain still produces a repaired
///   answer rather than shipping the unreviewed one.
///
/// Any stage failure sets `fallback` and ends the loop; the best text produced
/// so far is still returned, which is the whole point of a fallback.
///
/// This cannot fail: the only fallible thing is the caller's executor, and that
/// becomes a [`StageResult::error`].
///
/// `Strategy::Pipeline` ([`crate::Strategy::Pipeline`]) picks the provider for a
/// stage; this runs the stage. Keeping the two apart is why
/// [`StageExecutorArgs`] carries a [`FitnessTier`] rather than a provider.
pub fn execute_pipeline<E: StageExecutor>(config: &PipelineConfig, executor: &E) -> PipelineResult {
    let mut results: Vec<StageResult> = Vec::with_capacity(config.stages.len());
    let mut context = StageContext::default();
    let mut fallback = false;
    let mut reflect_verdict = None;

    for stage in &config.stages {
        // A judge that approved the work means there is nothing to fix.
        if stage.stage == StageName::Fix && reflect_verdict == Some(Verdict::Pass) {
            results.push(StageResult::skipped(StageName::Fix));
            continue;
        }

        let result = execute_stage(stage, &config.request, &context, executor);
        let stage_name = stage.stage;
        let failed = result.error.is_some();
        let text = result.text.clone();
        results.push(result);

        // A chain built on a stage that produced nothing is not a chain.
        if failed {
            fallback = true;
            break;
        }

        match stage_name {
            StageName::Plan => context.plan = Some(text),
            StageName::Execute => context.execution = Some(text),
            StageName::Reflect => {
                context.reflection = Some(text);
                match parse_reflect_json(context.reflection.as_deref().unwrap_or_default()) {
                    // Conservative: an unreadable judge is a fail, never a pass.
                    None => reflect_verdict = Some(Verdict::Fail),
                    Some(verdict) => {
                        reflect_verdict = Some(verdict.verdict());
                        if let ReflectResult::Fail { corrected, .. } = &verdict
                            && !corrected.is_empty()
                        {
                            context.execution = Some(corrected.clone());
                        }
                    }
                }
            }
            // Only a fix that said something replaces the judge's correction.
            // The reference assigns unconditionally, which overwrites the
            // correction with an empty string and makes its own correction
            // branch unreachable — `fix` runs on every `fail`, so the value it
            // clobbers is the only one that could have answered. Guarding on
            // non-empty is the same rule the Anthropic adapter uses for a turn
            // left with no text.
            StageName::Fix => {
                if !text.is_empty() {
                    context.execution = Some(text);
                }
            }
        }
    }

    let text = best_text(&results, reflect_verdict, &context);

    PipelineResult {
        text,
        stages: results,
        fallback,
        reflect_verdict,
    }
}

/// Runs one stage, turning a caller error into a [`StageResult::error`].
fn execute_stage<E: StageExecutor>(
    stage: &PipelineStage,
    request: &str,
    context: &StageContext,
    executor: &E,
) -> StageResult {
    let (system, user) = render_prompt(stage.stage, request, context);
    let system = stage.system_override.as_deref().unwrap_or(&system);
    let args = StageExecutorArgs {
        system,
        user: &user,
        stream: false,
        fitness_tier: stage.fitness_tier,
    };

    match executor.run(&args) {
        Ok(out) => StageResult {
            stage: stage.stage,
            text: out.text,
            provider: out.provider,
            latency_ms: out.latency_ms,
            input_tokens: out.input_tokens,
            output_tokens: out.output_tokens,
            skipped: false,
            error: None,
        },
        Err(error) => StageResult {
            stage: stage.stage,
            text: String::new(),
            provider: None,
            latency_ms: None,
            input_tokens: None,
            output_tokens: None,
            skipped: false,
            error: Some(error),
        },
    }
}

/// Picks the best available output: `fix` > the judge's correction >
/// `execute` > the last successful stage.
///
/// Every step is a truthiness test on the text, not on presence, so a stage that
/// "succeeded" with an empty answer does not shadow a real one — the same
/// short-circuit the reference gets from JavaScript's `||`. The correction
/// survives to this function only because the `fix` stage does not overwrite it
/// with an empty answer; see the `StageName::Fix` arm in
/// [`execute_pipeline`].
fn best_text(
    results: &[StageResult],
    reflect_verdict: Option<Verdict>,
    context: &StageContext,
) -> String {
    let usable = |r: &StageResult| !r.skipped && r.error.is_none();

    results
        .iter()
        .find(|r| r.stage == StageName::Fix && usable(r))
        .map(|r| r.text.as_str())
        .filter(|t| !t.is_empty())
        .or_else(|| {
            // The judge's correction lives in `context.execution` because the
            // reference threads it there. Guarded by the verdict because the
            // same field is the ordinary `execute` output when the judge
            // approved and `fix` was skipped.
            (reflect_verdict == Some(Verdict::Fail))
                .then_some(context.execution.as_deref())
                .flatten()
                .filter(|t| !t.is_empty())
        })
        .or_else(|| {
            results
                .iter()
                .find(|r| r.stage == StageName::Execute && usable(r))
                .map(|r| r.text.as_str())
                .filter(|t| !t.is_empty())
        })
        .or_else(|| {
            results
                .iter()
                .rev()
                .find(|r| usable(r))
                .map(|r| r.text.as_str())
                .filter(|t| !t.is_empty())
        })
        .unwrap_or_default()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::{
        FitnessTier, PipelineConfig, PipelineStage, ReflectResult, StageExecutor,
        StageExecutorArgs, StageExecutorResult, StageName, TaskType, Verdict,
        build_pipeline_config, execute_pipeline, parse_reflect_json,
    };

    /// A stage executor that replays a fixed script, one answer per stage call,
    /// so a test can drive the judge without a network. Holds no clock, per the
    /// module's purity note.
    ///
    /// The cursor is a `Cell` because the engine takes `&E`: a pipeline is
    /// sequential, so the executor never needs interior mutability for
    /// concurrency, only for the borrow.
    struct Scripted {
        answers: Vec<String>,
        next: Cell<usize>,
        fail_at: Option<usize>,
    }

    impl Scripted {
        fn new(answers: &[&str]) -> Self {
            Self {
                answers: answers.iter().map(|s| (*s).to_owned()).collect(),
                next: Cell::new(0),
                fail_at: None,
            }
        }

        /// Fails on the `n`th call; every earlier call answers with `""`.
        fn failing_at(n: usize) -> Self {
            Self {
                answers: vec![String::new(); n],
                next: Cell::new(0),
                fail_at: Some(n),
            }
        }
    }

    impl StageExecutor for Scripted {
        fn run(&self, _args: &StageExecutorArgs<'_>) -> Result<StageExecutorResult, String> {
            let index = self.next.get();
            self.next.set(index + 1);
            if self.fail_at == Some(index) {
                return Err("stage failed".to_owned());
            }
            Ok(StageExecutorResult::new(
                self.answers.get(index).cloned().unwrap_or_default(),
            ))
        }
    }

    fn code_pipeline() -> PipelineConfig {
        PipelineConfig {
            stages: vec![
                PipelineStage::new(StageName::Plan, FitnessTier::BestReasoning),
                PipelineStage::new(StageName::Execute, FitnessTier::Cheapest),
                PipelineStage::new(StageName::Reflect, FitnessTier::Moderate),
                PipelineStage::new(StageName::Fix, FitnessTier::Cheapest),
            ],
            request: "add two numbers".to_owned(),
            task_type: TaskType::Code,
        }
    }

    fn pass_json() -> &'static str {
        r#"{"status":"pass","confirmation":"looks right"}"#
    }

    fn fail_json() -> &'static str {
        r#"{"status":"fail","issues":["off by one"],"corrected":"5"}"#
    }

    #[test]
    fn chains_stages_when_pipeline() {
        let config = code_pipeline();

        let result = execute_pipeline(
            &config,
            &Scripted::new(&["the plan", "2 + 2 = 4", fail_json(), "2 + 2 = 4"]),
        );

        assert_eq!(
            result.stages.iter().map(|s| s.stage).collect::<Vec<_>>(),
            vec![
                StageName::Plan,
                StageName::Execute,
                StageName::Reflect,
                StageName::Fix
            ]
        );
    }

    #[test]
    fn skips_fix_when_reflect_passes() {
        let config = code_pipeline();

        let result = execute_pipeline(
            &config,
            &Scripted::new(&["the plan", "2 + 2 = 4", pass_json(), "unreached"]),
        );

        assert_eq!(result.reflect_verdict, Some(Verdict::Pass));
        assert!(result.stages.last().is_some_and(|s| s.skipped));
    }

    #[test]
    fn ends_pipeline_when_stage_errors() {
        let config = code_pipeline();

        let result = execute_pipeline(&config, &Scripted::failing_at(0));

        assert!(result.fallback);
        assert_eq!(result.stages.len(), 1);
    }

    #[test]
    fn treats_unparseable_verdict_as_fail_when_judge_silent() {
        let config = code_pipeline();

        let result = execute_pipeline(
            &config,
            &Scripted::new(&["the plan", "2 + 2 = 4", "I approve!", "2 + 2 = 4"]),
        );

        assert_eq!(result.reflect_verdict, Some(Verdict::Fail));
    }

    #[test]
    fn prefers_fix_output_when_reflect_fails() {
        let config = code_pipeline();

        let result = execute_pipeline(
            &config,
            &Scripted::new(&["the plan", "2 + 2 = 4", fail_json(), "2 + 2 = 4, corrected"]),
        );

        assert_eq!(result.text, "2 + 2 = 4, corrected");
    }

    #[test]
    fn falls_back_to_judge_correction_when_fix_says_nothing() {
        let config = code_pipeline();

        let result = execute_pipeline(
            &config,
            &Scripted::new(&["the plan", "2 + 2 = 4", fail_json(), ""]),
        );

        assert_eq!(result.text, "5");
    }

    #[test]
    fn parses_pass_verdict_when_fenced() {
        let text = "Here you go:\n```json\n{\"status\":\"pass\",\"confirmation\":\"ok\"}\n```";

        let got = parse_reflect_json(text);

        assert_eq!(
            got,
            Some(ReflectResult::Pass {
                confirmation: "ok".to_owned()
            })
        );
    }

    #[test]
    fn parses_fail_verdict_when_fenced() {
        let text = "```\n{\"status\":\"fail\",\"issues\":[\"a\",\"b\",7],\"corrected\":\"c\"}\n```";

        let got = parse_reflect_json(text);

        assert_eq!(
            got,
            Some(ReflectResult::Fail {
                issues: vec!["a".to_owned(), "b".to_owned()],
                corrected: "c".to_owned(),
            })
        );
    }

    #[test]
    fn returns_none_when_verdict_not_json() {
        assert_eq!(parse_reflect_json("looks good to me"), None);
    }

    #[test]
    fn builds_two_stages_when_task_math() {
        let config = build_pipeline_config("2+2", TaskType::Math);

        assert_eq!(config.stages.len(), 2);
    }

    #[test]
    fn builds_one_stage_when_task_simple() {
        let config = build_pipeline_config("hi", TaskType::Simple);

        assert_eq!(config.stages.len(), 1);
    }
}
