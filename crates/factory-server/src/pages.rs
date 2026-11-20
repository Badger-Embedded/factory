use std::collections::HashMap;

use axum::http::StatusCode;
use factory_core::{Ask, BranchSummary, Command, Gates, Job, Stage, Task, Turn};
use maud::{DOCTYPE, Markup, PreEscaped, html};

use crate::cost::{format_cost, merge_fraction};
use crate::projects::{Project, Projects};
use crate::settings::Settings;
use crate::store::{JobRow, LogEntry, ProjectTotals};

pub const STYLESHEET: &str = include_str!("../static/style.css");
const PROJECT_PICKER: &str = include_str!("../static/project-picker.js");
const PASTE_IMAGES: &str = include_str!("../static/paste-images.js");
const LOG_SCROLL: &str = include_str!("../static/log-scroll.js");

const STEPS: [&str; 9] = [
    "requested",
    "clarifying",
    "spec",
    "plan",
    "implementing",
    "verifying",
    "pr-open",
    "ci-and-review",
    "merged",
];

fn layout(user: &str, title: &str, body: &Markup) -> Markup {
    page(user, title, false, &html! {}, body)
}

fn page(user: &str, title: &str, refresh: bool, nav: &Markup, body: &Markup) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (title) " · factory" }
                @if refresh { meta http-equiv="refresh" content="5"; }
                link rel="stylesheet" href="/style.css";
            }
            body {
                div.topbar {
                    div.topbar-inner {
                        a.brand href="/" { span.brand-mark aria-hidden="true" {} "factory" }
                        div.topbar-end {
                            (nav)
                            a.nav href="/settings" { "Settings" }
                            @if !user.is_empty() { span.user { (user) } }
                        }
                    }
                }
                main { (body) }
            }
        }
    }
}

pub fn error(status: StatusCode, message: &str) -> Markup {
    layout(
        "",
        status.as_str(),
        &html! {
            div.card.failed {
                h1 { (status) }
                p { (message) }
                a href="/" { "Back to jobs" }
            }
        },
    )
}

pub fn index(
    user: &str,
    projects: &Projects,
    selected: Option<&str>,
    settings: &Settings,
    jobs: &[JobRow],
    costs: &HashMap<i64, f64>,
    totals: ProjectTotals,
) -> Markup {
    let all: Vec<&JobRow> = jobs
        .iter()
        .filter(|row| Some(row.project.as_str()) == selected)
        .collect();
    let nav = selected.map_or_else(|| html! {}, |selected| project_selector(projects, selected));
    page(
        user,
        "Jobs",
        false,
        &nav,
        &html! {
            @if let Some(selected) = selected {
                form.card method="post" action="/jobs" enctype="multipart/form-data" {
                    h2.form-title { "New request for " (selected) }
                    input type="hidden" name="project" value=(selected);
                    div.field {
                        label for="request" { "What should change?" }
                        textarea #request name="request" required
                            placeholder="Add a reboot button to the device page" {}
                    }
                    div.field {
                        label for="images" { "Images (optional)" }
                        div.file-picker {
                            input #images type="file" name="images" multiple
                                accept="image/png,image/jpeg,image/gif,image/webp"
                                class="visually-hidden";
                            label.file-button for="images" { "Choose images" }
                            span.hint { "PNG, JPEG, GIF or WebP. You can also paste them into the box above." }
                        }
                        ul #pasted-images.pasted-images {}
                    }
                    div.form-actions { button.primary { "Request" } }
                    script { (PreEscaped(PASTE_IMAGES)) }
                }
            } @else {
                div.card { p.empty { "No projects are configured." } }
            }
            @if selected.is_some() {
                div.overview {
                    (stats(totals))
                    (lane_counts(&all, settings))
                }
            }
            (board(&all, costs, settings))
        },
    )
}

