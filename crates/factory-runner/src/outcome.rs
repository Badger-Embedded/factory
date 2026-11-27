use factory_core::{
    AgentResult, DiffVerdict, Event, Gap, PlannedPr, PrDescription, Question, Stage, Task,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::stream::ResultMessage;

/// What an agent run must end with, which depends on the job's stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expect {
    Clarify,
    Spec,
    Plan,
    Commits,
    /// Like `Commits`, and the run may also end with `rerun_ci`.
    FixCi,
    /// The LLM review pass. It ends with `approve`, or `changes` with
    /// feedback. The other stages instead end with `questions` or their own
    /// done outcome.
    Review,
    /// The quick review of a picked pull request. It ends with a `score`
    /// from 1 to 5 and the `review` text, and never asks a question.
    QuickReview,
    /// The failure analysis of a Renovate pull request. It ends with
    /// `transient` or `caused_by_update`, each with a reason, and never asks
    /// a question.
    FailureAnalysis,
    /// The diff check of a Renovate pull request. It ends with `safe` or
    /// `unsafe`, with a reason and a summary, and never asks a question.
    DiffCheck,
    /// The packaging analysis after a merged Renovate pull request. It ends
    /// with `gaps`, a list that may be empty, and never asks a question.
    PackagingGaps,
}

impl Expect {
    /// `None` for a stage in which no agent runs.
    #[must_use]
    pub fn for_stage(stage: &Stage) -> Option<Self> {
        match stage {
            Stage::Clarifying => Some(Expect::Clarify),
            Stage::Spec => Some(Expect::Spec),
            Stage::Plan => Some(Expect::Plan),
            Stage::Implementing { .. } | Stage::Verifying { .. } | Stage::CiAndReview { .. } => {
                Some(Expect::Commits)
            }
            _ => None,
        }
    }

    /// What the run of `task` in `stage` must end with. A `FixCi` task may
    /// also end with `rerun_ci`, and a `Review` task ends with an `approve`
    /// or `changes` verdict of its own.
    #[must_use]
    pub fn for_task(stage: &Stage, task: &Task) -> Option<Self> {
        match (stage, task) {
            (Stage::Renovate, Task::AnalyzeFailure { .. }) => return Some(Expect::FailureAnalysis),
            (Stage::Renovate, Task::CheckDiff { .. }) => return Some(Expect::DiffCheck),
            (Stage::Packaging, Task::FindGaps { .. }) => return Some(Expect::PackagingGaps),
            _ => {}
        }
        match (Self::for_stage(stage)?, task) {
            (Expect::Commits, Task::FixCi { .. }) => Some(Expect::FixCi),
            (_, Task::Review) => Some(Expect::Review),
            (_, Task::QuickReview) => Some(Expect::QuickReview),
            (expect, _) => Some(expect),
        }
    }

