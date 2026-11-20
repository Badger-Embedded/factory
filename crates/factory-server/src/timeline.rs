//! The job page timeline. It lays the job's history from `job_events` over
//! the steps every job runs: four planning steps, four steps for each planned
//! pull request, then merged.

use factory_core::{Job, Stage, Turn};
use factory_runner::Expect;
use maud::{Markup, html};

use crate::cost::format_cost;
use crate::pages::task_label;
use crate::projects::StageModels;
use crate::store::JobEvent;

const PLANNING: [&str; 4] = ["requested", "clarifying", "spec", "plan"];
const PER_PR: [&str; 4] = ["implementing", "verifying", "pr-open", "ci-and-review"];
/// Older notes in a step collapse into one "earlier" line.
const NOTES_SHOWN: usize = 8;

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Done,
    Current,
    Failed,
    Cancelled,
    Pending,
}

impl State {
    fn class(self) -> &'static str {
        match self {
            State::Done => "done",
            State::Current => "current",
            State::Failed => "failed",
            State::Cancelled => "cancelled",
            State::Pending => "pending",
        }
    }
}

struct Note {
    at: String,
    text: String,
}

#[derive(Default)]
struct Step {
    /// When the job first entered the step.
    start: Option<String>,
    /// When the job entered the step the last time.
    visit: Option<String>,
    /// Time spent in the step over every finished visit.
    seconds: i64,
    usd: f64,
    notes: Vec<Note>,
    approved: Option<String>,
}

struct Timeline {
    steps: Vec<Step>,
    /// The step the job is in, or for a failed or cancelled job the step it
    /// was in when that happened. `None` when the history does not say.
    current: Option<usize>,
}

fn stage_order(stage: &str, pr: Option<usize>, prs: usize) -> Option<usize> {
    if let Some(i) = PLANNING.iter().position(|name| *name == stage) {
        return Some(i);
    }
    if let (Some(i), Some(pr)) = (PER_PR.iter().position(|name| *name == stage), pr) {
        return (pr < prs).then_some(PLANNING.len() + PER_PR.len() * pr + i);
    }
    (stage == "merged").then_some(merged_order(prs))
}

fn merged_order(prs: usize) -> usize {
    PLANNING.len() + PER_PR.len() * prs
}

fn step_name(order: usize, prs: usize) -> &'static str {
    match order.checked_sub(PLANNING.len()) {
        None => PLANNING[order],
        Some(_) if order == merged_order(prs) => "merged",
        Some(i) => PER_PR[i % PER_PR.len()],
    }
}

fn step_pr(order: usize, prs: usize) -> Option<usize> {
    let i = order.checked_sub(PLANNING.len())?;
    (order < merged_order(prs)).then_some(i / PER_PR.len())
}

fn build(job: &Job, events: &[JobEvent]) -> Timeline {
    let prs = job.plan().len();
    let mut steps: Vec<Step> = (0..=merged_order(prs)).map(|_| Step::default()).collect();
    let mut current = None;
    let mut active: Option<usize> = None;
    for event in events {
        if let Some(step) = current.and_then(|i: usize| steps.get_mut(i)) {
            note(step, event);
        }
        let at = event.at.as_str();
        let entered = stage_order(&event.stage, event.pr, prs);
        if entered != active {
            if let Some(step) = active.and_then(|i| steps.get_mut(i)) {
                step.seconds += elapsed(step.visit.as_deref(), at);
            }
            active = entered;
            if let Some(step) = entered.and_then(|i| steps.get_mut(i)) {
                step.start.get_or_insert_with(|| at.to_owned());
                step.visit = Some(at.to_owned());
            }
        }
        if entered.is_some() {
            current = entered;
        }
        if let (Some(usd), Some(step)) = (event.usd, current.and_then(|i| steps.get_mut(i))) {
            step.usd += usd;
        }
    }
    if !matches!(job.stage(), Stage::Failed { .. } | Stage::Cancelled) {
        current = stage_order(job.stage().name(), job.stage().pr(), prs);
    }
    Timeline { steps, current }
}