/// A bar list of the live jobs per lane. Done jobs are not live. The longest
/// bar is the lane with the most jobs, and the others scale against it.
#[allow(clippy::cast_precision_loss, reason = "job counts stay far below 2^52")]
fn lane_counts(rows: &[&JobRow], settings: &Settings) -> Markup {
    let counts: Vec<(Lane, usize)> = Lane::ALL
        .into_iter()
        .filter(|lane| *lane != Lane::Done)
        .map(|lane| {
            let count = rows
                .iter()
                .filter(|row| status(&row.job, settings).lane == lane)
                .count();
            (lane, count)
        })
        .collect();
    let longest = counts.iter().map(|(_, count)| *count).max().unwrap_or(0);
    html! {
        section.card.lane-counts {
            h2 { "Live jobs by lane" }
            @if longest == 0 {
                p.empty { "No live jobs." }
            } @else {
                ul.bars {
                    @for (lane, count) in counts {
                        @let width = format!("{:.1}%", count as f64 / longest as f64 * 100.0);
                        li class={ "bar bar-" (lane.name()) } {
                            span.bar-label { (lane.title()) }
                            span.bar-count { (count) }
                            span.bar-track { span.bar-fill style={ "width: " (width) } {} }
                        }
                    }
                }
            }
        }
    }
}

const NO_VALUE: &str = "–";

/// The KPI strip: large-number tiles for the project's totals and a gauge
/// for the merge rate. A figure that is zero or would divide by zero shows a
/// dash.
#[allow(clippy::cast_precision_loss, reason = "job counts stay far below 2^52")]
fn stats(totals: ProjectTotals) -> Markup {
    let created = totals.jobs_created;
    let merged = totals.jobs_merged;
    let average = (merged > 0).then(|| format_cost(totals.usd / merged as f64));
    let tiles = [
        ("Total API cost", format_cost(totals.usd)),
        ("Jobs created", count_or_dash(created)),
        ("Jobs merged", count_or_dash(merged)),
        (
            "Cost per merged job",
            average.unwrap_or_else(|| NO_VALUE.to_owned()),
        ),
    ];
    html! {
        div.stats {
            @for (label, value) in tiles {
                div.card.stat {
                    div.stat-value { (value) }
                    div.stat-label { (label) }
                }
            }
            (merge_gauge(created, merged))
        }
    }
}

/// A half-circle gauge filled to the merge rate. The arc has `pathLength` 100,
/// so the dash length is the percentage.
fn merge_gauge(created: i64, merged: i64) -> Markup {
    let fraction = merge_fraction(created, merged);
    let label = if created > 0 {
        format!("{:.0}%", fraction * 100.0)
    } else {
        NO_VALUE.to_owned()
    };
    let fill = format!("{:.1} 100", fraction * 100.0);
    html! {
        div.card.stat.gauge {
            svg.gauge-arc viewBox="0 0 100 56" role="img" aria-label=(format!("Merge rate {label}")) {
                path.gauge-track d="M 10 50 A 40 40 0 0 1 90 50" pathLength="100" {}
                path.gauge-fill d="M 10 50 A 40 40 0 0 1 90 50" pathLength="100"
                    stroke-dasharray=(fill) {}
            }
            div.stat-value { (label) }
            div.stat-label { "Merge rate" }
        }
    }
}

fn count_or_dash(count: i64) -> String {
    if count > 0 {
        count.to_string()
    } else {
        NO_VALUE.to_owned()
    }
}

fn project_selector(projects: &Projects, selected: &str) -> Markup {
    html! {
        form.project-picker method="get" action="/" {
            label.visually-hidden for="project" { "Project" }
            select #project name="project" {
                @for name in projects.names() {
                    option selected[name == selected] { (name) }
                }
            }
            noscript { button { "Go" } }
            script { (PreEscaped(PROJECT_PICKER)) }
        }
    }
}

