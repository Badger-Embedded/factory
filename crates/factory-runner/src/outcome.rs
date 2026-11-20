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
    /// also end with `rerun_ci`.
    #[must_use]
    pub fn for_task(stage: &Stage, task: &Task) -> Option<Self> {
        match (Self::for_stage(stage)?, task) {
            (Expect::Commits, Task::FixCi { .. }) => Some(Expect::FixCi),
            (expect, _) => Some(expect),
        }
    }

    /// The outcomes a run may end with. Every run may end with questions.
    #[must_use]
    pub fn outcomes(self) -> Vec<&'static str> {
        let done = match self {
            Expect::Clarify => "clarified",
            Expect::Spec => "spec",
            Expect::Plan => "plan",
            Expect::Commits | Expect::FixCi => "commits_ready",
        };
        let mut outcomes = vec!["questions", done];
        if self == Expect::FixCi {
            outcomes.push("rerun_ci");
        }
        outcomes
    }

    /// The JSON Schema passed to `claude --json-schema`.
    #[must_use]
    pub fn schema(self) -> Value {
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
            Expect::Clarify => {}
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

/// Turns a run's final message into the event for the job. A run that
/// failed, or ended without a valid outcome for `expect`, becomes
/// [`Event::AgentFailed`].
#[must_use]
pub fn event_from_result(expect: Expect, result: &ResultMessage) -> Event {
    match agent_result(expect, result) {
        Ok(result) => Event::Agent(result),
        Err(reason) => Event::AgentFailed { reason },
    }
}

fn agent_result(expect: Expect, result: &ResultMessage) -> Result<AgentResult, String> {
    if result.is_error || result.subtype != "success" {
        let detail = result
            .errors
            .first()
            .or(result.result.as_ref())
            .filter(|text| !text.is_empty())
            .map_or(String::new(), |text| format!(": {text}"));
        return Err(format!(
            "the agent run ended with {}{detail}",
            result.subtype
        ));
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