    /// The outcomes a run may end with. Every stage run may end with
    /// questions; a review run ends with `approve` or `changes`, the two
    /// Renovate runs with their verdicts, and the packaging analysis with
    /// `gaps`.
    #[must_use]
    pub fn outcomes(self) -> Vec<&'static str> {
        let done = match self {
            Expect::Clarify => "clarified",
            Expect::Spec => "spec",
            Expect::Plan => "plan",
            Expect::Commits | Expect::FixCi => "commits_ready",
            Expect::Review => return vec!["approve", "changes"],
            Expect::QuickReview => return vec!["review"],
            Expect::FailureAnalysis => return vec!["transient", "caused_by_update"],
            Expect::DiffCheck => return vec!["safe", "unsafe"],
            Expect::PackagingGaps => return vec!["gaps"],
        };
        let mut outcomes = vec!["questions", done];
        if self == Expect::FixCi {
            outcomes.push("rerun_ci");
        }
        outcomes
    }

    /// The JSON Schema passed to `claude --json-schema`. The review schema
    /// matches `agents/schemas/review.json`, which the review skill cites.
    #[must_use]
    pub fn schema(self) -> Value {
        if self == Expect::QuickReview {
            return quick_review_schema();
        }
        if self == Expect::Review {
            return json!({
                "type": "object",
                "properties": {
                    "outcome": {
                        "type": "string",
                        "enum": ["approve", "changes"],
                        "description": "approve when the change is ready to merge, changes when the author must fix something first."
                    },
                    "feedback": {
                        "type": "string",
                        "description": "With outcome changes: the problems the author must fix, each with its file and line, concrete enough to act on without the diff in view."
                    }
                },
                "required": ["outcome"],
                "additionalProperties": false
            });
        }
        if let Some(schema) = self.renovate_schema() {
            return schema;
        }
        let mut properties = json!({
            "outcome": { "type": "string", "enum": self.outcomes() },
            "questions": {
                "type": "array",
                "items": { "type": "string" },
                "description": "With outcome questions: what you need the requester to answer."
            }
        });
        match self {
            Expect::Spec => {
                properties["spec"] = json!({
                    "type": "string",
                    "description": "With outcome spec: the spec as Markdown."
                });
            }
            Expect::Plan => {
                properties["plan"] = json!({
                    "type": "array",
                    "description": "With outcome plan: the pull requests, in merge order.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "title": { "type": "string" },
                            "summary": { "type": "string" },
                            "notes": {
                                "type": "string",
                                "description": "What you found that the implementer of this pull request needs, since it starts in a new session: the files and functions to change, the tests to extend, the docs/design.md rows and sections that apply, and what an earlier pull request in this plan adds that this one builds on."
                            }
                        },
                        "required": ["title", "summary", "notes"],
                        "additionalProperties": false
                    }
                });
            }
            Expect::Commits | Expect::FixCi => {
                properties["pull_request"] = json!({
                    "type": "object",
                    "description": "With outcome commits_ready: the pull request's title and description, following the repository's CONTRIBUTING.md. Required when the factory opens the pull request from this run. When the prompt says the pull request's title and description stay as they are, leave it out, or set it to null when the schema requires the key.",
                    "properties": {
                        "title": { "type": "string" },
                        "body": { "type": "string" }
                    },
                    "required": ["title", "body"],
                    "additionalProperties": false
                });
            }
            // Review returns its own schema above, before this match.
            Expect::Clarify
            | Expect::Review
            | Expect::QuickReview
            | Expect::FailureAnalysis
            | Expect::DiffCheck
            | Expect::PackagingGaps => {}
        }
        if self == Expect::FixCi {
            properties["reason"] = json!({
                "type": "string",
                "description": "With outcome rerun_ci: why a rerun is enough and nothing needs a commit."
            });
        }
        json!({
            "type": "object",
            "properties": properties,
            "required": ["outcome"],
            "additionalProperties": false
        })
    }

    /// The schema of the two Renovate runs, or `None` for another run.
    fn renovate_schema(self) -> Option<Value> {
        match self {
            Expect::FailureAnalysis => Some(json!({
                "type": "object",
                "properties": {
                    "outcome": {
                        "type": "string",
                        "enum": self.outcomes(),
                        "description": "transient when the failure has nothing to do with the update and a rerun may pass, caused_by_update when the update makes CI fail."
                    },
                    "reason": {
                        "type": "string",
                        "description": "Why, citing the lines of the log that show it."
                    }
                },
                "required": ["outcome", "reason"],
                "additionalProperties": false
            })),
            Expect::DiffCheck => Some(json!({
                "type": "object",
                "properties": {
                    "outcome": {
                        "type": "string",
                        "enum": self.outcomes(),
                        "description": "safe when every change only moves a version, in any file, and no breaking change in the new version affects the project. unsafe for any other change, even mixed with version moves, or when a breaking change affects the project or the effect is unclear."
                    },
                    "reason": {
                        "type": "string",
                        "description": "For safe, the files and why each listed breaking change does not affect the project. For unsafe, the change that is not a version move, or the breaking change and where the project uses it, or what could not be determined."
                    },
                    "summary": {
                        "type": "string",
                        "description": "A short summary of the update: what moves from which version to which."
                    },
                    "from": {
                        "type": "string",
                        "description": "The upstream ref the update moves from, such as a version tag, when the diff shows it."
                    },
                    "to": {
                        "type": "string",
                        "description": "The upstream ref the update moves to, when the diff shows it."
                    }
                },
                "required": ["outcome", "reason", "summary"],
                "additionalProperties": false
            })),
            Expect::PackagingGaps => Some(json!({
                "type": "object",
                "properties": {
                    "outcome": {
                        "type": "string",
                        "enum": self.outcomes()
                    },
                    "gaps": {
                        "type": "array",
                        "description": "The packaging gaps found, or an empty list when there are none.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "title": {
                                    "type": "string",
                                    "description": "A short title for the gap."
                                },
                                "description": {
                                    "type": "string",
                                    "description": "What upstream introduced and why the packaging does not reflect it."
                                },
                                "evidence": {
                                    "type": "string",
                                    "description": "The upstream change that shows it, quoted from the patch or the release notes."
                                },
                                "current": {
                                    "type": "string",
                                    "description": "What the packaging manifest has now for this."
                                },
                                "suggestion": {
                                    "type": "string",
                                    "description": "The change to the packaging that closes the gap."
                                }
                            },
                            "required": ["title", "description", "evidence", "current", "suggestion"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["outcome", "gaps"],
                "additionalProperties": false
            })),
            _ => None,
        }
    }

    /// The schema Codex gets through `codex exec --output-schema`. The strict
    /// structured output format lists every property in `required` and forbids
    /// extra keys on every object, so a property a stage may leave out is made
    /// nullable here and the outcome resolution reads a `null` as absent. The
    /// outcomes and shapes are otherwise those of [`schema`](Self::schema);
    /// Claude's `--json-schema` value does not change.
    ///
    /// # Panics
    ///
    /// Panics if [`schema`](Self::schema) does not return a JSON object with a
    /// `properties` object, which it always does.
    #[must_use]
    pub fn strict_schema(self) -> Value {
        let mut schema = self.schema();
        // Every property of the quick review is required already.
        if self == Expect::QuickReview {
            return schema;
        }
        let object = schema.as_object_mut().expect("the schema is a JSON object");
        let keys: Vec<Value> = object["properties"]
            .as_object()
            .expect("the schema has properties")
            .keys()
            .map(|key| Value::from(key.clone()))
            .collect();
        object.insert("required".into(), Value::Array(keys));
        for (name, property) in object["properties"]
            .as_object_mut()
            .expect("the schema has properties")
        {
            // `outcome` is always present, so it stays non-nullable; every other
            // property a stage may leave out becomes nullable.
            if name != "outcome" {
                make_nullable(property);
            }
        }
        schema
    }
}

