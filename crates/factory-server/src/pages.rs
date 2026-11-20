use std::collections::HashMap;

use axum::http::StatusCode;
use factory_core::{
    Ask, BranchSummary, Command, EFFORTS, Gates, Job, PlanUsage, Stage, Task, Turn, UsageWindow,
    Verdict,
};
use maud::{DOCTYPE, Markup, PreEscaped, html};

use crate::cost::{format_cost, merge_fraction};
use crate::github::RepoReport;
use crate::mcp::McpServer;
use crate::projects::Project;
use crate::projects::{StageEfforts, StageModels};
use crate::settings::Settings;
use crate::store::{JobEvent, JobRow, LogEntry, ProjectTotals};
use crate::timeline::timeline;

pub const STYLESHEET: &str = include_str!("../static/style.css");
/// htmx 2.0.4, vendored so the dashboard loads no script from a CDN. The
/// tailnet host it runs on cannot reach one.
pub const HTMX: &str = include_str!("../static/htmx.min.js");
const PROJECT_PICKER: &str = include_str!("../static/project-picker.js");
const PASTE_IMAGES: &str = include_str!("../static/paste-images.js");
/// Keeps the log scroll place and a half-typed form across a poll's swap. It
/// runs once on the job page, outside every swapped region, so a swap never
/// re-runs or removes it.
const JOB_UPDATES: &str = include_str!("../static/job-updates.js");

/// Each live region polls its own fragment on this interval, so a state change
/// shows without a reload.
const POLL_TRIGGER: &str = "every 5s";

fn fragment_url(region: &str, project: &str) -> String {
    format!(
        "/fragments/{region}?project={}",
        crate::web::query_escape(project)
    )
}

fn layout(user: &str, title: &str, body: &Markup) -> Markup {
    page(user, title, &html! {}, body)
}

