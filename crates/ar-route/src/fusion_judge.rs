//! Fusion judge — the synthesis half of the `fusion` strategy.
//!
//! [`crate::dispatch_fusion`] fans the prompt out to a panel and returns the
//! first panel member's answer, which is honest but is not fusion: the
//! reference then hands the whole panel to a `judgeModel` that analyzes the
//! responses and writes one merged answer, and that synthesis is where most of
//! fusion's quality lift comes from (`fusion.ts::handleFusionChat` +
//! `buildJudgePrompt`, #14533).
//!
//! Two things this module needs and the earlier port recorded as missing are
//! here now: the **body composer** (panel answers are collected into one text
//! turn rather than left as a `ChunkStream`) and the **place to name the judge
//! model** (a [`JudgeTarget`] naming the provider, dispatched through the same
//! [`Executor`] the panel used).
//!
//! # Async, unlike `pipeline.rs`
//!
//! The judge is a real upstream call, so [`synthesize`] is `async` over
//! [`Executor`] rather than sync over the pipeline crate's [`StageExecutor`].
//! Blocking on that trait from a tokio worker would stall the runtime — and
//! `block_on` inside a worker is a panic, not a stall — so the seam here is the
//! same async [`Executor::call`] the panel fan-out already uses, and the judge's
//! body is buffered once to read its text.
//!
//! # Deviations from the reference, all deliberate
//!
//! 1. **The prompt is ported verbatim**, including the anonymized `[Source N]`
//!    numbering that prevents brand bias — the judge's quality depends on the
//!    directive being the upstream one.
//! 2. **No quorum or straggler grace here.** Those are wall-clock concerns
//!    belonging to the fan-out ([`crate::dispatch_fusion`], which enforces the
//!    panel ceiling); a judge given four answers behaves the same as one given
//!    two, which is the seam this module is.
//! 3. **A single survivor is returned unjudged.** Synthesizing one answer
//!    through a judge is the reference's *degradation*, not its fusion, and
//!    spending a second dispatch on it would be theatre. The outcome says so
//!    rather than pretending a judgment happened.
//!
//! # Example
//!
//! ```
//! use ar_route::{CanonicalRequest, ExecError, Executor, JudgePanel, JudgeTarget, ProviderId, Upstream, synthesize};
//! use futures::StreamExt;
//! use std::future::Future;
//! use std::pin::Pin;
//!
//! struct Panel;
//! impl Executor for Panel {
//!     fn call<'a>(
//!         &'a self,
//!         _p: &'a ProviderId,
//!         _c: &'a CanonicalRequest,
//!     ) -> Pin<Box<dyn Future<Output = Result<Upstream, ExecError>> + Send + 'a>>
//!     {
//!         Box::pin(async move {
//!             let body = serde_json::json!({"choices": [{"message": {"content": "42"}}]}).to_string();
//!             Ok(Upstream::success(Box::pin(futures::stream::iter([bytes::Bytes::from(body)]))))
//!         })
//!     }
//! }
//!
//! # futures::executor::block_on(async {
//! let panel = JudgePanel::new([("a", "42"), ("b", "43")]);
//! let out = synthesize(&panel, &JudgeTarget::provider("judge"), "how many rungs", &Panel).await;
//! assert!(out.judged);
//! # });
//! ```

use bytes::Bytes;
use futures::StreamExt;

use crate::contract::{CanonicalRequest, ExecError, Executor, ProviderId, Upstream};

/// The hard ceiling on fan-out members, ported from
/// `fusion.ts::FUSION_DEFAULTS.maxPanel` (#1905).
///
/// Every panel member is asked in parallel and its full response buffered at
/// once; the reference rejected oversized panels up front rather than OOM. The
/// same ceiling is enforced by [`crate::dispatch_fusion`] before any call.
pub const MAX_PANEL: usize = 40;