/// The quick review's schema, which matches `agents/schemas/quick-review.json`.
fn quick_review_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "score": {
                "type": "integer",
                "minimum": 1,
                "maximum": 5,
                "description": "How safe the pull request is to merge: 5 is safe to merge, 1 is do not merge."
            },
            "review": {
                "type": "string",
                "description": "The whole review as Markdown: what the pull request does well, and each problem with its file and line."
            }
        },
        "required": ["score", "review"],
        "additionalProperties": false
    })
}

/// Adds `"null"` to a property's `type`, so a strict schema can require the
/// property while still letting a stage leave it empty. Nested objects and
/// array items keep their own `required` and `additionalProperties`, which the
/// normal schema already sets strictly.
fn make_nullable(property: &mut Value) {
    if let Some(ty) = property.get_mut("type")
        && let Some(name) = ty.as_str()
    {
        *ty = json!([name, "null"]);
    }
}

#[derive(Deserialize)]
struct Output {
    outcome: String,
    // `Option` so a strict Codex schema's `null`, which it writes for a property
    // the stage left out, reads the same as Claude omitting the key.
    questions: Option<Vec<String>>,
    spec: Option<String>,
    plan: Option<Vec<PlanEntry>>,
    pull_request: Option<PrDescription>,
    reason: Option<String>,
    summary: Option<String>,
    from: Option<String>,
    to: Option<String>,
    gaps: Option<Vec<GapEntry>>,
}

#[derive(Deserialize)]
struct GapEntry {
    title: String,
    description: String,
    evidence: String,
    current: String,
    suggestion: String,
}