fn page(user: &str, title: &str, nav: &Markup, body: &Markup) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (title) " · factory" }
                link rel="stylesheet" href="/style.css";
                script defer src="/htmx.min.js" {}
            }
            body {
                div.topbar {
                    div.topbar-inner {
                        a.brand href="/" { span.brand-mark aria-hidden="true" {} "factory" }
                        div.topbar-end {
                            (nav)
                            a.nav href="/onboard" { "Onboard" }
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

#[allow(
    clippy::too_many_arguments,
    reason = "the page takes each value its handler read"
)]
pub fn index(
    user: &str,
    project_names: &[String],
    selected: Option<&str>,
    jobs: &[JobRow],
    costs: &HashMap<i64, f64>,
    totals: ProjectTotals,
    plan_usage: Option<&PlanUsage>,
    auto_merge: bool,
) -> Markup {
    let all: Vec<&JobRow> = jobs
        .iter()
        .filter(|row| Some(row.project.as_str()) == selected)
        .collect();
    let nav = selected.map_or_else(
        || html! {},
        |selected| project_selector(project_names, selected),
    );
    page(
        user,
        "Jobs",
        &nav,
        &html! {
            (plan_usage_region(plan_usage))
            @if let Some(selected) = selected {
                (composer(selected))
                (board(&all, costs, auto_merge, selected))
                (stats(totals, selected))
            } @else {
                div.card {
                    p.empty { "No projects are configured." }
                    p { a.nav href="/onboard" { "Onboard a project" } }
                }
            }
        },
    )
}

/// The new-request form as a one-line bar. It grows, and shows the image
/// picker, once it has focus or text, so it takes little room on a board
/// that is opened more often to check on jobs than to file one.
fn composer(project: &str) -> Markup {
    html! {
        form.composer method="post" action="/jobs" enctype="multipart/form-data" {
            input type="hidden" name="project" value=(project);
            label.visually-hidden for="request" { "New request for " (project) }
            div.composer-line {
                textarea #request name="request" required rows="1"
                    placeholder={ "Describe a change for " (project) "…" } {}
                button.primary { "Request" }
            }
            div.composer-more {
                div.file-picker {
                    input #images type="file" name="images" multiple
                        accept="image/png,image/jpeg,image/gif,image/webp"
                        class="visually-hidden";
                    label.file-button for="images" { "Attach images" }
                    span.hint { "PNG, JPEG, GIF or WebP. You can also paste them into the box above." }
                }
                ul #pasted-images.pasted-images {}
            }
            script { (PreEscaped(PASTE_IMAGES)) }
        }
    }
}

const NO_VALUE: &str = "–";

/// The project's totals as one line under the board. A figure that is zero
/// or would divide by zero shows a dash.
#[allow(clippy::cast_precision_loss, reason = "job counts stay far below 2^52")]
pub fn stats(totals: ProjectTotals, project: &str) -> Markup {
    let created = totals.jobs_created;
    let merged = totals.jobs_merged;
    let average = (merged > 0).then(|| format_cost(totals.usd / merged as f64));
    let rate = if created > 0 {
        format!("{:.0}%", merge_fraction(created, merged) * 100.0)
    } else {
        NO_VALUE.to_owned()
    };
    let figures = [
        ("Total API cost", format_cost(totals.usd)),
        ("Jobs created", count_or_dash(created)),
        ("Jobs merged", count_or_dash(merged)),
        ("Merge rate", rate),
        (
            "Cost per merged job",
            average.unwrap_or_else(|| NO_VALUE.to_owned()),
        ),
    ];
    html! {
        dl.totals #stats hx-get=(fragment_url("stats", project))
            hx-trigger=(POLL_TRIGGER) hx-swap="outerHTML" {
            @for (label, value) in figures {
                div.total {
                    dt { (label) }
                    dd { (value) }
                }
            }
        }
    }
}

/// The plan usage tile sits in a wrapper that polls on its own interval, so
/// the tile hides when the cache is empty, stale or errored and returns on a
/// later poll once it is fresh. htmx swaps the wrapper's contents, so an empty
/// response removes the tile while the wrapper keeps polling.
fn plan_usage_region(usage: Option<&PlanUsage>) -> Markup {
    html! {
        div #usage hx-get="/fragments/plan-usage" hx-trigger=(POLL_TRIGGER) {
            (plan_usage(usage))
        }
    }
}

/// The plan usage tile, or nothing when the handler passes `None`.
pub fn plan_usage(usage: Option<&PlanUsage>) -> Markup {
    html! {
        @if let Some(usage) = usage {
            (plan_usage_panel(usage))
        }
    }
}

/// The account-wide panel of Claude subscription limits: the 5-hour window,
/// the weekly window, and each per-model window the plan reports, such as
/// Fable. It reads the same for every project, because the limits belong to
/// the account.
fn plan_usage_panel(usage: &PlanUsage) -> Markup {
    html! {
        section.plan-usage {
            h2 { "Claude plan usage" }
            ul.usage-windows {
                (usage_window("5-hour", &usage.five_hour))
                (usage_window("Weekly", &usage.seven_day))
                @for model in &usage.model_scoped {
                    (usage_window(&model.display_name, &model.window))
                }
            }
        }
    }
}

fn usage_window(label: &str, window: &UsageWindow) -> Markup {
    let percent = window.percent_used();
    let fill = format!("{:.1}%", percent.clamp(0.0, 100.0));
    let reset = format!("resets {}", format_reset(window.resets_at));
    html! {
        li.usage-window title=(reset) {
            span.usage-name { (label) }
            span.usage-track { span.usage-fill style={ "width: " (fill) } {} }
            span.usage-percent { (format!("{percent:.0}% used")) }
            span.usage-reset { (reset) }
        }
    }
}

/// `unix`, a Unix timestamp in seconds, as `YYYY-MM-DD HH:MM UTC`.
fn format_reset(unix: i64) -> String {
    let (year, month, day) = civil_from_days(unix.div_euclid(86_400));
    let secs = unix.rem_euclid(86_400);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02} UTC",
        secs / 3600,
        secs % 3600 / 60
    )
}

/// The civil date (year, month 1 to 12, day 1 to 31) for `days` days after
/// 1970-01-01, by Howard Hinnant's `civil_from_days`.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (if month <= 2 { year + 1 } else { year }, month, day)
}

fn count_or_dash(count: i64) -> String {
    if count > 0 {
        count.to_string()
    } else {
        NO_VALUE.to_owned()
    }
}

fn project_selector(names: &[String], selected: &str) -> Markup {
    html! {
        form.project-picker method="get" action="/" {
            label.visually-hidden for="project" { "Project" }
            select #project name="project" {
                @for name in names {
                    option selected[name.as_str() == selected] { (name) }
                }
            }
            noscript { button { "Go" } }
            script { (PreEscaped(PROJECT_PICKER)) }
        }
    }
}

pub fn settings(
    user: &str,
    settings: &Settings,
    servers: &[McpServer],
    projects: &[(String, StageModels, StageEfforts, bool, bool)],
    notice: Option<&str>,
) -> Markup {
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
                    label for="poll_seconds" { "Seconds between looks at a pull request" }
                    input #poll_seconds type="number" name="poll_seconds" min="10" required
                        value=(settings.poll_seconds);
                    p.hint { "Also the wait before retrying a failed push or GitHub call." }
                }
                button.primary { "Save" }
            }
            (mcp_servers(servers))
            @for (name, models, efforts, auto_merge, llm_review) in projects {
                (project_form(name, models, efforts, *auto_merge, *llm_review))
            }
        },
    )
}

