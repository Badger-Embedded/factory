//! Stable names for stages, turns and events. The server stores them in the
//! job history, so a rename breaks the history of every stored job.

use crate::job::{AgentResult, Ask, Event, Stage, Task, Turn};

impl Stage {
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Stage::Requested => "requested",
            Stage::Clarifying => "clarifying",
            Stage::Spec => "spec",
            Stage::Plan => "plan",
            Stage::Implementing { .. } => "implementing",
            Stage::Verifying { .. } => "verifying",
            Stage::PrOpen { .. } => "pr-open",
            Stage::CiAndReview { .. } => "ci-and-review",
            Stage::Merged => "merged",
            Stage::Cancelled => "cancelled",
            Stage::Failed { .. } => "failed",
        }
    }

    /// The index in the plan of the pull request the stage works on.
    #[must_use]
    pub fn pr(&self) -> Option<usize> {
        match self {
            Stage::Implementing { pr }
            | Stage::Verifying { pr }
            | Stage::PrOpen { pr }
            | Stage::CiAndReview { pr } => Some(*pr),
            _ => None,
        }
    }
}

impl Task {
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Task::Clarify { .. } => "clarify",
            Task::WriteSpec => "write-spec",
            Task::WritePlan => "write-plan",
            Task::Implement { .. } => "implement",
            Task::Answer { .. } => "answer",
            Task::Revise { .. } => "revise",
            Task::FixChecks { .. } => "fix-checks",
            Task::FixCi { .. } => "fix-ci",
            Task::AddressReview { .. } => "address-review",
            Task::Rebase { .. } => "rebase",
            Task::Review => "review",
        }
    }
}

impl Ask {
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Ask::Answers { .. } => "answers",
            Ask::SpecApproval => "spec-approval",
            Ask::PlanApproval => "plan-approval",
            Ask::PublishApproval { .. } => "publish-approval",
            Ask::ReviewStopped { .. } => "review-stopped",
        }
    }
}

impl Turn {
    /// `agent:<task>`, `user:<ask>`, `factory`, `rerun-ci` or `done`.
    #[must_use]
    pub fn name(&self) -> String {
        match self {
            Turn::Agent(task) => format!("agent:{}", task.name()),
            Turn::User(ask) => format!("user:{}", ask.name()),
            Turn::Factory => "factory".into(),
            Turn::RerunCi { .. } => "rerun-ci".into(),
            Turn::Done => "done".into(),
        }
    }
}

impl Event {
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Event::Start => "start",
            Event::Edit { .. } => "edit",
            Event::Agent(result) => match result {
                AgentResult::Questions(_) => "agent:questions",
                AgentResult::Clarified => "agent:clarified",
                AgentResult::Spec(_) => "agent:spec",
                AgentResult::Plan(_) => "agent:plan",
                AgentResult::CommitsReady(_) => "agent:commits-ready",
                AgentResult::RerunCi { .. } => "agent:rerun-ci",
            },
            Event::AgentFailed { .. } => "agent-failed",
            Event::Answer(_) => "answer",
            Event::Approve => "approve",
            Event::RequestChanges { .. } => "request-changes",
            Event::ChecksPassed => "checks-passed",
            Event::Summarized(_) => "summarized",
            Event::ChecksFailed { .. } => "checks-failed",
            Event::Published { .. } => "published",
            Event::CiFailed { .. } => "ci-failed",
            Event::CiRerun => "ci-rerun",
            Event::Conflicted { .. } => "conflicted",
            Event::ReviewComments { .. } => "review-comments",
            Event::StartReview => "start-review",
            Event::ReviewApproved => "review-approved",
            Event::ReviewChanges { .. } => "review-changes",
            Event::ReviewFailed { .. } => "review-failed",
            Event::Merged => "merged",
            Event::ClosedUnmerged => "closed-unmerged",
            Event::Cancel => "cancel",
            Event::Retry => "retry",
            Event::Dismiss => "dismiss",
        }
    }
}