#[derive(Deserialize)]
struct PlanEntry {
    title: String,
    summary: String,
    notes: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct QuickReviewOutput {
    score: i64,
    review: String,
}

#[derive(Deserialize)]
struct ReviewOutput {
    outcome: String,
    feedback: Option<String>,
}

/// The failure event for a run of `expect`. A review run fails closed as
/// [`Event::ReviewFailed`], so the factory does not merge; every other run is
/// [`Event::AgentFailed`]. Both the podman run loop and the Codex parser build
/// their failures through this, so a review never resolves to an agent failure.
#[must_use]
pub fn failed_event(expect: Expect, reason: String) -> Event {
    if matches!(expect, Expect::Review | Expect::QuickReview) {
        Event::ReviewFailed { reason }
    } else {
        Event::AgentFailed { reason }
    }
}

/// Turns a Claude run's final message into the event for the job. A review run
/// becomes a review verdict or, when it failed or returned no valid result,
/// [`Event::ReviewFailed`], so the review fails closed. Every other run
/// becomes an agent result or [`Event::AgentFailed`].
#[must_use]
pub fn event_from_result(expect: Expect, result: &ResultMessage) -> Event {
    if matches!(expect, Expect::Review | Expect::QuickReview) {
        if let Some(reason) = run_error(result, "reviewer run") {
            return Event::ReviewFailed { reason };
        }
    } else if let Some(reason) = run_error(result, "agent run") {
        return Event::AgentFailed { reason };
    }
    event_from_output(expect, result.structured_output.clone())
}

/// Turns the structured output an agent produced into the event for the job.
/// Claude reads it from the `result` line's `structured_output`; Codex reads it
/// from the text of its last `agent_message`. `None` is the run ending with no
/// output. A review run becomes a verdict or [`Event::ReviewFailed`]; every
/// other run an agent result or [`Event::AgentFailed`].
#[must_use]
pub fn event_from_output(expect: Expect, output: Option<Value>) -> Event {
    if expect == Expect::Review {
        return match review_output(output) {
            Ok(event) => event,
            Err(reason) => Event::ReviewFailed { reason },
        };
    }
    if expect == Expect::QuickReview {
        return match quick_review_output(output) {
            Ok(event) => event,
            Err(reason) => Event::ReviewFailed { reason },
        };
    }
    match agent_output(expect, output) {
        Ok(result) => Event::Agent(result),
        Err(reason) => Event::AgentFailed { reason },
    }
}

/// The error a run that did not end in success carries, or `None` when it
/// succeeded. `subject` names the run for the message, such as `agent run`.
fn run_error(result: &ResultMessage, subject: &str) -> Option<String> {
    if !result.is_error && result.subtype == "success" {
        return None;
    }
    let detail = result
        .errors
        .first()
        .or(result.result.as_ref())
        .filter(|text| !text.is_empty())
        .map_or(String::new(), |text| format!(": {text}"));
    Some(format!(
        "the {subject} ended with {}{detail}",
        result.subtype
    ))
}

/// Turns a review run's structured output into its verdict event. Output that
/// is missing, or not a valid `approve`/`changes` result, is an error the
/// caller turns into [`Event::ReviewFailed`].
fn review_output(output: Option<Value>) -> Result<Event, String> {
    let output = output.ok_or("the reviewer run ended without structured output")?;
    let output: ReviewOutput = serde_json::from_value(output)
        .map_err(|err| format!("the reviewer's structured output does not parse: {err}"))?;
    match output.outcome.as_str() {
        "approve" => Ok(Event::ReviewApproved),
        "changes" => match output.feedback {
            Some(feedback) if !feedback.trim().is_empty() => Ok(Event::ReviewChanges { feedback }),
            _ => Err("the reviewer asked for changes but gave no feedback".into()),
        },
        other => Err(format!(
            "the reviewer ended with outcome {other:?}, expected \"approve\" or \"changes\""
        )),
    }
}

/// Turns a quick review's structured output into [`Event::QuickReviewed`].
/// The output does not hold the reviewed head, so `head` is empty and the
/// executor fills it in from the checkout. A score outside 1 to 5 or an empty
/// review is an error the caller turns into [`Event::ReviewFailed`].
fn quick_review_output(output: Option<Value>) -> Result<Event, String> {
    let output = output.ok_or("the reviewer run ended without structured output")?;
    let output: QuickReviewOutput = serde_json::from_value(output)
        .map_err(|err| format!("the reviewer's structured output does not parse: {err}"))?;
    let score = u8::try_from(output.score)
        .ok()
        .filter(|score| (1..=5).contains(score))
        .ok_or_else(|| {
            format!(
                "the reviewer gave the score {}, expected a whole number from 1 to 5",
                output.score
            )
        })?;
    if output.review.trim().is_empty() {
        return Err("the reviewer gave a score but wrote no review".into());
    }
    Ok(Event::QuickReviewed {
        score,
        review: output.review,
        head: String::new(),
    })
}

fn gaps_result(entries: Option<Vec<GapEntry>>) -> Result<AgentResult, String> {
    let entries = entries.ok_or("the agent reported no list of gaps")?;
    let mut gaps = Vec::new();
    for entry in entries {
        if entry.title.trim().is_empty() || entry.description.trim().is_empty() {
            return Err("the agent reported a gap with no title or no description".into());
        }
        gaps.push(Gap {
            title: entry.title,
            description: entry.description,
            evidence: entry.evidence,
            current: entry.current,
            suggestion: entry.suggestion,
        });
    }
    Ok(AgentResult::Gaps(gaps))
}

fn agent_output(expect: Expect, output: Option<Value>) -> Result<AgentResult, String> {
    let output = output.ok_or("the agent run ended without structured output")?;
    let output: Output = serde_json::from_value(output)
        .map_err(|err| format!("the agent's structured output does not parse: {err}"))?;

    let reason = output.reason.filter(|reason| !reason.trim().is_empty());
    match (expect, output.outcome.as_str()) {
        (Expect::FailureAnalysis, outcome @ ("transient" | "caused_by_update")) => match reason {
            Some(reason) => Ok(AgentResult::FailureVerdict {
                transient: outcome == "transient",
                reason,
            }),
            None => Err(format!(
                "the agent ended with outcome {outcome} but gave no reason"
            )),
        },
        (Expect::DiffCheck, outcome @ ("safe" | "unsafe")) => {
            let summary = output.summary.filter(|summary| !summary.trim().is_empty());
            match (reason, summary) {
                (Some(reason), Some(summary)) => {
                    Ok(AgentResult::DiffVerdict(Box::new(DiffVerdict {
                        safe: outcome == "safe",
                        reason,
                        summary,
                        from: output.from.filter(|from| !from.trim().is_empty()),
                        to: output.to.filter(|to| !to.trim().is_empty()),
                    })))
                }
                _ => Err(format!(
                    "the agent ended with outcome {outcome} but gave no reason or no summary"
                )),
            }
        }
        (Expect::PackagingGaps, "gaps") => gaps_result(output.gaps),
        (Expect::FailureAnalysis | Expect::DiffCheck | Expect::PackagingGaps, other) => {
            Err(format!(
                "the agent ended with outcome {other:?}, expected one of {:?}",
                expect.outcomes()
            ))
        }
        (_, "questions") => {
            let questions = output.questions.unwrap_or_default();
            if questions.is_empty() {
                return Err("the agent ended with questions but asked none".into());
            }
            Ok(AgentResult::Questions(
                questions
                    .into_iter()
                    .map(|text| Question { text })
                    .collect(),
            ))
        }
        (Expect::Clarify, "clarified") => Ok(AgentResult::Clarified),
        (Expect::Spec, "spec") => match output.spec {
            Some(spec) if !spec.trim().is_empty() => Ok(AgentResult::Spec(spec)),
            _ => Err("the agent ended with outcome spec but wrote no spec".into()),
        },
        (Expect::Plan, "plan") => Ok(AgentResult::Plan(
            output
                .plan
                .unwrap_or_default()
                .into_iter()
                .map(|entry| PlannedPr {
                    title: entry.title,
                    summary: entry.summary,
                    number: None,
                    description: None,
                    notes: entry.notes.filter(|notes| !notes.trim().is_empty()),
                })
                .collect(),
        )),
        (Expect::Commits | Expect::FixCi, "commits_ready") => match output.pull_request {
            Some(pr) if !pr.title.trim().is_empty() && !pr.body.trim().is_empty() => {
                Ok(AgentResult::CommitsReady(Some(pr)))
            }
            _ => Ok(AgentResult::CommitsReady(None)),
        },
        (Expect::FixCi, "rerun_ci") => match reason {
            Some(reason) => Ok(AgentResult::RerunCi { reason }),
            None => Err("the agent ended with outcome rerun_ci but gave no reason".into()),
        },
        (_, other) => Err(format!(
            "the agent ended with outcome {other:?}, expected one of {:?}",
            expect.outcomes()
        )),
    }
}