/// The "MCP servers" section: one edit form per registered server, then a form
/// to add one. A project turns a server on and gives its header values in the
/// project's own settings form, so this section carries no per-project state.
fn mcp_servers(servers: &[McpServer]) -> Markup {
    html! {
        div.card {
            h2 { "MCP servers" }
            p.hint { "Remote MCP servers over HTTPS, offered to agent runs. A project turns one on and picks its stages in the project's own settings below. Header values are entered per project." }
            @if servers.is_empty() {
                p.meta { "No servers yet." }
            }
        }
        @for server in servers {
            (mcp_server_form(server))
        }
        form.card method="post" action="/settings/mcp" {
            h2 { "Add a server" }
            div.field {
                label for="mcp-new-name" { "Name" }
                input #mcp-new-name type="text" name="name" required;
                p.hint { "1 to 32 characters of a-z, 0-9 and -. It is the key in a run's MCP config and part of the proxy path." }
            }
            div.field {
                label for="mcp-new-url" { "URL" }
                input #mcp-new-url.wide type="url" name="url" required
                    placeholder="https://mcp.context7.com/mcp";
                p.hint { "An https URL to the server's streamable-HTTP endpoint." }
            }
            div.field {
                label for="mcp-new-headers" { "Header names" }
                textarea #mcp-new-headers name="headers" placeholder="Authorization" {}
                p.hint { "Optional, one per line. Each project gives its own value for these, such as an API key. A keyless server needs none." }
            }
            button.primary { "Add server" }
        }
    }
}

/// One server's edit form: its URL and header names, a Save button and a Delete
/// button that posts the same form to the delete route. The name is fixed,
/// because it keys the stored secrets and the proxy path.
fn mcp_server_form(server: &McpServer) -> Markup {
    let action = format!("/settings/mcp/{}", server.name);
    let delete = format!("/settings/mcp/{}/delete", server.name);
    html! {
        form.card method="post" action=(action) {
            h2 { (server.name) }
            div.field {
                label for={ "mcp-" (server.name) "-url" } { "URL" }
                input id={ "mcp-" (server.name) "-url" }.wide type="url" name="url" required
                    value=(server.url);
            }
            div.field {
                label for={ "mcp-" (server.name) "-headers" } { "Header names" }
                textarea id={ "mcp-" (server.name) "-headers" } name="headers" {
                    (server.headers.join("\n"))
                }
                p.hint { "One per line. Each project gives its own value for these." }
            }
            p.hint { "Changing the URL's host or port deletes every project's stored values for this server, so they must be entered again. Changing only the path keeps them." }
            div.row {
                button.primary { "Save" }
                button.danger formaction=(delete) formnovalidate { "Delete" }
            }
        }
    }
}

/// One project's form on the settings page: its two flags and a model field
/// with an effort selector beside it for each of the five stages.
fn project_form(
    name: &str,
    models: &StageModels,
    efforts: &StageEfforts,
    auto_merge: bool,
    llm_review: bool,
) -> Markup {
    html! {
        form.card method="post" action={ "/settings/projects/" (name) } {
            h2 { (name) }
            div.field {
                label.check {
                    input type="checkbox" name="auto_merge" value="on" checked[auto_merge];
                    " Merge pull requests when their checks pass"
                }
                p.hint { "The factory squash-merges once every check passed and no review comment waits for the agent. Off, you merge on GitHub." }
            }
            div.field {
                label.check {
                    input type="checkbox" name="llm_review" value="on" checked[llm_review];
                    " Review with an LLM before merge"
                }
                p.hint { "When the checks pass, the review model reads the branch and asks for changes or approves. Changes go back to the agent." }
            }
            p.hint { "The model and effort each stage runs with. An empty model means no model; effort is medium unless you change it." }
            @for (stage, model, effort) in [
                ("clarify", &models.clarify, &efforts.clarify),
                ("spec", &models.spec, &efforts.spec),
                ("plan", &models.plan, &efforts.plan),
                ("implement", &models.implement, &efforts.implement),
                ("review", &models.review, &efforts.review),
            ] {
                div.row {
                    div.field {
                        label for={ (name) "-" (stage) } { (stage) " model" }
                        input id={ (name) "-" (stage) } type="text" name=(stage)
                            value=(model.as_deref().unwrap_or_default());
                    }
                    div.field {
                        label for={ (name) "-" (stage) "-effort" } { (stage) " effort" }
                        select id={ (name) "-" (stage) "-effort" } name={ (stage) "_effort" } {
                            @for level in EFFORTS {
                                option value=(level) selected[level == effort.as_str()] {
                                    (level)
                                }
                            }
                        }
                    }
                }
            }
            button.primary { "Save " (name) }
        }
    }
}