/// Adds to `step` what `event` says happened in it.
fn note(step: &mut Step, event: &JobEvent) {
    let text = match event.event.as_str() {
        "approve" => {
            step.approved = Some(event.at.clone());
            None
        }
        "agent:questions" => Some("The agent asked questions"),
        "answer" => Some("You answered"),
        "request-changes" => Some("You asked for changes"),
        "checks-failed" => Some("Checks failed"),
        "ci-failed" => Some("CI failed"),
        "agent:rerun-ci" => Some("The agent asked to rerun CI"),
        "ci-rerun" => Some("CI ran again"),
        "conflicted" => Some("Conflict with the base branch"),
        "review-comments" => Some("Review comments on GitHub"),
        "review-approved" => Some("LLM review approved"),
        "review-changes" => Some("LLM review asked for changes"),
        "review-failed" => Some("LLM review run failed"),
        "retry" => Some("You retried the job"),
        "cancel" => Some("You cancelled the job"),
        _ => None,
    };
    let failure = (event.stage == "failed").then(|| match &event.detail {
        Some(reason) => format!("Failed: {reason}"),
        None => "Failed".to_owned(),
    });
    for text in text.map(str::to_owned).into_iter().chain(failure) {
        step.notes.push(Note {
            at: event.at.clone(),
            text,
        });
    }
}

fn state(job: &Job, timeline: &Timeline, order: usize) -> State {
    let step = &timeline.steps[order];
    match (timeline.current, job.stage()) {
        (_, Stage::Merged) => State::Done,
        (Some(current), stage) if current == order => match stage {
            Stage::Failed { .. } => State::Failed,
            Stage::Cancelled => State::Cancelled,
            _ => State::Current,
        },
        (Some(current), _) if order < current => State::Done,
        _ if step.start.is_some() => State::Done,
        _ => State::Pending,
    }
}

/// Seconds from `from` to `to`, both as SQLite's `CURRENT_TIMESTAMP` writes
/// them. A time that does not parse counts as no time.
fn elapsed(from: Option<&str>, to: &str) -> i64 {
    match (from.and_then(seconds), seconds(to)) {
        (Some(from), Some(to)) if to > from => to - from,
        _ => 0,
    }
}

fn seconds(at: &str) -> Option<i64> {
    let field = |range: std::ops::Range<usize>| at.get(range)?.parse::<i64>().ok();
    let (year, month, day) = (field(0..4)?, field(5..7)?, field(8..10)?);
    let (hour, minute, second) = (field(11..13)?, field(14..16)?, field(17..19)?);
    Some(days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second)
}

/// Days since 1970-01-01 of a proleptic Gregorian date.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn duration(seconds: i64) -> String {
    let minutes = seconds / 60;
    match minutes {
        0 => "<1m".to_owned(),
        1..60 => format!("{minutes}m"),
        60..1440 => format!("{}h {}m", minutes / 60, minutes % 60),
        _ => format!("{}d {}h", minutes / 1440, minutes % 1440 / 60),
    }
}

fn clock(at: &str) -> &str {
    at.get(11..16).unwrap_or(at)
}

fn gate(job: &Job, order: usize) -> bool {
    let gates = job.gates();
    let prs = job.plan().len();
    match (step_name(order, prs), step_pr(order, prs)) {
        ("spec", _) => gates.spec,
        ("plan", _) => gates.plan,
        ("verifying", Some(pr)) => gates.publish && job.plan()[pr].number.is_none(),
        _ => false,
    }
}

fn model<'a>(job: &Job, models: &'a StageModels) -> Option<&'a str> {
    let Turn::Agent(task) = job.turn() else {
        return None;
    };
    let model = match Expect::for_task(job.stage(), task)? {
        Expect::Clarify => &models.clarify,
        Expect::Spec => &models.spec,
        Expect::Plan => &models.plan,
        Expect::Commits | Expect::FixCi => &models.implement,
        Expect::Review => &models.review,
    };
    Some(model.as_deref().unwrap_or("default"))
}

pub(crate) fn timeline(job: &Job, events: &[JobEvent], models: &StageModels) -> Markup {
    let timeline = build(job, events);
    let prs = job.plan().len();
    let merged = merged_order(prs);
    html! {
        div.card.timeline {
            h2 { "Timeline" }
            ol.tl {
                @for order in 0..PLANNING.len() { (step(job, &timeline, models, order)) }
            }
            @if prs == 0 {
                ol.tl {
                    li.pending { span.node {} span.step-name { "the planned pull requests" } }
                }
            }
            @for pr in 0..prs { (pr_group(job, &timeline, models, pr)) }
            ol.tl { (step(job, &timeline, models, merged)) }
            @if timeline.current.is_none() {
                @if let Stage::Failed { reason, .. } = job.stage() {
                    p.tl-end.failed { "Failed: " (reason) }
                }
            }
        }
    }
}