/// The judge directive, ported verbatim from `fusion.ts::buildJudgePrompt`.
///
/// Kept byte-for-byte: the anonymized-source instruction and the "not a
/// vote-counter" clause are the parts that make synthesis better than picking
/// the longest answer, and paraphrasing either is a silent behaviour change.
const JUDGE_DIRECTIVE: &str = "\
You are the JUDGE in a model-fusion panel. {count} expert models independently answered the user's \
most recent request. Their responses are below, anonymized by source.

Do NOT mention that multiple models were used, and do NOT refer to the sources. Produce ONE \
authoritative final answer addressed directly to the user.

First, internally analyze the panel along these dimensions: consensus (points most sources agree \
on — usually higher-confidence, but NOT automatically correct), contradictions (where they \
disagree — resolve with your own judgment), partial coverage, unique insights only one source \
surfaced, and blind spots every source missed.

You are not a vote-counter, and the panel is not a ceiling — treat it as strong evidence, not as \
the limit of what you may say. Apply your OWN reasoning and knowledge as a full participant: if the \
consensus is wrong, incomplete, or outdated, override it and state what is correct; if every \
source missed something you know, add it; if a lone source is right against the majority, side \
with it. Do not water down a correct answer to match panel agreement. The only hard limit is \
honesty — do not assert facts you are not confident about.

Then write the best possible final answer — more complete and correct than any single response, \
and than the panel as a whole — with no filler.

=== PANEL RESPONSES ===
{panel}
=== END PANEL RESPONSES ===

Now write the final answer to the user's original request.";

/// Where the judge runs.
///
/// One variant because the reference has exactly one knob: `combo.config.judgeModel`.
/// A caller that wants the panel's own leader to judge names that provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JudgeTarget {
    /// A named provider serves the synthesis.
    Provider(ProviderId),
}

impl JudgeTarget {
    /// The provider named for synthesis.
    #[must_use]
    pub fn provider(id: impl AsRef<str>) -> Self {
        Self::Provider(ProviderId::new(id))
    }

    /// The provider this target dispatches to.
    #[must_use]
    pub fn provider_id(&self) -> &ProviderId {
        match self {
            Self::Provider(id) => id,
        }
    }
}

/// The panel's answers, each labelled with the provider that produced it.
///
/// The provider name is metadata for the operator's trace; it is deliberately
/// absent from the judge prompt, which sees only `[Source N]`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JudgePanel {
    /// `(provider, answer text)` pairs, in panel order.
    pub answers: Vec<(ProviderId, String)>,
}

impl JudgePanel {
    /// Builds a panel from `(provider, text)` pairs.
    #[must_use]
    pub fn new<I, P, T>(answers: I) -> Self
    where
        I: IntoIterator<Item = (P, T)>,
        P: AsRef<str>,
        T: Into<String>,
    {
        Self {
            answers: answers
                .into_iter()
                .map(|(p, t)| (ProviderId::new(p.as_ref()), t.into()))
                .collect(),
        }
    }

    /// Number of answers held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.answers.len()
    }

    /// Whether the panel holds no answers at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.answers.is_empty()
    }

    /// How many answers the judge will actually see: blank turns dropped.
    ///
    /// An empty turn reads to a judge as "this model had nothing to say", which
    /// is a different claim from "this model failed" — and the reference's own
    /// panel only holds answers.
    #[must_use]
    pub fn usable_count(&self) -> usize {
        self.answers
            .iter()
            .filter(|(_, text)| !text.trim().is_empty())
            .count()
    }

    /// The judge prompt: directive plus the anonymized panel.
    #[must_use]
    pub fn judge_prompt(&self) -> String {
        let sources = self
            .answers
            .iter()
            .filter(|(_, text)| !text.trim().is_empty())
            .enumerate()
            .map(|(i, (_, text))| format!("[Source {}]\n{}", i + 1, text.trim()))
            .collect::<Vec<_>>()
            .join("\n\n");
        JUDGE_DIRECTIVE
            .replace("{count}", &self.usable_count().to_string())
            .replace("{panel}", &sources)
    }
}