/// The values the onboarding form holds on a redraw, so a failed submit keeps
/// what the operator typed.
pub struct OnboardView<'a> {
    /// The project name as typed. Empty means the repository name is used.
    pub name: &'a str,
    pub repo: &'a str,
    pub base: &'a str,
    pub checks: &'a str,
    pub models: &'a StageModels,
    pub auto_merge: bool,
    pub llm_review: bool,
}

/// The onboarding wizard. Step 1 is the project, step 2 the optional models and
/// flags. The second step appears only after the checks in `report` pass, so
/// the operator cannot create a project the checks reject. Before the first
/// submit `report` is `None` and only step 1 shows.
pub fn onboard(user: &str, view: &OnboardView<'_>, report: Option<&RepoReport>) -> Markup {
    let show_step2 = report.is_some_and(|report| report.errors.is_empty());
    layout(
        user,
        "Onboard a project",
        &html! {
            h1 { "Onboard a project" }
            p.meta { "The factory checks the repository as the bot before it creates anything. It creates no project while a required check fails." }
            form.card method="post" action="/onboard" {
                h2 { "Project" }
                div.field {
                    label for="repo" { "Repository" }
                    input #repo.wide type="text" name="repo" required
                        placeholder="owner/name" value=(view.repo);
                    p.hint { "The GitHub repository as owner/name, such as Badger-Embedded/maestro." }
                }
                div.field {
                    label for="name" { "Name" }
                    input #name type="text" name="name" value=(view.name)
                        placeholder="defaults to the repository name";
                    p.hint { "The key jobs use. Letters, digits, dash, underscore and dot." }
                }
                div.field {
                    label for="base" { "Base branch" }
                    input #base type="text" name="base" value=(view.base) placeholder="main";
                    p.hint { "The branch pull requests target. Defaults to main." }
                }
                div.field {
                    label for="checks" { "Local checks command (optional)" }
                    input #checks.wide type="text" name="checks" value=(view.checks);
                    p.hint { "The factory runs this with bash in the worker, in the job's checkout, before it pushes, such as nix develop -c just lint && nix develop -c just test. It is separate from GitHub's CI on the pull request. Leave it empty to push without local checks." }
                }
                @if let Some(report) = report {
                    (onboard_results(report))
                }
                @if show_step2 {
                    h2 { "Optional settings" }
                    p.hint { "The model each stage runs with, and the merge flags. Empty means the default. Skip to create with the defaults a project starts with." }
                    @for (stage, value) in [
                        ("clarify", &view.models.clarify),
                        ("spec", &view.models.spec),
                        ("plan", &view.models.plan),
                        ("implement", &view.models.implement),
                        ("review", &view.models.review),
                    ] {
                        div.field {
                            label for={ "model-" (stage) } { (stage) }
                            input id={ "model-" (stage) } type="text" name=(stage)
                                value=(value.as_deref().unwrap_or_default());
                        }
                    }
                    div.field {
                        label.check {
                            input type="checkbox" name="auto_merge" value="on" checked[view.auto_merge];
                            " Merge pull requests when their checks pass"
                        }
                    }
                    div.field {
                        label.check {
                            input type="checkbox" name="llm_review" value="on" checked[view.llm_review];
                            " Review with an LLM before merge"
                        }
                    }
                }
                div.form-actions {
                    @if show_step2 {
                        button.primary name="action" value="create" { "Create project" }
                        button name="action" value="skip" { "Skip extras and create" }
                        button name="action" value="check" { "Re-run checks" }
                    } @else {
                        button.primary name="action" value="check" { "Run checks" }
                    }
                }
            }
        },
    )
}

/// The checks panel under step 1: the required checks that failed, or a line
/// that they passed, then any warnings, which do not block creation.
fn onboard_results(report: &RepoReport) -> Markup {
    html! {
        div.checks {
            h3 { "Checks" }
            @if report.errors.is_empty() {
                p.check-ok { "All required checks passed." }
            } @else {
                ul.check-errors {
                    @for error in &report.errors {
                        li { (error) }
                    }
                }
            }
            @if !report.warnings.is_empty() {
                ul.check-warnings {
                    @for warning in &report.warnings {
                        li { (warning) }
                    }
                }
            }
        }
    }
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
    Failed,
    Done,
}

/// The board's sections, top to bottom, in the order you act on them. Agent
/// and Factory share Running, and each row's badge says which one has the
/// turn.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    You,
    Failed,
    Running,
    Done,
}

impl Section {
    const ALL: [Section; 4] = [
        Section::You,
        Section::Failed,
        Section::Running,
        Section::Done,
    ];

    fn of(lane: Lane) -> Section {
        match lane {
            Lane::You => Section::You,
            Lane::Failed => Section::Failed,
            Lane::Agent | Lane::Factory => Section::Running,
            Lane::Done => Section::Done,
        }
    }

