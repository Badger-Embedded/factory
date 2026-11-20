use factory_core::{AgentResult, Event, PlannedPr, PrDescription, Question, Stage, Task};
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
        match (Self::for_stage(stage)?, task) {
            (Expect::Commits, Task::FixCi { .. }) => Some(Expect::FixCi),
            (_, Task::Review) => Some(Expect::Review),
            (expect, _) => Some(expect),
        }
    }

    /// The outcomes a run may end with. Every stage run may end with
    /// questions; a review run ends with `approve` or `changes`.
    #[must_use]
    pub fn outcomes(self) -> Vec<&'static str> {
        let done = match self {
            Expect::Clarify => "clarified",
            Expect::Spec => "spec",
            Expect::Plan => "plan",
            Expect::Commits | Expect::FixCi => "commits_ready",
            Expect::Review => return vec!["approve", "changes"],
        };
        let mut outcomes = vec!["questions", done];
        if self == Expect::FixCi {
            outcomes.push("rerun_ci");
        }
        outcomes
    }

    /// The JSON Schema passed to `claude --json-schema`. The review schema
    /// matches `plugin/schemas/review.json`, which the review skill cites.
    #[must_use]
    pub fn schema(self) -> Value {
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
                            "summary": { "type": "string" }
                        },
                        "required": ["title", "summary"],
                        "additionalProperties": false
                    }
                });
            }
            Expect::Commits | Expect::FixCi => {
                properties["pull_request"] = json!({
                    "type": "object",
                    "description": "With outcome commits_ready: the pull request's title and description, following the repository's CONTRIBUTING.md.",
                    "properties": {
                        "title": { "type": "string" },
                        "body": { "type": "string" }
                    },
                    "required": ["title", "body"],
                    "additionalProperties": false
                });
            }
            // Review returns its own schema above, before this match.
            Expect::Clarify | Expect::Review => {}
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
}

#[derive(Deserialize)]
struct Output {
    outcome: String,
    #[serde(default)]
    questions: Vec<String>,
    spec: Option<String>,
    plan: Option<Vec<PlanEntry>>,
    pull_request: Option<PrDescription>,
    reason: Option<String>,
}

#[derive(Deserialize)]
struct PlanEntry {
    title: String,
    summary: String,
}

#[derive(Deserialize)]
struct ReviewOutput {
    outcome: String,
    feedback: Option<String>,
}

/// Turns a run's final message into the event for the job. A review run
/// becomes a review verdict or, when it failed or returned no valid result,
/// [`Event::ReviewFailed`], so the review fails closed. Every other run
/// becomes an agent result or [`Event::AgentFailed`].
#[must_use]
pub fn event_from_result(expect: Expect, result: &ResultMessage) -> Event {
    if expect == Expect::Review {
        return match review_result(result) {
            Ok(event) => event,
            Err(reason) => Event::ReviewFailed { reason },
        };
    }
    match agent_result(expect, result) {
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

/// Turns a review run's final message into its verdict event. A run that
/// failed, or ended without a valid `approve`/`changes` result, is an error
/// the caller turns into [`Event::ReviewFailed`].
fn review_result(result: &ResultMessage) -> Result<Event, String> {
    if let Some(reason) = run_error(result, "reviewer run") {
        return Err(reason);
    }
    let output = result
        .structured_output
        .clone()
        .ok_or("the reviewer run ended without structured output")?;
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

fn agent_result(expect: Expect, result: &ResultMessage) -> Result<AgentResult, String> {
    if let Some(reason) = run_error(result, "agent run") {
        return Err(reason);
    }
    let output = result
        .structured_output
        .clone()
        .ok_or("the agent run ended without structured output")?;
    let output: Output = serde_json::from_value(output)
        .map_err(|err| format!("the agent's structured output does not parse: {err}"))?;

    match (expect, output.outcome.as_str()) {
        (_, "questions") => {
            if output.questions.is_empty() {
                return Err("the agent ended with questions but asked none".into());
            }
            Ok(AgentResult::Questions(
                output
                    .questions
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
                })
                .collect(),
        )),
        (Expect::Commits | Expect::FixCi, "commits_ready") => match output.pull_request {
            Some(pr) if !pr.title.trim().is_empty() && !pr.body.trim().is_empty() => {
                Ok(AgentResult::CommitsReady(pr))
            }
            _ => Err(
                "the agent ended with outcome commits_ready but wrote no pull request title and description"
                    .into(),
            ),
        },
        (Expect::FixCi, "rerun_ci") => match output.reason {
            Some(reason) if !reason.trim().is_empty() => Ok(AgentResult::RerunCi { reason }),
            _ => Err("the agent ended with outcome rerun_ci but gave no reason".into()),
        },
        (_, other) => Err(format!(
            "the agent ended with outcome {other:?}, expected one of {:?}",
            expect.outcomes()
        )),
    }
}