/// What one synthesis produced.
// `Debug` and `Default` only: `Upstream` carries a boxed stream and has no
// `Clone`/`PartialEq`, so an outcome holding one cannot derive them — comparing
// syntheses would mean consuming the responses, which is not a comparison.
#[derive(Debug, Default)]
pub struct JudgeOutcome {
    /// The judge's own response, relayed verbatim: real ids, real usage, real
    /// finish reason, real tool-call blocks. `None` when no judge ran or its
    /// call failed; the caller relays a panel answer in that case rather than
    /// nothing.
    ///
    /// Never re-rendered into the inbound dialect: a body the router invented is
    /// a body whose accounting fields are invented too, and a judge that
    /// answered with a tool call would arrive as prose.
    pub upstream: Option<Upstream>,
    /// Status the judge answered with, when it answered at all.
    pub status: Option<u16>,
    /// Whether a judge call actually happened. `false` for a panel too small to
    /// synthesize, which is a decision, not a failure.
    pub judged: bool,
    /// Why no synthesis happened or why it failed. The panel answers still
    /// stand, so this is a degradation to report, not an error to propagate.
    pub error: Option<String>,
}

/// Runs the judge's second dispatch over a panel.
///
/// The judge request is non-streaming and carries the composed prompt as its
/// only turn, which is the shape `appendUserTurn` produces upstream: the
/// directive plus the anonymized panel.
///
/// # Errors
/// None. Every failure — a panel too small, a refused call, a non-2xx answer —
/// is reported on [`JudgeOutcome`] with `judged: false`, because the panel
/// answers are still a usable answer and propagating would throw away a
/// completed fan-out over one synthesis call.
pub async fn synthesize<E: Executor + ?Sized>(
    panel: &JudgePanel,
    judge: &JudgeTarget,
    request: &str,
    exec: &E,
) -> JudgeOutcome {
    if panel.usable_count() < 2 {
        return JudgeOutcome {
            error: Some(format!(
                "a panel of {} live answers needs no synthesis; the answer is returned unjudged",
                panel.usable_count()
            )),
            ..JudgeOutcome::default()
        };
    }
    let body = match serde_json::to_vec(&serde_json::json!({
        "model": judge.provider_id().as_str(),
        "stream": false,
        "messages": [
            {
                "role": "user",
                "content": {
                    "task": request,
                    "directive": JUDGE_DIRECTIVE
                        .replace("{count}", &panel.usable_count().to_string())
                        .replace("{panel}", &synthesized_sources(panel)),
                },
            },
        ],
    })) {
        Ok(body) => body,
        // A serde failure on a literal object is unreachable; reporting it as a
        // degradation keeps the function's no-error contract honest.
        Err(e) => {
            return JudgeOutcome {
                error: Some(format!("judge prompt could not be composed: {e}")),
                ..JudgeOutcome::default()
            };
        }
    };
    let canonical = CanonicalRequest::new(judge.provider_id().as_str(), Bytes::from(body));
    let upstream = match exec.call(judge.provider_id(), &canonical).await {
        Ok(upstream) => upstream,
        Err(ExecError(e)) => {
            return JudgeOutcome {
                error: Some(format!("judge call failed: {e}")),
                ..JudgeOutcome::default()
            };
        }
    };
    let status = upstream.status.as_u16();
    if !upstream.status.is_success() {
        return JudgeOutcome {
            status: Some(status),
            error: Some(format!("judge answered {status}")),
            ..JudgeOutcome::default()
        };
    }
    // The body is peeked, never consumed: the judge may have answered with an
    // unreadable or empty body, which is a synthesis that did not happen; the
    // bytes themselves are relayed as they arrived.
    let peeked = match read_all(upstream.stream).await {
        Ok(peeked) => peeked,
        Err(e) => {
            return JudgeOutcome {
                status: Some(status),
                error: Some(format!("judge body could not be read: {e}")),
                ..JudgeOutcome::default()
            };
        }
    };
    if extract_panel_text(&peeked).trim().is_empty() {
        // A 2xx the judge wrote nothing readable in is not a synthesis: the
        // caller must fall back to a panel answer rather than relay `{}`.
        return JudgeOutcome {
            status: Some(status),
            error: Some("judge body carried no readable text".to_owned()),
            ..JudgeOutcome::default()
        };
    }
    JudgeOutcome {
        upstream: Some(Upstream {
            stream: Box::pin(futures::stream::iter([peeked])),
            ..upstream
        }),
        status: Some(status),
        judged: true,
        error: None,
    }
}