    fn title(self) -> &'static str {
        match self {
            Section::You => "Needs you",
            Section::Failed => "Failed",
            Section::Running => "Running",
            Section::Done => "Done",
        }
    }

    fn name(self) -> &'static str {
        match self {
            Section::You => "you",
            Section::Failed => "failed",
            Section::Running => "running",
            Section::Done => "done",
        }
    }
}

/// Done lists this many jobs. The older ones sit in a collapsed `<details>`.
const DONE_SHOWN: usize = 5;

pub fn board(
    rows: &[&JobRow],
    costs: &HashMap<i64, f64>,
    auto_merge: bool,
    project: &str,
) -> Markup {
    let in_section = |section: Section| -> Vec<&JobRow> {
        rows.iter()
            .copied()
            .filter(|row| Section::of(status(&row.job, auto_merge).lane) == section)
            .collect()
    };
    let live = rows.iter().any(|row| !row.job.is_finished());
    html! {
        div.board #board hx-get=(fragment_url("board", project))
            hx-trigger=(POLL_TRIGGER) hx-swap="outerHTML" {
            @if !live {
                p.board-quiet { "Nothing needs you and no job is running." }
            }
            @for section in Section::ALL {
                @let jobs = in_section(section);
                @if !jobs.is_empty() {
                    section class={ "inbox inbox-" (section.name()) } {
                        h2 { (section.title()) " " span.count { (jobs.len()) } }
                        @if section == Section::Done && jobs.len() > DONE_SHOWN {
                            (job_rows(&jobs[..DONE_SHOWN], costs, auto_merge))
                            details.older {
                                summary { "Show " (jobs.len() - DONE_SHOWN) " older" }
                                (job_rows(&jobs[DONE_SHOWN..], costs, auto_merge))
                            }
                        } @else {
                            (job_rows(&jobs, costs, auto_merge))
                        }
                    }
                }
            }
        }
    }
}

fn job_rows(rows: &[&JobRow], costs: &HashMap<i64, f64>, auto_merge: bool) -> Markup {
    html! {
        ul.jobs {
            @for row in rows {
                @let lane = status(&row.job, auto_merge).lane;
                li class={ "job job-" (Section::of(lane).name()) } {
                    span.id { "#" (row.id) }
                    a.title href={ "/jobs/" (row.id) } title=(first_line(row.job.request())) {
                        (first_line(row.job.request()))
                    }
                    div.job-sub {
                        (turn_badge(&row.job, auto_merge))
                        @if !row.job.is_finished() { (flow_steps(&row.job)) }
                        @if let Some(detail) = row_detail(&row.job) {
                            span.stage { (detail) }
                        }
                        span.requester title=(row.requested_by) { (row.requested_by) }
                        span.cost { (cost_label(costs.get(&row.id).copied().unwrap_or(0.0))) }
                    }
                    (row_actions(row, lane))
                }
            }
        }
    }
}

/// The stage of a live job, or why a failed one failed. A merged or
/// cancelled job has none, because its badge already says so.
fn row_detail(job: &Job) -> Option<String> {
    match job.stage() {
        Stage::Failed { reason, .. } => Some(first_line(reason)),
        Stage::Merged | Stage::Cancelled => None,
        _ => Some(stage_label(job)),
    }
}

/// Approve for a spec or plan gate, next to a link to read what you approve.
/// Retry for a failed job that can resume. Any other job that waits on you
/// links to its page, where the answer and review controls are.
fn row_actions(row: &JobRow, lane: Lane) -> Markup {
    let page = format!("/jobs/{}", row.id);
    html! {
        @if lane == Lane::You {
            div.job-actions {
                @if let Some(gate) = gate_name(&row.job) {
                    a.link-button href=(page) { "Read " (gate) }
                    form.approve method="post" action={ (page) "/approve" } {
                        input type="hidden" name="project" value=(row.project);
                        button.primary { "Approve " (gate) }
                    }
                } @else {
                    a.link-button href=(page) { "Open" }
                }
            }
        }
        @if lane == Lane::Failed && row.job.can_retry() {
            div.job-actions {
                form.retry method="post" action={ (page) "/retry" } {
                    input type="hidden" name="project" value=(row.project);
                    button.primary { "Retry" }
                }
            }
        }
    }
}