fn pr_group(job: &Job, timeline: &Timeline, models: &StageModels, pr: usize) -> Markup {
    let planned = &job.plan()[pr];
    let orders: Vec<usize> = (0..PER_PR.len())
        .map(|i| PLANNING.len() + PER_PR.len() * pr + i)
        .collect();
    let states: Vec<State> = orders
        .iter()
        .map(|&order| state(job, timeline, order))
        .collect();
    let open = states
        .iter()
        .any(|state| matches!(state, State::Current | State::Failed | State::Cancelled));
    let finished = states.iter().all(|state| *state == State::Done)
        && timeline
            .current
            .is_some_and(|current| current > orders[orders.len() - 1]);
    let seconds: i64 = orders
        .iter()
        .map(|&order| timeline.steps[order].seconds)
        .sum();
    let usd: f64 = orders.iter().map(|&order| timeline.steps[order].usd).sum();
    html! {
        details.pr-group open[open] {
            summary {
                span.pr-name {
                    "PR " (pr + 1) " of " (job.plan().len())
                    @if let Some(number) = planned.number { " · #" (number) }
                }
                @if finished {
                    span.when { (duration(seconds)) @if usd > 0.0 { " · " (format_cost(usd)) } }
                }
                span.pr-title { (planned.title) }
            }
            ol.tl {
                @for order in orders { (step(job, timeline, models, order)) }
            }
        }
    }
}

fn step(job: &Job, timeline: &Timeline, models: &StageModels, order: usize) -> Markup {
    let step = &timeline.steps[order];
    let state = state(job, timeline, order);
    let hidden = step.notes.len().saturating_sub(NOTES_SHOWN);
    html! {
        li class=(state.class()) aria-current=[(state == State::Current).then_some("step")] {
            span.node {}
            span.step-name { (step_name(order, job.plan().len())) }
            span.when title=[step.start.as_deref().map(|at| format!("{at} UTC"))] {
                (when(step, state))
            }
            div.sub {
                @if let Some(at) = &step.approved {
                    span.step-tag.ok { "✓ approved " (clock(at)) }
                } @else if gate(job, order) && state != State::Done {
                    span.step-tag.gate title="Waits for your approval" { "✋ approval" }
                }
                @if state == State::Current { (actor(job, models)) }
                @if state == State::Failed {
                    @if let Stage::Failed { reason, .. } = job.stage() {
                        span.failure { (reason) }
                    }
                }
            }
            @if !step.notes.is_empty() {
                ul.rounds {
                    @if hidden > 0 { li.earlier { (hidden) " earlier" } }
                    @for note in &step.notes[hidden..] {
                        li { span { (note.text) } span.when { (clock(&note.at)) } }
                    }
                }
            }
        }
    }
}

fn when(step: &Step, state: State) -> String {
    let Some(start) = &step.start else {
        return String::new();
    };
    let cost = if step.usd > 0.0 {
        format!(" · {}", format_cost(step.usd))
    } else {
        String::new()
    };
    match state {
        State::Current => {
            format!(
                "since {}{cost}",
                clock(step.visit.as_deref().unwrap_or(start))
            )
        }
        _ if step.seconds > 0 => format!("{} · {}{cost}", clock(start), duration(step.seconds)),
        _ => format!("{}{cost}", clock(start)),
    }
}

fn actor(job: &Job, models: &StageModels) -> Markup {
    match job.turn() {
        Turn::Agent(task) => html! {
            span.step-tag.agent title="The model of this run" {
                (model(job, models).unwrap_or("default"))
            }
            span { (task_label(task)) }
        },
        Turn::User(_) => html! { span.step-tag.you { "waiting for you" } },
        Turn::Factory | Turn::RerunCi { .. } => html! { span.step-tag.factory { "factory" } },
        Turn::Done => html! {},
    }
}