/// The assistant text of one completion, across the four wire shapes the
/// dispatcher speaks.
///
/// A 1:1 port of `fusion.ts::extractPanelText`, minus the reference's reuse of
/// the translator's `extractTextContent`: this crate's four shapes are
/// enumerated, and a shape this build cannot parse reads as no text rather than
/// as a guess. Empty means "nothing to synthesize from", which is what a failed
/// or empty panel answer is.
#[must_use]
pub fn extract_panel_text(body: &[u8]) -> String {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(body) else {
        return String::new();
    };
    // OpenAI chat: content on the message, or on a delta for a chunk-shaped body.
    // Each branch yields only when it found *non-blank* text, because a body can
    // carry an empty `content` on an early shape and real text on a later one —
    // returning the empty match would short-circuit the fallbacks below.
    if let Some(choice) = value.pointer("/choices/0")
        && let Some(turn) = choice.get("message").or_else(|| choice.get("delta"))
        && let Some(text) = content_text(turn.get("content")).filter(|t| !t.trim().is_empty())
    {
        return text;
    }
    // Anthropic messages: text blocks under the reply's own `content`.
    if let Some(text) = content_text(value.get("content")).filter(|t| !t.trim().is_empty()) {
        return text;
    }
    // Gemini: parts carry bare `text`, with no type discriminator.
    if let Some(parts) = value.pointer("/candidates/0/content/parts").and_then(|p| p.as_array())
    {
        let joined = parts
            .iter()
            .filter_map(|p| p.get("text").and_then(serde_json::Value::as_str))
            .collect::<String>();
        if !joined.trim().is_empty() {
            return joined;
        }
    }
    // OpenAI Responses: output items each carry a content array of `{text}`.
    if let Some(output) = value.get("output").and_then(|o| o.as_array()) {
        let joined = output
            .iter()
            .filter_map(|item| item.get("content").and_then(|c| c.as_array()))
            .flatten()
            .filter_map(|c| c.get("text").and_then(serde_json::Value::as_str))
            .collect::<String>();
        if !joined.trim().is_empty() {
            return joined;
        }
    }
    String::new()
}

/// One `content` value as text: a plain string, or the array of typed
/// `{type: "text"}` blocks every shape uses for multi-part replies. Takes the
/// content field itself, not the turn around it — `{"message": {"content": …}}`
/// reaches this as `message.content`, because a turn object is not text in any
/// dialect.
fn content_text(value: Option<&serde_json::Value>) -> Option<String> {
    let value = value?;
    if let Some(text) = value.as_str() {
        return Some(text.to_owned());
    }
    let blocks = value.as_array()?;
    let joined = blocks
        .iter()
        .filter_map(|block| {
            block
                .as_str()
                .or_else(|| block.get("text").and_then(serde_json::Value::as_str))
        })
        .collect::<String>();
    (!joined.trim().is_empty()).then_some(joined)
}