/// The job flow's steps, with the per-pull-request steps once. A job with
/// several pull requests shows the steps of the one it is on, and its stage
/// label says which.
const FLOW: [&str; 9] = [
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

fn flow_index(stage: &Stage) -> Option<usize> {
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

/// A row of small segments, one per step of [`FLOW`]: done, current, or to
/// come.
fn flow_steps(job: &Job) -> Markup {
    let Some(current) = flow_index(job.stage()) else {
        return html! {};
    };
    let label = format!("Step {} of {}: {}", current + 1, FLOW.len(), FLOW[current]);
    html! {
        span.steps role="img" aria-label=(label) title=(label) {
            @for i in 0..FLOW.len() {
                @let class = match i.cmp(&current) {
                    std::cmp::Ordering::Less => "step done",
                    std::cmp::Ordering::Equal => "step current",
                    std::cmp::Ordering::Greater => "step",
                };
                span class=(class) {}
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

#[allow(
    clippy::too_many_arguments,
    reason = "the page takes each value its handler read"
)]
pub fn job(
    user: &str,
    row: &JobRow,
    project: Option<&Project>,
    settings: &Settings,
    logs: &[LogEntry],
    events: &[JobEvent],
    images: &[String],
    cost_usd: f64,
    models: &StageModels,
    efforts: &StageEfforts,
    auto_merge: bool,
) -> Markup {
    let job = &row.job;
    let repo = project.map(|project| format!("https://github.com/{}", project.repo));
    let repo_url = repo.as_deref();
    let id = row.id;
    page(
        user,
        &format!("Job #{id}"),
        &html! {},
        &html! {
            (job_poll(false, id, job))
            (job_header(id, row, repo_url, cost_usd))
            div.job-layout {
                div.job-now {
                    (status_region(false, id, job, settings, auto_merge))
                    p.meta { "Approval gates: " (gates_line(job.gates())) }
                }
                aside.job-side { (timeline_region(false, job, events, models, efforts)) }
                div.job-main {
                    (request_card(id, job, images))
                    (spec_region(false, job))
                    (plan_region(false, job, repo_url))
                    (reviews_region(false, job))
                    (log_region(false, logs))
                    @if *job.turn() != Turn::Done {
                        form.footer-actions method="post" action={ "/jobs/" (id) "/cancel" } {
                            button.danger { "Cancel job" }
                        }
                    }
                }
            }
            script { (PreEscaped(JOB_UPDATES)) }
        },
    )
}

/// The job page's live regions, re-rendered for a poll as out-of-band swaps.
/// It renders the same region bodies the full page does, from the same data,
/// so a swapped region cannot disagree with the page it updates. The poller
/// only appears here once the job is terminal, to replace itself with a
/// version that no longer polls; a running job keeps the poller the page set
/// up, timer and all.
#[allow(
    clippy::too_many_arguments,
    reason = "the fragment takes each value its handler read"
)]
pub fn job_fragment(
    row: &JobRow,
    project: Option<&Project>,
    settings: &Settings,
    logs: &[LogEntry],
    events: &[JobEvent],
    cost_usd: f64,
    models: &StageModels,
    efforts: &StageEfforts,
    auto_merge: bool,
) -> Markup {
    let job = &row.job;
    let repo = project.map(|project| format!("https://github.com/{}", project.repo));
    let repo_url = repo.as_deref();
    html! {
        @if terminal(job) { (job_poll(true, row.id, job)) }
        (cost_badge(true, cost_usd))
        (timeline_region(true, job, events, models, efforts))
        (status_region(true, row.id, job, settings, auto_merge))
        (spec_region(true, job))
        (plan_region(true, job, repo_url))
        (reviews_region(true, job))
        (log_region(true, logs))
    }
}

/// Merged, failed and cancelled jobs do not change again, so their page and
/// its fragments carry no polling attribute.
fn terminal(job: &Job) -> bool {
    matches!(
        job.stage(),
        Stage::Merged | Stage::Failed { .. } | Stage::Cancelled
    )
}

/// The hidden element that polls the job fragment. A running job polls every
/// `POLL_TRIGGER` and lets htmx apply the out-of-band swaps the response
/// carries; `hx-swap="none"` keeps it from swapping the response into itself.
/// A terminal job gets a bare element with no polling attribute, so the loop
/// stops both on first load and when a running page reaches a terminal stage.
fn job_poll(oob: bool, id: i64, job: &Job) -> Markup {
    let polling = !terminal(job);
    html! {
        div #job-poll hx-swap-oob=[oob.then_some("true")]
            hx-get=[polling.then(|| format!("/jobs/{id}/fragment"))]
            hx-trigger=[polling.then_some(POLL_TRIGGER)]
            hx-swap=[polling.then_some("none")] {}
    }
}

/// The cost pill on the title row. A poll updates it out of band, so the title
/// stays put while the figure changes.
fn cost_badge(oob: bool, cost_usd: f64) -> Markup {
    html! {
        span #job-cost.badge.cost hx-swap-oob=[oob.then_some("true")] {
            (cost_label(cost_usd))
        }
    }
}

/// The timeline, in a region a poll swaps. It sits in the side column.
fn timeline_region(
    oob: bool,
    job: &Job,
    events: &[JobEvent],
    models: &StageModels,
    efforts: &StageEfforts,
) -> Markup {
    html! {
        div #job-timeline hx-swap-oob=[oob.then_some("true")] {
            (timeline(job, events, models, efforts))
        }
    }
}