pub fn settings(user: &str, settings: &Settings, notice: Option<&str>) -> Markup {
    layout(
        user,
        "Settings",
        &html! {
            h1 { "Settings" }
            p.meta { "The job loop reads these on every pass. A run that already started keeps the values it started with." }
            @if let Some(notice) = notice {
                div.card.saved { (notice) }
            }
            form.card method="post" action="/settings" {
                h2 { "Workers" }
                div.field {
                    label for="max_workers" { "Workers at a time" }
                    input #max_workers type="number" name="max_workers" min="0" required
                        value=(settings.max_workers);
                    p.hint { "Agent runs and check runs each take one. 0 pauses new runs." }
                }
                div.field {
                    label for="worker_image" { "Image" }
                    input #worker_image.wide type="text" name="worker_image" required
                        value=(settings.worker_image);
                }
                div.row {
                    div.field {
                        label for="worker_memory" { "Memory" }
                        input #worker_memory type="text" name="worker_memory" required
                            value=(settings.worker_memory);
                        p.hint { "As podman's --memory takes it, such as 3g." }
                    }
                    div.field {
                        label for="worker_cpus" { "CPUs" }
                        input #worker_cpus type="text" name="worker_cpus" required
                            value=(settings.worker_cpus);
                    }
                }
                h2 { "Approval gates" }
                div.field {
                    label.check {
                        input type="checkbox" name="gate_spec" value="on" checked[settings.gates.spec];
                        " Approve the spec"
                    }
                    label.check {
                        input type="checkbox" name="gate_plan" value="on" checked[settings.gates.plan];
                        " Approve the plan"
                    }
                    label.check {
                        input type="checkbox" name="gate_publish" value="on" checked[settings.gates.publish];
                        " Approve before pushing"
                    }
                    p.hint { "Applies to jobs created after you save. A job keeps the gates it started with." }
                }
                h2 { "GitHub" }
                div.field {
                    label.check {
                        input type="checkbox" name="auto_merge" value="on" checked[settings.auto_merge];
                        " Merge pull requests when their checks pass"
                    }
                    p.hint { "The factory squash-merges once every check passed and no review comment waits for the agent. Off, you merge on GitHub." }
                }
                div.field {
                    label for="poll_seconds" { "Seconds between looks at a pull request" }
                    input #poll_seconds type="number" name="poll_seconds" min="10" required
                        value=(settings.poll_seconds);
                    p.hint { "Also the wait before retrying a failed push or GitHub call." }
                }
                button.primary { "Save" }
            }
        },
    )
}

fn gates_line(gates: Gates) -> String {
    let on: Vec<&str> = [
        (gates.spec, "spec"),
        (gates.plan, "plan"),
        (gates.publish, "publish"),
    ]
    .into_iter()
    .filter_map(|(on, name)| on.then_some(name))
    .collect();
    if on.is_empty() {
        "none".into()
    } else {
        on.join(", ")
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Lane {
    You,
    Agent,
    Factory,
    Done,
}

impl Lane {
    const ALL: [Lane; 4] = [Lane::You, Lane::Agent, Lane::Factory, Lane::Done];

    fn title(self) -> &'static str {
        match self {
            Lane::You => "You",
            Lane::Agent => "Agent",
            Lane::Factory => "Factory",
            Lane::Done => "Done",
        }
    }

    fn name(self) -> &'static str {
        match self {
            Lane::You => "you",
            Lane::Agent => "agent",
            Lane::Factory => "factory",
            Lane::Done => "done",
        }
    }

    fn class(self) -> &'static str {
        match self {
            Lane::You => "lane lane-you",
            Lane::Agent => "lane lane-agent",
            Lane::Factory => "lane lane-factory",
            Lane::Done => "lane lane-done",
        }
    }

    fn empty(self) -> &'static str {
        match self {
            Lane::You => "Nothing needs you right now.",
            Lane::Agent => "No agent is working.",
            Lane::Factory => "The factory has nothing to do.",
            Lane::Done => "No finished jobs.",
        }
    }
}

fn board(rows: &[&JobRow], costs: &HashMap<i64, f64>, settings: &Settings) -> Markup {
    html! {
        div.board {
            @for lane in Lane::ALL {
                @let cards: Vec<&JobRow> = rows
                    .iter()
                    .copied()
                    .filter(|row| status(&row.job, settings).lane == lane)
                    .collect();
                @if lane == Lane::Done {
                    details class=(lane.class()) {
                        summary { (lane.title()) " " span.count { (cards.len()) } }
                        (lane_cards(&cards, costs, settings, lane))
                    }
                } @else {
                    section class=(lane.class()) {
                        h2 { (lane.title()) " " span.count { (cards.len()) } }
                        (lane_cards(&cards, costs, settings, lane))
                    }
                }
            }
        }
    }
}