/// The anonymized panel block, numbered from 1 over live answers only.
fn synthesized_sources(panel: &JudgePanel) -> String {
    panel
        .answers
        .iter()
        .filter(|(_, text)| !text.trim().is_empty())
        .enumerate()
        .map(|(i, (_, text))| format!("[Source {}]\n{}", i + 1, text.trim()))
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Buffers one upstream body. The judge's own request is non-streaming, so this
/// is one body, read once — the same cost the non-stream chat arm already pays.
async fn read_all(mut stream: crate::contract::ChunkStream) -> Result<Bytes, ExecError> {
    let mut out = Vec::new();
    while let Some(chunk) = stream.next().await {
        out.extend_from_slice(&chunk);
    }
    Ok(Bytes::from(out))
}

#[cfg(test)]
mod tests {
    use super::{JudgePanel, JudgeTarget, synthesize};
    use crate::contract::{CanonicalRequest, ExecError, Executor, ProviderId, Upstream};
    use bytes::Bytes;
    use http::StatusCode;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Mutex;

    /// A judge that records the body it was handed and replies with a scripted
    /// status.
    struct ScriptedJudge {
        seen: Mutex<String>,
        reply: String,
        fail: bool,
        status: u16,
    }

    impl ScriptedJudge {
        fn ok() -> Self {
            Self {
                seen: Mutex::new(String::new()),
                reply: r#"{"choices":[{"message":{"content":"the answer is 42"}}]}"#.to_owned(),
                fail: false,
                status: 200,
            }
        }

        fn failing() -> Self {
            Self {
                seen: Mutex::new(String::new()),
                reply: String::new(),
                fail: true,
                status: 200,
            }
        }

        fn body(&self) -> String {
            self.seen.lock().expect("recorder").clone()
        }
    }

    impl Executor for ScriptedJudge {
        fn call<'a>(
            &'a self,
            _provider: &'a ProviderId,
            canonical: &'a CanonicalRequest,
        ) -> Pin<Box<dyn Future<Output = Result<Upstream, ExecError>> + Send + 'a>> {
            *self.seen.lock().expect("recorder") =
                String::from_utf8_lossy(&canonical.body).into_owned();
            if self.fail {
                return Box::pin(async { Err(ExecError("judge refused".to_owned())) });
            }
            let reply = self.reply.clone();
            let status = self.status;
            Box::pin(async move {
                let code = StatusCode::from_u16(status).unwrap_or(StatusCode::OK);
                if code.is_success() {
                    let chunk = Bytes::from(reply);
                    Ok(Upstream::success(Box::pin(futures::stream::iter([chunk]))))
                } else {
                    Ok(Upstream::failure(code, Bytes::new(), None))
                }
            })
        }
    }

    /// Provider labels are `vendor-*` and the panel text is prose about an
    /// unrelated subject, so "the provider id never reaches the judge prompt" is
    /// a claim about the body and not an accident of shared fixture strings.
    fn panel() -> JudgePanel {
        JudgePanel::new([
            ("vendor-alpha", "rusted metal has poor conductivity"),
            ("vendor-beta", "pure copper conducts better than alloy"),
        ])
    }

    fn judge_target() -> JudgeTarget {
        JudgeTarget::provider("judge-1")
    }

    #[test]
    fn a_synthesis_reaches_the_judge_as_one_user_turn() {
        futures::executor::block_on(async {
            let judge = ScriptedJudge::ok();
            let out = synthesize(&panel(), &judge_target(), "which metal conducts?", &judge).await;
            assert!(out.judged, "a two-answer panel must synthesize: {out:?}");
            assert_eq!(out.status, Some(200));
            let seen = judge.body();
            assert!(seen.contains("[Source 1]"), "sources are numbered: {seen}");
            assert!(seen.contains("[Source 2]"), "{seen}");
            assert!(seen.contains("rusted metal has poor conductivity"), "{seen}");
            assert!(seen.contains("pure copper conducts better than alloy"), "{seen}");
            assert!(
                seen.contains("which metal conducts?"),
                "the original request reaches the judge: {seen}"
            );
        });
    }

    /// Reads the body a judged outcome relays, so an assertion is about the
    /// bytes the client will actually receive.
    async fn relayed_body(out: super::JudgeOutcome) -> String {
        use futures::StreamExt;
        let mut upstream = out.upstream.expect("a judged outcome carries a body");
        let mut bytes = Vec::new();
        while let Some(chunk) = upstream.stream.next().await {
            bytes.extend_from_slice(&chunk);
        }
        String::from_utf8_lossy(&bytes).into_owned()
    }

    #[test]
    fn the_judges_own_body_is_relayed_verbatim() {
        // The judge's bytes go to the client as they arrived: real ids, real
        // usage, real finish reason. Re-rendering them in the inbound dialect
        // would invent every one of those fields.
        futures::executor::block_on(async {
            let judge = ScriptedJudge::ok();
            let out = synthesize(&panel(), &judge_target(), "q", &judge).await;
            assert_eq!(relayed_body(out).await, judge.reply, "the body is the judge's own");
        });
    }

    #[test]
    fn a_judged_answer_survives_the_extractor_check() {
        // The readability check gates synthesis, so a body the extractor cannot
        // read must not be reported as judged — and the check itself must agree
        // with what is relayed.
        futures::executor::block_on(async {
            let judge = ScriptedJudge::ok();
            let out = synthesize(&panel(), &judge_target(), "q", &judge).await;
            assert_eq!(
                super::extract_panel_text(relayed_body(out).await.as_bytes()),
                "the answer is 42",
                "the same text the extractor gated on"
            );
        });
    }

    #[test]
    fn a_judge_that_answers_with_no_readable_text_does_not_count_as_synthesis() {
        // A 2xx carrying `{}` is not an answer. Reporting it as one would hand
        // the client an empty fusion where a panel answer stood ready.
        futures::executor::block_on(async {
            let judge = ScriptedJudge {
                seen: Mutex::new(String::new()),
                reply: "{}".to_owned(),
                fail: false,
                status: 200,
            };
            let out = synthesize(&panel(), &judge_target(), "q", &judge).await;
            assert!(!out.judged);
            assert!(out.upstream.is_none(), "an unreadable judge body is not relayed");
            assert_eq!(
                out.error.as_deref(),
                Some("judge body carried no readable text")
            );
        });
    }

    #[test]
    fn panel_text_is_read_from_every_wire_shape_the_dispatcher_speaks() {
        // `extractPanelText`, 1:1: OpenAI chat, an OpenAI chunk's delta,
        // Anthropic messages blocks, Gemini parts, and Responses output items.
        for (label, body, want) in [
            (
                "openai chat",
                r#"{"choices":[{"message":{"content":"copper wins"}}]}"#,
                "copper wins",
            ),
            (
                "openai delta",
                r#"{"choices":[{"delta":{"content":"streamed text"}}]}"#,
                "streamed text",
            ),
            (
                "anthropic",
                r#"{"content":[{"type":"text","text":"claude text"}]}"#,
                "claude text",
            ),
            (
                "gemini",
                r#"{"candidates":[{"content":{"parts":[{"text":"gem "},{"text":"text"}]}}]}"#,
                "gem text",
            ),
            (
                "responses",
                r#"{"output":[{"content":[{"type":"output_text","text":"resp text"}]}]}"#,
                "resp text",
            ),
        ] {
            assert_eq!(
                super::extract_panel_text(body.as_bytes()),
                want,
                "{label} shape"
            );
        }
    }

    #[test]
    fn an_unreadable_or_empty_body_is_no_text_rather_than_a_guess() {
        assert_eq!(super::extract_panel_text(b"not json"), "");
        assert_eq!(super::extract_panel_text(b"{}"), "");
        assert_eq!(super::extract_panel_text(br#"{"choices":[]}"#), "");
        assert_eq!(
            super::extract_panel_text(br#"{"choices":[{"message":{"content":"  "}}]}"#),
            "",
            "whitespace is not an answer"
        );
    }

    #[test]
    fn the_judge_never_sees_which_provider_answered() {
        futures::executor::block_on(async {
            let judge = ScriptedJudge::ok();
            synthesize(&panel(), &judge_target(), "q", &judge).await;
            let seen = judge.body();
            assert!(!seen.contains("vendor-alpha"), "provider leaked: {seen}");
            assert!(!seen.contains("vendor-beta"), "provider leaked: {seen}");
        });
    }

    #[test]
    fn the_directive_travels_with_the_panel_and_states_the_count() {
        futures::executor::block_on(async {
            let judge = ScriptedJudge::ok();
            synthesize(&panel(), &judge_target(), "q", &judge).await;
            let seen = judge.body();
            assert!(
                seen.contains("2 expert models independently answered"),
                "the count is interpolated: {seen}"
            );
            assert!(seen.contains("=== PANEL RESPONSES ==="), "{seen}");
            assert!(
                seen.contains("You are not a vote-counter"),
                "the upstream directive is verbatim: {seen}"
            );
        });
    }

    #[test]
    fn a_lone_panel_answer_is_returned_unjudged() {
        futures::executor::block_on(async {
            let judge = ScriptedJudge::ok();
            let lone = JudgePanel::new([("vendor-alpha", "the only answer")]);
            let out = synthesize(&lone, &judge_target(), "q", &judge).await;
            assert!(!out.judged, "one answer needs no second dispatch");
            assert!(out.upstream.is_none(), "the caller relays the panel answer");
            assert!(out.error.is_some(), "the reason is reported");
            assert!(judge.body().is_empty(), "the judge must not run at all");
        });
    }

    #[test]
    fn empty_answers_do_not_count_toward_synthesis() {
        futures::executor::block_on(async {
            let judge = ScriptedJudge::ok();
            let p = JudgePanel::new([
                ("vendor-alpha", "the real answer"),
                ("vendor-beta", "   "),
            ]);
            assert_eq!(p.usable_count(), 1);
            assert_eq!(p.len(), 2, "the failed member is still in the trace");
            let out = synthesize(&p, &judge_target(), "q", &judge).await;
            assert!(!out.judged);
            assert!(judge.body().is_empty());
        });
    }

    #[test]
    fn an_empty_panel_is_unjudged_and_says_why() {
        futures::executor::block_on(async {
            let judge = ScriptedJudge::ok();
            let out = synthesize(&JudgePanel::default(), &judge_target(), "q", &judge).await;
            assert!(!out.judged);
            assert!(out.upstream.is_none());
            assert!(
                out.error.as_deref().is_some_and(|e| e.contains("needs no synthesis")),
                "the reason is reported: {out:?}"
            );
        });
    }

    #[test]
    fn a_refused_judge_degrades_instead_of_losing_the_panel() {
        futures::executor::block_on(async {
            let judge = ScriptedJudge::failing();
            let out = synthesize(&panel(), &judge_target(), "q", &judge).await;
            assert!(!out.judged, "a refused judge did not judge");
            assert_eq!(out.error.as_deref(), Some("judge call failed: judge refused"));
            assert!(out.upstream.is_none(), "the caller falls back to a panel answer");
        });
    }

    #[test]
    fn a_non_2xx_judge_is_reported_with_its_status() {
        futures::executor::block_on(async {
            let judge = ScriptedJudge {
                seen: Mutex::new(String::new()),
                reply: String::new(),
                fail: false,
                status: 503,
            };
            let out = synthesize(&panel(), &judge_target(), "q", &judge).await;
            assert!(!out.judged);
            assert_eq!(out.status, Some(503));
            assert_eq!(out.error.as_deref(), Some("judge answered 503"));
        });
    }

    #[test]
    fn the_prompt_helper_agrees_with_the_dispatched_body() {
        // One directive, two renderings: the helper a caller may read and the
        // body the judge receives must not drift apart.
        let prompt = panel().judge_prompt();
        let judge = ScriptedJudge::ok();
        futures::executor::block_on(async {
            synthesize(&panel(), &judge_target(), "q", &judge).await;
        });
        let body = judge.body();
        for needle in ["[Source 1]", "=== PANEL RESPONSES ===", "2 expert models"] {
            assert!(prompt.contains(needle), "helper lost {needle}: {prompt}");
            assert!(body.contains(needle), "body lost {needle}: {body}");
        }
    }

    #[test]
    fn the_fan_out_ceiling_is_the_references_forty() {
        // #1905's OOM ceiling is a documented constant, and the fan-out reads
        // it rather than restating the number.
        assert_eq!(crate::fusion_judge::MAX_PANEL, 40);
    }
}