/// The status and its controls. A poll swaps this region, so the approval or
/// answer form appears when the job enters a gate. `job-updates.js` skips the
/// swap while a field here has focus, so a half-typed answer survives a poll.
fn status_region(oob: bool, id: i64, job: &Job, settings: &Settings, auto_merge: bool) -> Markup {
    html! {
        div #job-status hx-swap-oob=[oob.then_some("true")] {
            (status_card(id, job, settings, auto_merge))
        }
    }
}

fn spec_region(oob: bool, job: &Job) -> Markup {
    html! {
        div #job-spec hx-swap-oob=[oob.then_some("true")] { (spec_card(job)) }
    }
}

fn plan_region(oob: bool, job: &Job, repo_url: Option<&str>) -> Markup {
    html! {
        div #job-plan hx-swap-oob=[oob.then_some("true")] { (plan_card(job, repo_url)) }
    }
}

/// The LLM review passes, in a region a poll swaps. A pass is added each time
/// the reviewer runs, so the card grows while a job is live.
fn reviews_region(oob: bool, job: &Job) -> Markup {
    html! {
        div #job-reviews hx-swap-oob=[oob.then_some("true")] { (reviews_card(job)) }
    }
}

/// The log, in a region a poll swaps. `job-updates.js` keeps the reader's
/// scroll place across the swap: pinned to the bottom, or where they scrolled.
fn log_region(oob: bool, logs: &[LogEntry]) -> Markup {
    html! {
        div #job-log hx-swap-oob=[oob.then_some("true")] { (log_card(logs)) }
    }
}

fn job_header(id: i64, row: &JobRow, repo_url: Option<&str>, cost_usd: f64) -> Markup {
    let job = &row.job;
    html! {
        div.title-row {
            h1 { "#" (id) " " (first_line(job.request())) }
            (cost_badge(false, cost_usd))
        }
        p.meta {
            @if let Some(url) = repo_url {
                a href=(url) { (row.project) }
            } @else {
                (row.project)
            }
            " · requested by " (row.requested_by) " · " (row.created_at) " UTC"
        }
    }
}

fn request_card(id: i64, job: &Job, images: &[String]) -> Markup {
    html! {
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
    }
}

fn spec_card(job: &Job) -> Markup {
    html! {
        @if let Some(spec) = job.spec() {
            div.card {
                h2 { "Spec" }
                pre.doc { (spec) }
            }
        }
    }
}