fn lane_cards(
    cards: &[&JobRow],
    costs: &HashMap<i64, f64>,
    settings: &Settings,
    lane: Lane,
) -> Markup {
    html! {
        @if cards.is_empty() {
            p.empty { (lane.empty()) }
        } @else {
            ul.jobs {
                @for row in cards {
                    li.card.job {
                        div.job-head {
                            span.id { "#" (row.id) }
                            (turn_badge(&row.job, settings))
                        }
                        a.title href={ "/jobs/" (row.id) } title=(first_line(row.job.request())) {
                            (first_line(row.job.request()))
                        }
                        div.job-sub {
                            span { (stage_label(&row.job)) }
                            span.requester title=(row.requested_by) { (row.requested_by) }
                            span.cost { (cost_label(costs.get(&row.id).copied().unwrap_or(0.0))) }
                        }
                        @if lane == Lane::You {
                            @if let Some(gate) = gate_name(&row.job) {
                                form.approve method="post" action={ "/jobs/" (row.id) "/approve" } {
                                    input type="hidden" name="project" value=(row.project);
                                    button.primary { "Approve " (gate) }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn gate_name(job: &Job) -> Option<&'static str> {
    match job.turn() {
        Turn::User(Ask::SpecApproval) => Some("spec"),
        Turn::User(Ask::PlanApproval) => Some("plan"),
        _ => None,
    }
}

fn cost_label(usd: f64) -> String {
    if usd > 0.0 {
        format_cost(usd)
    } else {
        "–".to_owned()
    }
}

pub fn job(
    user: &str,
    row: &JobRow,
    project: Option<&Project>,
    settings: &Settings,
    logs: &[LogEntry],
    images: &[String],
    cost_usd: f64,
) -> Markup {
    let job = &row.job;
    let repo_url = project.map(|project| format!("https://github.com/{}", project.repo));
    let id = row.id;
    page(
        user,
        &format!("Job #{id}"),
        matches!(job.turn(), Turn::Agent(_)),
        &html! {},
        &html! {
            h1 { "#" (id) " " (first_line(job.request())) }
            p.meta {
                @if let Some(url) = &repo_url {
                    a href=(url) { (row.project) }
                } @else {
                    (row.project)
                }
                " · requested by " (row.requested_by) " · " (row.created_at) " UTC"
                " · " (cost_label(cost_usd))
            }
            (steps(job))
            (status_card(id, job, settings))
            p.meta { "Approval gates: " (gates_line(job.gates())) }
            div.card {
                h2 { "Request" }
                pre.doc { (job.request()) }
                @if !images.is_empty() {
                    ul.request-images {
                        @for name in images {
                            li {
                                a href={ "/jobs/" (id) "/images/" (name) } {
                                    img src={ "/jobs/" (id) "/images/" (name) } alt=(name);
                                }
                            }
                        }
                    }
                }
            }
            @if let Some(spec) = job.spec() {
                div.card {
                    h2 { "Spec" }
                    pre.doc { (spec) }
                }
            }
            @if !job.plan().is_empty() {
                div.card {
                    h2 { "Plan" }
                    ol.plan {
                        @for pr in job.plan() {
                            li {
                                strong { (pr.title) }
                                @if let Some(number) = pr.number {
                                    " · "
                                    @if let Some(url) = &repo_url {
                                        a href={ (url) "/pull/" (number) } { "#" (number) }
                                    } @else {
                                        "#" (number)
                                    }
                                }
                                div.summary { (pr.summary) }
                            }
                        }
                    }
                }
            }
            @if !logs.is_empty() {
                div.card {
                    h2 { "Log" }
                    ol.log {
                        @for entry in logs {
                            li class={ "log-" (entry.kind) } {
                                span.log-at { (entry.at.get(11..).unwrap_or(&entry.at)) }
                                span.log-text { (entry.text) }
                            }
                        }
                    }
                    script { (PreEscaped(LOG_SCROLL)) }
                }
            }
            @if *job.turn() != Turn::Done {
                form.footer-actions method="post" action={ "/jobs/" (id) "/cancel" } {
                    button.danger { "Cancel job" }
                }
            }
        },
    )
}

fn step_index(stage: &Stage) -> Option<usize> {
    Some(match stage {
        Stage::Requested => 0,
        Stage::Clarifying => 1,
        Stage::Spec => 2,
        Stage::Plan => 3,
        Stage::Implementing { .. } => 4,
        Stage::Verifying { .. } => 5,
        Stage::PrOpen { .. } => 6,
        Stage::CiAndReview { .. } => 7,
        Stage::Merged => 8,
        Stage::Cancelled | Stage::Failed { .. } => return None,
    })
}

fn steps(job: &Job) -> Markup {
    let Some(current) = step_index(job.stage()) else {
        return html! {};
    };
    let merged = *job.stage() == Stage::Merged;
    html! {
        ol.steps aria-label="Job stages" {
            @for (i, step) in STEPS.iter().enumerate() {
                @let class = if i < current || merged { "done" } else if i == current { "current" } else { "" };
                li class=(class) aria-current=[(i == current).then_some("step")] { (step) }
            }
        }
    }
}

fn status_card(id: i64, job: &Job, settings: &Settings) -> Markup {
    match (job.stage(), job.turn()) {
        (_, Turn::User(Ask::Answers { questions })) => html! {
            form.card.needs-you method="post" action={ "/jobs/" (id) "/answers" } {
                h2 { "The agent has questions" }
                @for (i, question) in questions.iter().enumerate() {
                    div.question {
                        label for={ "answer_" (i) } { (question.text) }
                        textarea id={ "answer_" (i) } name={ "answer_" (i) } required {}
                    }
                }
                button.primary { "Send answers" }
            }
        },
        (_, Turn::User(ask @ (Ask::SpecApproval | Ask::PlanApproval))) => {
            let what = if *ask == Ask::SpecApproval {
                "spec"
            } else {
                "plan"
            };
            html! {
                div.card.needs-you {
                    h2 { "Review the " (what) " below" }
                    div.row {
                        form method="post" action={ "/jobs/" (id) "/approve" } {
                            button.primary { "Approve " (what) }
                        }
                        form.grow method="post" action={ "/jobs/" (id) "/changes" } {
                            textarea name="feedback" required aria-label="What should change"
                                placeholder="Or say what should change" {}
                            p { button { "Request changes" } }
                        }
                    }
                }
            }
        }
        (
            Stage::Verifying { pr },
            Turn::User(Ask::PublishApproval {
                summary: Some(summary),
            }),
        ) => publish_card(id, job, *pr, summary),
        (Stage::Failed { reason }, _) => html! {
            div.card.failed {
                h2 { "Failed" }
                p.empty { (reason) }
            }
        },
        _ => html! {
            div.card {
                (turn_badge(job, settings)) " " (turn_detail(job, settings))
            }
        },
    }
}

/// Whether the job waits for the factory to collect what the publish
/// approval shows.
fn collecting(job: &Job) -> bool {
    matches!(job.next_command(), Some(Command::Summarize { .. }))
}

fn in_review(job: &Job) -> bool {
    matches!(job.next_command(), Some(Command::WatchPr { .. }))
}

struct Status {
    lane: Lane,
    class: &'static str,
    text: &'static str,
}

/// One match decides the lane and the badge, so they cannot disagree.
fn status(job: &Job, settings: &Settings) -> Status {
    let (lane, class, text) = match (job.stage(), job.turn()) {
        (Stage::Merged, _) => (Lane::Done, "ok", "merged"),
        (Stage::Failed { .. }, _) => (Lane::Done, "bad", "failed"),
        (Stage::Cancelled, _) => (Lane::Done, "factory", "cancelled"),
        (Stage::Requested, _) => (Lane::Factory, "factory", "queued"),
        (_, Turn::User(_)) if !collecting(job) => (Lane::You, "you", "needs you"),
        _ if in_review(job) && settings.auto_merge => {
            (Lane::Factory, "factory", "merges when green")
        }
        _ if in_review(job) => (Lane::You, "you", "in review"),
        (_, Turn::Agent(_)) => (Lane::Agent, "agent", "agent working"),
        (_, Turn::User(_) | Turn::Factory | Turn::Done) => {
            (Lane::Factory, "factory", "factory working")
        }
    };
    Status { lane, class, text }
}

fn turn_badge(job: &Job, settings: &Settings) -> Markup {
    let Status { class, text, .. } = status(job, settings);
    html! { span class={ "badge " (class) } { (text) } }
}

fn first_line(text: &str) -> String {
    const MAX: usize = 80;
    let line = text.lines().next().unwrap_or_default();
    if line.chars().count() > MAX {
        format!("{}…", line.chars().take(MAX).collect::<String>())
    } else {
        line.to_owned()
    }
}

fn stage_label(job: &Job) -> String {
    let of = |pr: usize| format!("PR {} of {}", pr + 1, job.plan().len());
    match job.stage() {
        Stage::Requested => "requested".into(),
        Stage::Clarifying => "clarifying".into(),
        Stage::Spec => "spec".into(),
        Stage::Plan => "plan".into(),
        Stage::Implementing { pr } => format!("implementing, {}", of(*pr)),
        Stage::Verifying { pr } => format!("verifying, {}", of(*pr)),
        Stage::PrOpen { pr } => format!("pr-open, {}", of(*pr)),
        Stage::CiAndReview { pr } => format!("ci-and-review, {}", of(*pr)),
        Stage::Merged => "merged".into(),
        Stage::Cancelled => "cancelled".into(),
        Stage::Failed { .. } => "failed".into(),
    }
}

fn turn_detail(job: &Job, settings: &Settings) -> String {
    match job.turn() {
        Turn::Agent(task) => format!("The agent is {}.", task_label(task)),
        Turn::Factory => match job.next_command() {
            Some(Command::RunChecks { .. }) => {
                "The factory is running the project's checks.".into()
            }
            Some(Command::Publish { .. }) => {
                "The factory is pushing the branch and opening the pull request.".into()
            }
            Some(Command::WatchPr { number, .. }) if settings.auto_merge => format!(
                "Auto-merge is on. The factory merges pull request #{number} once all its \
                 checks pass, unless review comments come first, which go to the agent. It \
                 looks every {} seconds.",
                settings.poll_seconds
            ),
            Some(Command::WatchPr { number, .. }) => {
                format!(
                    "Pull request #{number} waits for CI and your review on GitHub. Merge it \
                     when it is ready, or leave review comments and the agent addresses them. \
                     The factory looks every {} seconds.",
                    settings.poll_seconds
                )
            }
            Some(Command::RunAgent { .. } | Command::Summarize { .. }) | None => {
                "Waiting for a free worker.".into()
            }
        },
        Turn::User(_) if collecting(job) => {
            "The factory is collecting the commits and the diff stat for your approval.".into()
        }
        Turn::User(_) => "Waiting for you.".into(),
        Turn::Done => match job.stage() {
            Stage::Merged => "Every planned pull request is merged.".into(),
            _ => "This job is cancelled.".into(),
        },
    }
}

fn task_label(task: &Task) -> &'static str {
    match task {
        Task::Clarify { .. } => "reading the repository",
        Task::WriteSpec => "writing the spec",
        Task::WritePlan => "writing the plan",
        Task::Implement { .. } => "implementing the pull request",
        Task::Answer { .. } => "reading your answers",
        Task::Revise { .. } => "revising after your feedback",
        Task::FixChecks { .. } => "fixing failed checks",
        Task::FixCi { .. } => "fixing the CI failure",
        Task::AddressReview { .. } => "addressing review comments",
        Task::Rebase { .. } => "rebasing onto the newest base branch to resolve a conflict",
    }
}

fn publish_card(id: i64, job: &Job, pr: usize, summary: &BranchSummary) -> Markup {
    let planned = &job.plan()[pr];
    html! {
        div.card.needs-you {
            h2 { "Approve before the first push of PR " (pr + 1) " of " (job.plan().len()) }
            @if let Some(description) = &planned.description {
                h3 { (description.title) }
                pre.doc { (description.body) }
            } @else {
                p.empty { "The agent wrote no title or description." }
            }
            h3 { "Commits" }
            @if summary.commits.is_empty() {
                p.empty { "The branch has no commits after the base branch." }
            } @else {
                ul.commits {
                    @for commit in &summary.commits {
                        li { (commit) }
                    }
                }
            }
            h3 { "Diff stat" }
            @if summary.diff_stat.is_empty() {
                p.empty { "The branch changes no files." }
            } @else {
                pre.doc { (summary.diff_stat) }
            }
            div.row {
                form method="post" action={ "/jobs/" (id) "/approve" } {
                    button.primary { "Approve and push" }
                }
                form.grow method="post" action={ "/jobs/" (id) "/changes" } {
                    textarea name="feedback" required aria-label="What should change"
                        placeholder="Or say what should change" {}
                    p { button { "Request changes" } }
                }
            }
        }
    }
}