fn plan_card(job: &Job, repo_url: Option<&str>) -> Markup {
    html! {
        @if !job.plan().is_empty() {
            div.card {
                h2 { "Plan" }
                ol.plan {
                    @for pr in job.plan() {
                        li {
                            strong { (pr.title) }
                            @if let Some(number) = pr.number {
                                " · "
                                @if let Some(url) = repo_url {
                                    a href={ (url) "/pull/" (number) } target="_blank" rel="noopener noreferrer" { "#" (number) }
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
    }
}

/// Every review pass the reviewer has run, oldest first, each with its number,
/// verdict and feedback. The feedback stays on the factory, never on the pull
/// request. An empty list renders nothing, so the OOB wrapper has a target
/// before the first pass.
fn reviews_card(job: &Job) -> Markup {
    html! {
        @if !job.reviews().is_empty() {
            div.card {
                h2 { "LLM review" }
                ol.reviews {
                    @for pass in job.reviews() {
                        li {
                            div.review-head {
                                span.review-number { "Pass " (pass.number) }
                                span class={ "badge " (verdict_class(pass.verdict)) } {
                                    (verdict_label(pass.verdict))
                                }
                            }
                            @if let Some(feedback) = &pass.feedback {
                                pre.doc { (feedback) }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn verdict_class(verdict: Verdict) -> &'static str {
    match verdict {
        Verdict::Approve => "ok",
        Verdict::Changes => "you",
    }
}

fn verdict_label(verdict: Verdict) -> &'static str {
    match verdict {
        Verdict::Approve => "approved",
        Verdict::Changes => "changes requested",
    }
}

fn log_card(logs: &[LogEntry]) -> Markup {
    html! {
        @if !logs.is_empty() {
            div.card {
                div.card-head {
                    h2 { "Log" }
                    span.log-count { (logs.len()) @if logs.len() == 1 { " line" } @else { " lines" } }
                }
                ol.log {
                    @for entry in logs {
                        li class={ "log-" (entry.kind) } {
                            span.log-at { (entry.at.get(11..).unwrap_or(&entry.at)) }
                            span.log-text { (entry.text) }
                        }
                    }
                }
            }
        }
    }
}

fn status_card(id: i64, job: &Job, settings: &Settings, auto_merge: bool) -> Markup {
    match (job.stage(), job.turn()) {
        (_, Turn::User(Ask::Answers { questions })) => html! {
            form.card.needs-you method="post" action={ "/jobs/" (id) "/answers" } {
                div.card-head {
                    span.badge.you { "needs you" }
                    h2 { "The agent has questions" }
                }
                @for (i, question) in questions.iter().enumerate() {
                    div.question {
                        label for={ "answer_" (i) } {
                            span.question-number { (i + 1) }
                            span { (question.text) }
                        }
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
                    div.card-head {
                        span.badge.you { "needs you" }
                        h2 { "Review the " (what) " below" }
                    }
                    div.row.actions {
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
        (_, Turn::User(Ask::ReviewStopped { reason })) => html! {
            div.card.needs-you {
                div.card-head {
                    span.badge.you { "needs you" }
                    h2 { "The review stopped before merging" }
                }
                p.status-detail { (reason) }
            }
        },
        (Stage::Failed { reason, .. }, _) => html! {
            div.card.failed {
                div.card-head {
                    span.badge.bad { "failed" }
                    h2 { "The job failed" }
                }
                p.failure-reason { (reason) }
                @if job.can_retry() {
                    div.row.actions {
                        form method="post" action={ "/jobs/" (id) "/retry" } {
                            button.primary { "Retry" }
                        }
                    }
                }
            }
        },
        _ => html! {
            div.card.status {
                (turn_badge(job, auto_merge))
                span.status-detail { (turn_detail(job, settings, auto_merge)) }
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
fn status(job: &Job, auto_merge: bool) -> Status {
    let (lane, class, text) = match (job.stage(), job.turn()) {
        (Stage::Merged, _) => (Lane::Done, "ok", "merged"),
        (Stage::Failed { .. }, _) => (Lane::Failed, "bad", "failed"),
        (Stage::Cancelled, _) => (Lane::Done, "factory", "cancelled"),
        (Stage::Requested, _) => (Lane::Factory, "factory", "queued"),
        (_, Turn::User(_)) if !collecting(job) => (Lane::You, "you", "needs you"),
        _ if in_review(job) && auto_merge => (Lane::Factory, "factory", "merges when green"),
        _ if in_review(job) => (Lane::You, "you", "in review"),
        (_, Turn::Agent(_)) => (Lane::Agent, "agent", "agent working"),
        (_, Turn::User(_) | Turn::Factory | Turn::RerunCi { .. } | Turn::Done) => {
            (Lane::Factory, "factory", "factory working")
        }
    };
    Status { lane, class, text }
}

fn turn_badge(job: &Job, auto_merge: bool) -> Markup {
    let Status { class, text, .. } = status(job, auto_merge);
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

fn turn_detail(job: &Job, settings: &Settings, auto_merge: bool) -> String {
    match job.turn() {
        Turn::Agent(task) => format!("The agent is {}.", task_label(task)),
        Turn::Factory => match job.next_command() {
            Some(Command::RunChecks { .. }) => {
                "The factory is running the project's checks.".into()
            }
            Some(Command::Publish { .. }) => {
                "The factory is pushing the branch and opening the pull request.".into()
            }
            Some(Command::WatchPr { number, .. }) if auto_merge => format!(
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
            Some(
                Command::RunAgent { .. } | Command::Summarize { .. } | Command::RerunCi { .. },
            )
            | None => "Waiting for a free worker.".into(),
        },
        Turn::User(_) if collecting(job) => {
            "The factory is collecting the commits and the diff stat for your approval.".into()
        }
        Turn::RerunCi { .. } => "The factory is rerunning the failed CI runs.".into(),
        Turn::User(_) => "Waiting for you.".into(),
        Turn::Done => match job.stage() {
            Stage::Merged => "Every planned pull request is merged.".into(),
            _ => "This job is cancelled.".into(),
        },
    }
}

pub(crate) fn task_label(task: &Task) -> &'static str {
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
        Task::Review => "reviewing the pull request",
    }
}

fn publish_card(id: i64, job: &Job, pr: usize, summary: &BranchSummary) -> Markup {
    let planned = &job.plan()[pr];
    html! {
        div.card.needs-you {
            div.card-head {
                span.badge.you { "needs you" }
                h2 { "Approve before the first push of PR " (pr + 1) " of " (job.plan().len()) }
            }
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
                pre.doc.diff-stat { (summary.diff_stat) }
            }
            div.row.actions {
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
