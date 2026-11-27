use std::collections::HashMap;

use axum::http::StatusCode;
use factory_core::{
    Ask, BranchSummary, Command, CommentState, DELETED_PREREQUISITE, Gates, Job, PlanUsageSnapshot,
    PrMode, PrerequisiteState, RenovatePr, RenovateWait, Stage, Task, Turn, UsageWindow, Verdict,
    Wait,
};
use maud::{DOCTYPE, Markup, PreEscaped, html};

use crate::check::{CheckOutcome, ToolList};
use crate::cost::{format_cost, merge_fraction};
use crate::forgejo_instances::ForgejoInstance;
use crate::github::RepoReport;
use crate::mcp::{McpServer, McpStages, McpToolState, McpTransport};
use factory_runner::{ForgeAccount, ForgeKind, OpenPr};

use crate::projects::{Project, ProjectLabel, RenovateAgent};
use crate::projects::{StageAgents, StageEfforts, StageModels};
use crate::settings::Settings;
use crate::store::{
    CodexPrice, Collaborator, JobEvent, JobFile, JobRow, LogEntry, Maintenance, PackagingType,
    PrerequisiteView, ProjectGroup, ProjectTotals,
};
use crate::timeline::timeline;
use crate::workers::Slot;

pub const STYLESHEET: &str = include_str!("../static/style.css");
/// htmx 2.0.4, vendored so the dashboard loads no script from a CDN. The
/// tailnet host it runs on cannot reach one.
pub const HTMX: &str = include_str!("../static/htmx.min.js");
const PROJECT_PICKER: &str = include_str!("../static/project-picker.js");
const ONBOARD_FORGE: &str = include_str!("../static/onboard-forge.js");
const SOURCE_LINK: &str = include_str!("../static/source-link.js");
const PR_PICKER: &str = include_str!("../static/pr-picker.js");
const PASTE_IMAGES: &str = include_str!("../static/paste-images.js");
const MCP_ROWS: &str = include_str!("../static/mcp-rows.js");
const NOTIFICATIONS: &str = include_str!("../static/notifications.js");
const PERMISSION_BAR: &str = include_str!("../static/permission-bar.js");
const NOTIFICATION_PERMISSION: &str = include_str!("../static/notification-permission.js");
const SETTINGS_FORM: &str = include_str!("../static/settings-form.js");
const SETTINGS_NAV: &str = include_str!("../static/settings-nav.js");
const MCP_TOOLS: &str = include_str!("../static/mcp-tools.js");
const SETTINGS_PIPELINE: &str = include_str!("../static/settings-pipeline.js");

/// The characters a header name may hold, as an HTML `pattern`, so the browser
/// refuses `Authorization: x` before the form posts. It matches the server's
/// `is_token_char`; the server still validates, so the pattern is only a hint.
///
/// A browser compiles `pattern` as a `RegExp` with the `v` flag, where `-` and
/// `|` are syntax characters inside a `[...]` class and must be escaped. An
/// invalid pattern is silently ignored, so an unescaped `|` or trailing `-`
/// here would turn off the check with no error.
const HEADER_NAME_PATTERN: &str = r"[A-Za-z0-9!#$%&'*+.^_`\|~\-]+";
/// Keeps the log scroll place and a half-typed form across a poll's swap. It
/// runs once on the job page, outside every swapped region, so a swap never
/// re-runs or removes it.
const JOB_UPDATES: &str = include_str!("../static/job-updates.js");
const JOB_SECTIONS: &str = include_str!("../static/job-sections.js");
const LOCAL_TIME: &str = include_str!("../static/local-time.js");
/// Reopens a `<details>` a poll swap would close, keyed by its `data-key`. It
/// runs once per page, outside every swapped region, and covers the board's
/// "Show N older" and the job page's MCP tool-call groups.
const DETAILS_STATE: &str = include_str!("../static/details-state.js");

/// Each live region polls its own fragment on this interval, so a state change
/// shows without a reload.
const POLL_TRIGGER: &str = "every 5s";

/// A board region that polls itself, paired with a version of its content. The
/// version is a hash of the region's inner markup, so it changes whenever the
/// rendered content changes — a worker's "2 min" counter, a usage window passing
/// its reset time — but not when a reader opens a `<details>`, which the server
/// always renders closed. The region's poll sends the version it holds, and the
/// handler answers `204 No Content` when it still matches, so htmx leaves the
/// region's DOM alone. See "Dashboard updates" in `docs/design.md`.
pub struct Region {
    pub version: String,
    pub markup: Markup,
}

impl maud::Render for Region {
    fn render_to(&self, buffer: &mut String) {
        buffer.push_str(&self.markup.0);
    }
}

/// A region's version: a short hash of its inner markup. It is computed from the
/// server's markup alone, so a `<details>` a reader opened does not change it,
/// and it is deterministic within the process, so the handler that renders a
/// region recomputes the same value a page earlier sent.
fn version_of(inner: &Markup) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    inner.0.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// The URL a region polls: its fragment route, the selected project, and the
/// version of the content on the page, so the handler can answer 204 when the
/// version still matches.
fn fragment_url(region: &str, project: &str, version: &str) -> String {
    format!(
        "/fragments/{region}?project={}&v={version}",
        crate::web::query_escape(project)
    )
}

/// The `hx-vals` that makes the plan usage poll send the version of the tile now
/// on the page. The `#usage` wrapper stays put and htmx swaps its contents, so
/// the version travels in a `data-version` on the swapped-in marker rather than
/// in the poll URL. htmx evaluates this before each poll.
const USAGE_VERSION_VALS: &str =
    "js:{v: document.querySelector('#usage [data-version]')?.dataset.version ?? ''}";

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
                        div.topbar-start {
                            a.brand href="/" { span.brand-mark aria-hidden="true" {} "factory" }
                            (nav)
                        }
                        div.topbar-end {
                            a.nav href="/onboard" { "Onboard" }
                            a.nav href="/settings" { "Settings" }
                            @if !user.is_empty() { span.user title=(user) { (user) } }
                        }
                    }
                }
                main { (body) }
                script { (PreEscaped(NOTIFICATIONS)) }
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
    project_names: &[ProjectLabel],
    attention: &[ProjectAttention],
    selected: Option<&str>,
    jobs: &[JobRow],
    costs: &HashMap<i64, f64>,
    waits: &HashMap<i64, Wait>,
    totals: ProjectTotals,
    plan_usage: Option<&UsageTile>,
    auto_merge: bool,
    project: Option<&Project>,
    workers: &Workers,
    notifications: bool,
    project_token_missing: bool,
) -> Markup {
    let all: Vec<&JobRow> = jobs
        .iter()
        .filter(|row| Some(row.project.as_str()) == selected)
        .collect();
    let nav = selected.map_or_else(
        || html! {},
        |selected| project_selector(project_names, attention, selected),
    );
    let links = Links::of(project);
    let problem = project.and_then(|project| project.forge.problem());
    page(
        user,
        "Jobs",
        &nav,
        &html! {
            @if notifications {
                (permission_bar())
            }
            @if let Some(selected) = selected {
                div.status-row.has-source[links.is_some()] {
                    (self::workers(workers, jobs, project_names, selected))
                    @if let Some(links) = links {
                        (source_link(links))
                    }
                    (plan_usage_region(plan_usage))
                }
                @if links.is_some() {
                    script { (PreEscaped(SOURCE_LINK)) }
                }
                @if let Some(problem) = &problem {
                    div.card.failed { (problem) }
                }
                @if project_token_missing {
                    (missing_project_token_notice(selected))
                }
                (composer(selected, shown_name(project_names, selected), &all))
                (board(&all, costs, waits, auto_merge, selected, project))
                (stats(totals, selected))
            } @else {
                div.card {
                    p.empty { "You are not a collaborator on any project." }
                    p { a.nav href="/onboard" { "Onboard a project" } }
                }
            }
            script { (PreEscaped(DETAILS_STATE)) }
            script { (PreEscaped(LOCAL_TIME)) }
        },
    )
}

fn permission_bar() -> Markup {
    html! {
        div.card #permission-bar hidden {
            "Get a browser notification when a job needs you "
            button.primary type="button" { "Allow" }
            " " a href="/settings/notifications" { "Notification settings" }
            script { (PreEscaped(PERMISSION_BAR)) }
        }
    }
}

/// The worker slots at render time: the limit from the settings, the slots
/// the scheduler filled, and the time to measure how long each job has held
/// its slot.
pub struct Workers {
    pub limit: usize,
    pub busy: Vec<Slot>,
    pub now: i64,
}

/// One tile per worker slot. A busy tile names the job, its stage and how
/// long it has held the slot. Slots are account-wide, so a job of another
/// project shows that project's name. A job in a project the viewer does not
/// collaborate on, named in `visible`, shows a plain busy tile with only the
/// time, so the count stays right without leaking the job's id, project,
/// stage or request. A job waiting for you holds no slot, so a full row of
/// busy tiles is why a new job has not started.
pub fn workers(
    workers: &Workers,
    jobs: &[JobRow],
    visible: &[ProjectLabel],
    project: &str,
) -> Region {
    let count = workers.limit.max(
        workers
            .busy
            .iter()
            .map(|slot| slot.number)
            .max()
            .unwrap_or(0),
    );
    let inner = html! {
        h2.visually-hidden { "Workers" }
        ul.worker-slots {
                @for number in 1..=count {
                    @let slot = workers.busy.iter().find(|slot| slot.number == number);
                    @let row = slot
                        .and_then(|slot| jobs.iter().find(|row| row.id == slot.job_id))
                        .filter(|row| visible.iter().any(|label| label.id == row.project));
                    @match (slot, row) {
                        (Some(slot), Some(row)) => {
                            @let shown = shown_name(visible, &row.project);
                            li.worker.busy {
                                span.worker-name { "Worker " (number) }
                                a.worker-job href={ "/jobs/" (row.id) } title=(job_title(&row.job)) {
                                    "#" (row.id)
                                    @if row.project != project { " " (shown) }
                                    " · " (stage_label(&row.job))
                                }
                                span.worker-time { (held_for(workers.now - slot.since)) }
                            }
                        }
                        (Some(slot), None) => {
                            li.worker.busy {
                                span.worker-name { "Worker " (number) }
                                span.worker-job { "busy" }
                                span.worker-time { (held_for(workers.now - slot.since)) }
                            }
                        }
                        (None, _) => {
                            li.worker.idle {
                                span.worker-name { "Worker " (number) }
                                span.worker-job { "idle" }
                            }
                        }
                    }
                }
            }
    };
    let version = version_of(&inner);
    let markup = html! {
        section.workers #workers hx-get=(fragment_url("workers", project, &version))
            hx-trigger=(POLL_TRIGGER) hx-swap="outerHTML" {
            (inner)
        }
    };
    Region { version, markup }
}

/// `seconds` as "under a minute", "12 min" or "2 h 5 min".
fn held_for(seconds: i64) -> String {
    let minutes = seconds.max(0) / 60;
    match minutes {
        0 => "under a minute".into(),
        1..60 => format!("{minutes} min"),
        _ => format!("{} h {} min", minutes / 60, minutes % 60),
    }
}

struct StartAfterOption {
    /// `after` posts a job number to add. `keep` posts the row of a current
    /// prerequisite, since a deleted job's number may belong to a new job.
    name: &'static str,
    value: i64,
    id: i64,
    title: String,
    checked: bool,
    /// The job's state, shown for a job that is no longer eligible.
    state: Option<String>,
}

impl StartAfterOption {
    fn candidate(row: &JobRow) -> Self {
        Self {
            name: "after",
            value: row.id,
            id: row.id,
            title: job_title(&row.job),
            checked: false,
            state: None,
        }
    }
}

/// The "Start after" picker. A checkbox per job posts a repeated field, so it
/// works without JavaScript. It renders nothing when there is nothing to list.
fn start_after(options: &[StartAfterOption]) -> Markup {
    html! {
        @if !options.is_empty() {
            fieldset.start-after {
                legend { "Start after" }
                span.hint { "The request waits until these jobs are merged." }
                @for option in options {
                    label.start-after-job {
                        input type="checkbox" name=(option.name) value=(option.value) checked[option.checked];
                        span.id { "#" (option.id) }
                        " " (option.title)
                        @if let Some(state) = &option.state {
                            " (" (state) ")"
                        }
                    }
                }
            }
        }
    }
}

/// The new-request form as a one-line bar. It grows, and shows the file
/// picker, once it has focus or text, so it takes little room on a board
/// that is opened more often to check on jobs than to file one.
fn composer(project: &str, shown: &str, rows: &[&JobRow]) -> Markup {
    let options: Vec<StartAfterOption> = rows
        .iter()
        .filter(|row| row.job.can_be_prerequisite())
        .map(|row| StartAfterOption::candidate(row))
        .collect();
    html! {
        form.composer method="post" action="/jobs" enctype="multipart/form-data" {
            input type="hidden" name="project" value=(project);
            label.visually-hidden for="request" { "New request for " (shown) }
            div.composer-line {
                textarea #request name="request" required rows="1"
                    placeholder={ "Describe a change for " (shown) "…" } {}
                div.composer-actions {
                    button.primary.request-button type="submit" name="mode" value="request" { "Request" }
                    (review_actions())
                }
            }
            div.composer-more tabindex="-1" {
                div.file-picker {
                    input #images type="file" name="images" multiple class="visually-hidden";
                    label.file-button for="images" { "Attach files" }
                    span.hint {
                        "Images (PNG, JPEG, GIF, WebP), text files or ZIP archives, up to 10 MiB \
                         each and 5 in total. You can also paste images into the box above."
                    }
                }
                ul #pasted-images.pasted-images {}
                (pr_picker(project))
                (start_after(&options))
            }
            script { (PreEscaped(PASTE_IMAGES)) }
            script { (PreEscaped(PR_PICKER)) }
        }
    }
}

fn review_actions() -> Markup {
    html! {
        div.review-actions hidden {
            div.split-button {
                button.primary.split-main type="submit" name="mode" value="quick-review"
                    formnovalidate { "Quick review" }
                button.split-toggle type="button" aria-haspopup="menu" aria-expanded="false"
                    aria-controls="review-menu" aria-label="More review actions" { "▾" }
                div #review-menu.split-menu role="menu" hidden {
                    button.split-item type="submit" name="mode" value="review-and-fix"
                        role="menuitem" formnovalidate { "Review and fix" }
                }
            }
            label.review-comment {
                input type="checkbox" name="comment" value="on";
                " Also comment on the pull request"
            }
        }
    }
}

/// The most rows the pull request picker lists.
pub const PR_ROWS: usize = 20;

/// A row of the pull request picker: the pull request and the live job that
/// may push to or merge it, if any.
pub struct PrChoice {
    pub pr: OpenPr,
    pub held_by: Option<i64>,
}

/// The pull request field of the new-request form. Without scripts it is a
/// text box for a pull request number. `pr-picker.js` turns it into a search
/// box with a result list, and posts the pick as `pr`.
fn pr_picker(project: &str) -> Markup {
    html! {
        div.pr-picker data-project=(project) {
            label.pr-label for="pr-search" { "Pull request" }
            input #pr-search type="text" name="pr" autocomplete="off" inputmode="numeric"
                placeholder="Number";
            ul #pr-results.pr-results role="listbox" aria-label="Open pull requests" hidden {}
            div.pr-picked hidden {}
        }
    }
}

/// The rows the picker lists for one search. `more` is set when the forge
/// had more matches than [`PR_ROWS`].
pub fn pr_results(choices: &[PrChoice], more: bool) -> Markup {
    html! {
        @for PrChoice { pr, held_by } in choices {
            li.pr-row role="option" #{ "pr-option-" (pr.number) } data-number=(pr.number)
                data-fork=[pr.fork.then_some("true")] data-held-job=[*held_by] {
                span.pr-main {
                    span.pr-number { "#" (pr.number) }
                    span.pr-title { (pr.title) }
                    @if pr.fork { span.badge.neutral { "fork" } }
                    @if pr.draft { span.badge.neutral { "draft" } }
                    @if let Some(job) = held_by { span.badge.factory { "job #" (job) } }
                }
                span.pr-meta { (pr.author) " · " (pr.head_branch) " → " (pr.base_branch) }
                template {
                    (external_link(
                        &pr.url,
                        html! { "#" (pr.number) " " (pr.title) },
                        Some("picked-pr"),
                        None,
                    ))
                    @if pr.fork {
                        span.pr-hint {
                            "The factory cannot push to a fork's branch, so it cannot change this \
                             pull request."
                        }
                    }
                    @if let Some(job) = held_by {
                        span.pr-hint {
                            "Job " a href={ "/jobs/" (job) } { "#" (job) } " may push to or merge \
                             this pull request."
                        }
                    }
                }
            }
        }
        @if choices.is_empty() {
            li.pr-note role="presentation" { "No pull request matches." }
        } @else if more {
            li.pr-note role="presentation" { "Refine the search to see more." }
        }
    }
}

/// A message row for a search the forge refused or the factory could not
/// make.
pub fn pr_results_error(message: &str) -> Markup {
    html! {
        li.pr-note.pr-error role="presentation" { "Could not search pull requests: " (message) }
    }
}

const NO_VALUE: &str = "–";

/// The project's totals as one line under the board. A figure that is zero
/// or would divide by zero shows a dash.
#[allow(clippy::cast_precision_loss, reason = "job counts stay far below 2^52")]
pub fn stats(totals: ProjectTotals, project: &str) -> Region {
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
    let inner = html! {
        @for (label, value) in figures {
            div.total {
                dt { (label) }
                dd { (value) }
            }
        }
    };
    let version = version_of(&inner);
    let markup = html! {
        dl.totals #stats hx-get=(fragment_url("stats", project, &version))
            hx-trigger=(POLL_TRIGGER) hx-swap="outerHTML" {
            (inner)
        }
    };
    Region { version, markup }
}

/// What the plan usage tile renders: the last snapshot and the time the page
/// renders at, a Unix timestamp in seconds, so a window whose reset time has
/// passed reads as reset.
pub struct UsageTile {
    pub snapshot: PlanUsageSnapshot,
    pub now: i64,
}

/// The plan usage tile sits in a wrapper that polls on its own interval, so
/// the tile appears once a run reports usage and its numbers follow later
/// reports. htmx swaps the wrapper's contents, so an empty response leaves no
/// tile while the wrapper keeps polling.
fn plan_usage_region(usage: Option<&UsageTile>) -> Markup {
    html! {
        div #usage hx-get="/fragments/plan-usage" hx-trigger=(POLL_TRIGGER)
            hx-vals=(USAGE_VERSION_VALS) {
            (plan_usage(usage))
        }
    }
}

/// The plan usage tile, or nothing when the handler passes `None`, with a hidden
/// marker that carries the content's version. The `#usage` wrapper swaps its
/// contents in place, so the marker is where the next poll reads the version
/// from, see `USAGE_VERSION_VALS`.
pub fn plan_usage(usage: Option<&UsageTile>) -> Region {
    let inner = html! {
        @if let Some(usage) = usage {
            (plan_usage_panel(usage))
        }
    };
    let version = version_of(&inner);
    let markup = html! {
        (inner)
        span.usage-version.visually-hidden data-version=(version) {}
    };
    Region { version, markup }
}

/// The account-wide panel of Claude subscription limits: the 5-hour window,
/// the weekly window and the Fable window, each when a run has reported it,
/// and the time of the last report. It reads the same for every project,
/// because the limits belong to the account.
fn plan_usage_panel(tile: &UsageTile) -> Markup {
    let usage = &tile.snapshot.usage;
    let windows = [
        ("5-hour", &usage.five_hour),
        ("Weekly", &usage.seven_day),
        ("Fable", &usage.fable),
    ];
    html! {
        section.plan-usage {
            h2 {
                "Claude plan usage "
                span.usage-as-of { "as of " (crate::localtime::unix(tile.snapshot.reported_at)) }
            }
            ul.usage-windows {
                @for (label, window) in windows {
                    @if let Some(window) = window {
                        (usage_window(label, window, tile.now))
                    }
                }
            }
        }
    }
}

/// One window's bar, percent and reset time. After the reset time passes the
/// last report describes the old window, so the bar is empty and the text says
/// the window reset with no report since.
fn usage_window(label: &str, window: &UsageWindow, now: i64) -> Markup {
    let reset = window.has_reset(now);
    let percent = if reset { 0.0 } else { window.percent_used() };
    let fill = format!("{:.1}%", percent.clamp(0.0, 100.0));
    html! {
        li.usage-window {
            span.usage-name { (label) }
            span.usage-track { span.usage-fill style={ "width: " (fill) } {} }
            @if reset {
                span.usage-percent { "no report since the reset" }
                span.usage-reset { "reset " (crate::localtime::unix(window.resets_at)) }
            } @else {
                span.usage-percent { (format!("{percent:.0}% used")) }
                span.usage-reset { "resets " (crate::localtime::unix(window.resets_at)) }
            }
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

/// The name to show for the project `id`, which is the identifier itself when
/// `labels` does not list it.
fn shown_name<'a>(labels: &'a [ProjectLabel], id: &'a str) -> &'a str {
    labels
        .iter()
        .find(|label| label.id == id)
        .map_or(id, |label| label.shown.as_str())
}

/// What a project asks of the reader, counted with the board's own lane
/// function so a picker count equals the board's section count.
pub struct ProjectAttention {
    pub label: ProjectLabel,
    /// The jobs in the project's Needs you section.
    pub needs_you: usize,
    /// The jobs in the project's Failed section.
    pub failed: usize,
    /// Whether any job is in the Agent or Factory lane.
    pub running: bool,
    /// The login's group the project is in: the group's place in the
    /// login's group order and its name.
    pub group: Option<(usize, String)>,
}

impl ProjectAttention {
    /// The group the picker sorts by, and the value of the row's
    /// `data-attention`.
    fn kind(&self) -> &'static str {
        if self.needs_you > 0 {
            "you"
        } else if self.failed > 0 {
            "failed"
        } else if self.running {
            "running"
        } else {
            "idle"
        }
    }
}

/// The picker's entries: one per project in `labels`, which the caller limits
/// to the projects the login collaborates on, so a job of any other project
/// adds to no count. `labels` come in shown-name order and the sort is stable,
/// so each group keeps that order. A project missing from `auto_merge` counts
/// its reviews as needing you. `groups` are the login's project groups in
/// their order.
pub fn project_attention(
    labels: &[ProjectLabel],
    jobs: &[JobRow],
    waits: &HashMap<i64, Wait>,
    auto_merge: &HashMap<String, bool>,
    groups: &[ProjectGroup],
) -> Vec<ProjectAttention> {
    let mut entries: Vec<ProjectAttention> = labels
        .iter()
        .map(|label| ProjectAttention {
            label: label.clone(),
            needs_you: 0,
            failed: 0,
            running: false,
            group: groups
                .iter()
                .enumerate()
                .find(|(_, group)| group.projects.contains(&label.id))
                .map(|(at, group)| (at, group.name.clone())),
        })
        .collect();
    let index: HashMap<&str, usize> = labels
        .iter()
        .enumerate()
        .map(|(at, label)| (label.id.as_str(), at))
        .collect();
    for row in jobs {
        let Some(&at) = index.get(row.project.as_str()) else {
            continue;
        };
        let merge = auto_merge.get(&row.project).copied().unwrap_or(false);
        let entry = &mut entries[at];
        match status(&row.job, merge, wait_of(waits, row.id)).lane {
            Lane::You => entry.needs_you += 1,
            Lane::Failed => entry.failed += 1,
            Lane::Agent | Lane::Factory => entry.running = true,
            Lane::Done => {}
        }
    }
    entries.sort_by_key(|entry| match entry.kind() {
        "you" => 0,
        "failed" => 1,
        "running" => 2,
        _ => 3,
    });
    entries
}

fn project_selector(
    labels: &[ProjectLabel],
    attention: &[ProjectAttention],
    selected: &str,
) -> Markup {
    html! {
        details.project-picker #project-picker {
            summary role="button" aria-label="Project" aria-expanded="false"
                aria-keyshortcuts="p" {
                span.picker-current { (shown_name(labels, selected)) }
                kbd.picker-key aria-hidden="true" hidden { "P" }
                span.picker-chevron aria-hidden="true" {}
            }
            div.picker-panel {
                input.picker-search #picker-search type="search" hidden
                    aria-label="Find a project" autocomplete="off"
                    spellcheck="false" placeholder="Find a project…";
                p.picker-status.visually-hidden role="status" {}
                (picker(attention, selected, false))
                p.picker-empty hidden { "No project matches." }
                div.picker-foot {
                    div.picker-keys hidden {
                        span.picker-hint { kbd { "J" } " " kbd { "K" } " move" }
                        span.picker-hint { kbd { "I" } " search" }
                        span.picker-hint.picker-hint-n { kbd { "N" } " " kbd { "⇧N" } " needs you" }
                    }
                    a.picker-groups href="/settings/groups" { "Edit groups" }
                }
            }
            script { (PreEscaped(PROJECT_PICKER)) }
        }
        (picker_badge(attention, selected, false))
    }
}

fn other_projects(count: usize, what: &str, projects: usize) -> String {
    let s = if projects == 1 { "" } else { "s" };
    format!("{count} {what} in {projects} other project{s}")
}

/// With nothing to show the badge stays an empty, hidden span, so the poll's
/// out-of-band swap has a target.
fn picker_badge(attention: &[ProjectAttention], selected: &str, oob: bool) -> Markup {
    let others = || attention.iter().filter(|entry| entry.label.id != selected);
    let you = others().map(|entry| entry.needs_you).sum::<usize>();
    let you_projects = others().filter(|entry| entry.needs_you > 0).count();
    let failed = others().map(|entry| entry.failed).sum::<usize>();
    let failed_projects = others().filter(|entry| entry.failed > 0).count();
    let mut parts = Vec::new();
    if you > 0 {
        parts.push(other_projects(you, "need you", you_projects));
    }
    if failed > 0 {
        parts.push(other_projects(failed, "failed", failed_projects));
    }
    let oob = oob.then_some("true");
    if parts.is_empty() {
        return html! { span #picker-badge hidden hx-swap-oob=[oob] {} };
    }
    let label = parts.join(", ");
    html! {
        span #picker-badge role="img" aria-label=(label) title=(label) hx-swap-oob=[oob] {
            @if you > 0 { span.badge.you { (you) } }
            @if failed > 0 { span.badge.bad { (failed) } }
        }
    }
}

/// A run of picker rows under one heading. The flat list's last section,
/// the projects with nothing to report, has no heading.
struct PickerSection<'a> {
    heading: Option<&'a str>,
    entries: Vec<&'a ProjectAttention>,
}

/// The picker's sections. With no project in one of the login's groups the
/// list splits by what each project asks of the reader, in the order
/// `project_attention` sorts. Otherwise each group is a section in the
/// login's group order and the projects in no group close the list under
/// "Other". Either way a section keeps the attention order inside it, and
/// an empty section is left out.
fn picker_sections(attention: &[ProjectAttention]) -> Vec<PickerSection<'_>> {
    if attention.iter().any(|entry| entry.group.is_some()) {
        let mut groups: Vec<(usize, &str)> = attention
            .iter()
            .filter_map(|entry| entry.group.as_ref())
            .map(|(rank, name)| (*rank, name.as_str()))
            .collect();
        groups.sort_unstable();
        groups.dedup();
        let mut sections: Vec<PickerSection> = groups
            .into_iter()
            .map(|(rank, name)| PickerSection {
                heading: Some(name),
                entries: attention
                    .iter()
                    .filter(|entry| entry.group.as_ref().is_some_and(|(at, _)| *at == rank))
                    .collect(),
            })
            .collect();
        let rest: Vec<&ProjectAttention> = attention
            .iter()
            .filter(|entry| entry.group.is_none())
            .collect();
        if !rest.is_empty() {
            sections.push(PickerSection {
                heading: Some("Other"),
                entries: rest,
            });
        }
        return sections;
    }
    [
        ("you", Some("Needs you")),
        ("failed", Some("Failed")),
        ("running", Some("Running")),
        ("idle", None),
    ]
    .into_iter()
    .map(|(kind, heading)| PickerSection {
        heading,
        entries: attention
            .iter()
            .filter(|entry| entry.kind() == kind)
            .collect(),
    })
    .filter(|section| !section.entries.is_empty())
    .collect()
}

fn picker_check() -> Markup {
    html! {
        svg viewBox="0 0 16 16" width="14" height="14" fill="none" stroke="currentColor"
            stroke-width="2" stroke-linecap="round" stroke-linejoin="round" {
            path d="M3.5 8.5l3 3 6-7" {}
        }
    }
}

/// The picker's project list. A poll's copy (`poll`) carries the top bar badge
/// as an out-of-band swap, and the page's copy leaves it to the top bar. An
/// empty `selected` is a recovery poll: it marks no row and counts every project.
/// Only the list polls, so the `<details>` around it keeps its open state.
pub fn picker(attention: &[ProjectAttention], selected: &str, poll: bool) -> Region {
    let inner = html! {
        @for (at, section) in picker_sections(attention).iter().enumerate() {
            li.picker-section {
                @if let Some(heading) = section.heading {
                    div.picker-heading id={ "picker-sec-" (at) } { (heading) }
                }
                ul aria-labelledby=[section.heading.map(|_| format!("picker-sec-{at}"))] {
                    @for entry in &section.entries {
                        (picker_row(entry, selected))
                    }
                }
            }
        }
    };
    let version = version_of(&html! { (inner) (picker_badge(attention, selected, false)) });
    let url = if selected.is_empty() {
        format!("/fragments/picker?v={version}")
    } else {
        fragment_url("picker", selected, &version)
    };
    let markup = html! {
        ul #picker-list hx-get=(url)
            hx-trigger=(POLL_TRIGGER) hx-swap="outerHTML" {
            (inner)
        }
        @if poll {
            (picker_badge(attention, selected, true))
        }
    };
    Region { version, markup }
}

fn picker_row(entry: &ProjectAttention, selected: &str) -> Markup {
    let label = &entry.label;
    let current = label.id == selected;
    html! {
        li id={ "picker-opt-" (label.id) } data-attention=(entry.kind()) data-id=(label.id) data-name=(label.shown) {
            a href={ "/?project=" (crate::web::query_escape(&label.id)) }
                aria-current=[current.then_some("page")] {
                span.picker-mark aria-hidden="true" {
                    @if current { (picker_check()) }
                }
                span.picker-name title=(label.shown) { (label.shown) }
                @if entry.needs_you > 0 {
                    span.badge.you {
                        (entry.needs_you)
                        span.visually-hidden { " need you" }
                    }
                }
                @if entry.failed > 0 {
                    span.badge.bad {
                        (entry.failed)
                        span.visually-hidden { " failed" }
                    }
                }
                @if entry.kind() == "running" {
                    span.picker-running { span.visually-hidden { "running" } }
                }
            }
        }
    }
}

/// A project's block on the settings page: its models, efforts and flags, and
/// its per-server MCP settings.
pub struct ProjectSettings {
    /// The identifier, which every form action and link uses.
    pub name: String,
    /// The name the page shows.
    pub shown: String,
    pub models: StageModels,
    pub efforts: StageEfforts,
    /// Every registered agent, in the registry's order, for the stage panel's
    /// agent control. Claude is selectable; Codex is shown disabled.
    pub agents: Vec<AgentChoice>,
    /// The agent each stage runs on, preselected in the control. A project with
    /// no saved agents reads the default for every stage.
    pub agent_selection: StageAgents,
    pub auto_merge: bool,
    pub llm_review: bool,
    pub gates: Gates,
    /// The factory's default gates, which the gate panels compare against.
    pub default_gates: Gates,
    /// One per registered server, in the registry's order.
    pub servers: Vec<ServerSettings>,
    /// The project's collaborators, oldest first, for the Collaborators card.
    pub collaborators: Vec<Collaborator>,
    /// The Renovate and packaging switches and the packaging fields.
    pub maintenance: Maintenance,
    /// Set when the project's forge is not one the factory supports.
    pub forge_problem: Option<String>,
    /// Set when the stored upstream forge is neither a fixed forge nor a
    /// registered Forgejo instance. The card then reads as GitHub, and a save
    /// picks the forge again.
    pub upstream_problem: Option<String>,
    /// The Forge account card. `None` when the project's forge is unknown.
    pub forge_account: Option<ForgeAccountCard>,
    /// The packaging upstream's token and the last connection check.
    pub upstream: UpstreamCard,
    /// The project's local checks command, or `None` when it has none.
    pub checks: Option<String>,
    /// The project's worker packages.
    pub packages: Vec<String>,
    /// The form token the Local checks and Worker packages cards post.
    pub checks_csrf: String,
    /// The Renovate agent override, or `None` when Renovate runs use the review
    /// stage's agent, model and effort.
    pub renovate_agent: Option<RenovateAgent>,
}

const CHECKS_LABEL: &str = "Local checks command (optional)";
const CHECKS_HINT: &str = "The factory runs this with bash in the worker, in the job's checkout, before it pushes, such as nix develop -c just lint && nix develop -c just test. It is separate from GitHub's CI on the pull request. Leave it empty to push without local checks.";

/// The result of reading the packaging upstream the way a job would.
pub struct UpstreamCheckView {
    /// Whether the repository was readable, or `None` when the check could
    /// not run.
    pub readable: Option<bool>,
    pub text: String,
}

/// What the packaging upstream's token card and connection check show.
pub struct UpstreamCard {
    /// The account the upstream token of each forge belongs to, or `None`
    /// with no token.
    pub accounts: Vec<(ForgeKind, Option<ForgeAccount>)>,
    pub secrets_available: bool,
    pub check: Option<UpstreamCheckView>,
}

/// What the Forge account card shows: the project token state and the
/// current login's own switch.
pub struct ForgeAccountCard {
    pub kind: ForgeKind,
    pub repo: String,
    /// The account the project token belongs to, or `None` with no token.
    pub project_account: Option<ForgeAccount>,
    pub secrets_available: bool,
    pub csrf: String,
    /// Whether the current login has "Use my account" on.
    pub use_my_account: bool,
    /// Whether the current login has a token for the project's forge.
    pub has_user_token: bool,
}

/// One agent the stage panel's agent control offers: its stable id, the name
/// the control shows, whether it can be selected, and the effort levels and
/// default it offers. An agent that cannot run, such as Codex, is shown disabled
/// and marked "Not available yet". The effort levels and default let the panel's
/// script redraw the effort radios when the agent switches.
pub struct AgentChoice {
    pub id: String,
    pub display_name: String,
    pub available: bool,
    /// The agent's effort levels, lowest first.
    pub efforts: Vec<String>,
    /// The level the panel preselects for this agent when the saved level is not
    /// one of its own.
    pub default_effort: String,
    pub models: Vec<String>,
    /// The hint under the model field.
    pub model_hint: String,
    /// Whether a stage on this agent needs a model, so the panel offers no
    /// "CLI default" chip.
    pub model_required: bool,
}

/// One server's block inside a project's form: whether the project runs it, the
/// stages it runs in, and the state of each header the server declares.
pub struct ServerSettings {
    pub name: String,
    pub enabled: bool,
    pub stages: McpStages,
    pub headers: Vec<HeaderState>,
    /// The tools the last refresh stored, each with its on or off state.
    pub tools: Vec<McpToolState>,
    /// The command line parts when the server is stdio, `None` when it is remote.
    pub stdio: Option<StdioSettings>,
}

/// A stdio server as a project's page shows it.
pub struct StdioSettings {
    pub command: String,
    /// The registry's base arguments.
    pub args: Vec<String>,
    /// The arguments this project appends after the base ones.
    pub extra_args: Vec<String>,
}

/// One declared header in a project's server block. `set` is whether a value is
/// stored; the value itself is never rendered.
pub struct HeaderState {
    pub name: String,
    pub set: bool,
}

/// Which settings page is open, so the sidebar marks its entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingsPage {
    Notifications,
    /// The login's own project groups.
    ProjectGroups,
    /// The login's own Local API key.
    LocalKey,
    /// A forge's page for the login's own token.
    Forge(ForgeKind),
    Workers,
    Gates,
    Polling,
    Renovate,
    Forgejo,
    /// An agent's account page, named by the agent's id.
    Agent(String),
    Mcp,
    McpServer(String),
    Project(String),
}

/// One agent in the sidebar's Agents group, as the registry reports it: the id
/// the open page marks itself with, the display name and the account page URL (none
/// for an agent with no page, which the sidebar lists as "Not available yet").
pub struct AgentNav {
    pub id: String,
    pub name: String,
    pub url: Option<String>,
}

/// What the settings sidebar lists besides its fixed entries.
pub struct SettingsNav {
    /// Whether the login is a factory admin, who sees the factory-wide groups.
    pub admin: bool,
    /// Onboarded project names, in name order.
    pub projects: Vec<ProjectLabel>,
    /// Registered MCP server names, in the registry's order.
    pub servers: Vec<String>,
    /// The forges the sidebar lists, in the order the pages show them.
    pub forges: Vec<ForgeKind>,
    /// The registered agents, in the registry's order.
    pub agents: Vec<AgentNav>,
}

/// A settings page: the sidebar, then `body` under its heading and the
/// optional `notice`. `SETTINGS_FORM` marks a form with unsaved changes and
/// asks before the page is left.
fn settings_page(
    user: &str,
    nav: &SettingsNav,
    current: &SettingsPage,
    title: &str,
    notice: Option<&str>,
    body: &Markup,
) -> Markup {
    layout(
        user,
        &format!("{title} · Settings"),
        &html! {
            div.settings {
                (settings_sidebar(nav, current))
                div.settings-main {
                    @if let Some(notice) = notice {
                        div.card.saved { (notice) }
                    }
                    (body)
                }
            }
            script { (PreEscaped(SETTINGS_NAV)) }
            script { (PreEscaped(SETTINGS_FORM)) }
        },
    )
}

fn settings_sidebar(nav: &SettingsNav, current: &SettingsPage) -> Markup {
    let entry = |href: String, label: &str, page: &SettingsPage| {
        let here = page == current;
        html! {
            a.settings-link href=(href) aria-current=[here.then_some("page")] { (label) }
        }
    };
    let in_mcp = matches!(current, SettingsPage::Mcp | SettingsPage::McpServer(_));
    html! {
        nav.settings-nav aria-label="Settings" {
            p.settings-group { "You" }
            (entry("/settings/notifications".into(), "Notifications", &SettingsPage::Notifications))
            (entry("/settings/groups".into(), "Project groups", &SettingsPage::ProjectGroups))
            (entry("/settings/local".into(), "Local API key", &SettingsPage::LocalKey))
            p.settings-group { "Forges" }
            @for kind in &nav.forges {
                (entry(
                    format!("/settings/forges/{}", kind.as_str()),
                    kind.display_name(),
                    &SettingsPage::Forge(kind.clone()),
                ))
            }
            @if nav.admin {
                p.settings-group { "Factory" }
                (entry("/settings/workers".into(), "Workers", &SettingsPage::Workers))
                (entry("/settings/gates".into(), "Default gates", &SettingsPage::Gates))
                (entry("/settings/polling".into(), "Pull request polling", &SettingsPage::Polling))
                (entry("/settings/renovate".into(), "Renovate", &SettingsPage::Renovate))
                (entry("/settings/forgejo".into(), "Forgejo servers", &SettingsPage::Forgejo))
                p.settings-group { "Agents" }
                @for agent in &nav.agents {
                    @match &agent.url {
                        Some(url) => (entry(url.clone(), &agent.name, &SettingsPage::Agent(agent.id.clone()))),
                        None => span.settings-link.settings-disabled {
                            (agent.name)
                            span.settings-note { "Not available yet" }
                        },
                    }
                }
                p.settings-group { "Integrations" }
                a.settings-link href="/settings/mcp" aria-current=[in_mcp.then_some("page")] {
                    "MCP servers"
                    span.settings-count { (nav.servers.len()) }
                }
            }
            p.settings-group { "Projects" }
            @for label in &nav.projects {
                (entry(
                    format!("/settings/projects/{}", crate::web::query_escape(&label.id)),
                    &label.shown,
                    &SettingsPage::Project(label.id.clone()),
                ))
            }
            a.settings-link.settings-add href="/onboard" { "Onboard a project" }
        }
    }
}

/// The bar at the bottom of a settings form with its submit button. It stays in
/// view as the page scrolls. `SETTINGS_FORM` shows "Unsaved changes" in it once
/// an input of the form changes.
fn save_bar(label: &str) -> Markup {
    html! {
        div.save-bar {
            span.save-bar-note { "No unsaved changes." }
            button.primary { (label) }
        }
    }
}

const RUNS_KEEP_VALUES: &str = "The job loop reads these on every pass. A run that already started keeps the values it started with.";

/// The per-user Notifications page: one checkbox for the login's browser
/// notifications. `NOTIFICATION_PERMISSION` fills in the browser's permission
/// state; without JavaScript the line stays hidden and the checkbox still saves.
pub fn settings_notifications(
    user: &str,
    nav: &SettingsNav,
    enabled: bool,
    notice: Option<&str>,
) -> Markup {
    settings_page(
        user,
        nav,
        &SettingsPage::Notifications,
        "Notifications",
        notice,
        &html! {
            h1 { "Notifications" }
            p.meta { "The dashboard notifies you while a factory tab is open in this browser. The setting follows your login to every browser." }
            form.card.settings-form method="post" action="/settings/notifications" {
                div.field {
                    label {
                        input type="checkbox" name="notifications" value="on" checked[enabled];
                        " Browser notifications for my jobs"
                    }
                    p.hint { "A notification arrives when a job you requested needs you or fails." }
                }
                (save_bar("Save"))
            }
            div.card #notification-permission hidden {
                p.permission-state {}
                button.primary type="button" hidden { "Allow notifications" }
            }
            script { (PreEscaped(NOTIFICATION_PERMISSION)) }
        },
    )
}

/// The per-user Project groups page. The first form names the login's
/// groups: a field per group, where an empty name removes the group, and one
/// field that adds a group. The second form puts each of the login's projects
/// in one group or none, and shows once a group exists.
pub fn settings_groups(
    user: &str,
    nav: &SettingsNav,
    groups: &[ProjectGroup],
    csrf: &str,
    notice: Option<&str>,
) -> Markup {
    let group_of = |project: &str| {
        groups
            .iter()
            .find(|group| group.projects.iter().any(|id| id == project))
            .map(|group| group.id)
    };
    settings_page(
        user,
        nav,
        &SettingsPage::ProjectGroups,
        "Project groups",
        notice,
        &html! {
            h1 { "Project groups" }
            p.meta {
                "Groups split the project picker into one section per group, with the projects in no group under Other. "
                "With no project in a group, the picker sorts projects by what needs you. "
                "Your groups follow your login to every browser."
            }
            form.card.settings-form #group-names method="post" action="/settings/groups" {
                input type="hidden" name="csrf" value=(csrf);
                h2 { "Groups" }
                @if !groups.is_empty() {
                    ul.group-names {
                        @for group in groups {
                            li {
                                input.wide type="text" name={ "group-" (group.id) } value=(group.name)
                                    maxlength=(MAX_GROUP_NAME) aria-label={ "Name of " (group.name) }
                                    autocomplete="off";
                            }
                        }
                    }
                    p.hint { "Clear a name to remove the group. Its projects move to Other." }
                }
                div.field.group-add {
                    label for="group-new" { "Add a group" }
                    input #group-new.wide type="text" name="new" maxlength=(MAX_GROUP_NAME)
                        autocomplete="off" placeholder="Name, such as Home or Work";
                }
                (save_bar("Save groups"))
            }
            @if !groups.is_empty() {
                form.card.settings-form #group-members method="post" action="/settings/groups/projects" {
                    input type="hidden" name="csrf" value=(csrf);
                    h2 { "Projects" }
                    @if nav.projects.is_empty() {
                        p.empty { "You are not a collaborator on any project." }
                    }
                    ul.group-members {
                        @for (at, label) in nav.projects.iter().enumerate() {
                            @let current = group_of(&label.id);
                            li {
                                label for={ "group-of-" (at) } title=(label.shown) { (label.shown) }
                                select id={ "group-of-" (at) } name={ "project:" (label.id) } {
                                    option value="" selected[current.is_none()] { "No group" }
                                    @for group in groups {
                                        option value=(group.id) selected[current == Some(group.id)] { (group.name) }
                                    }
                                }
                            }
                        }
                    }
                    (save_bar("Save"))
                }
            }
        },
    )
}

pub use crate::store::MAX_GROUP_NAME;

pub fn settings_workers(
    user: &str,
    nav: &SettingsNav,
    settings: &Settings,
    notice: Option<&str>,
) -> Markup {
    settings_page(
        user,
        nav,
        &SettingsPage::Workers,
        "Workers",
        notice,
        &html! {
            h1 { "Workers" }
            p.meta { (RUNS_KEEP_VALUES) }
            form.card.settings-form method="post" action="/settings/workers" {
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
                (save_bar("Save"))
            }
        },
    )
}

pub fn settings_gates(
    user: &str,
    nav: &SettingsNav,
    settings: &Settings,
    notice: Option<&str>,
) -> Markup {
    settings_page(
        user,
        nav,
        &SettingsPage::Gates,
        "Default gates",
        notice,
        &html! {
            h1 { "Default gates" }
            p.meta { "A project you onboard starts with these. Each project then has its own gates on its own page. Changing a default leaves existing projects alone." }
            form.card.settings-form method="post" action="/settings/gates" {
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
                }
                (save_bar("Save"))
            }
        },
    )
}

pub fn settings_polling(
    user: &str,
    nav: &SettingsNav,
    settings: &Settings,
    notice: Option<&str>,
) -> Markup {
    settings_page(
        user,
        nav,
        &SettingsPage::Polling,
        "Pull request polling",
        notice,
        &html! {
            h1 { "Pull request polling" }
            p.meta { (RUNS_KEEP_VALUES) }
            form.card.settings-form method="post" action="/settings/polling" {
                div.field {
                    label for="poll_seconds" { "Seconds between looks at a pull request" }
                    input #poll_seconds type="number" name="poll_seconds" min="10" required
                        value=(settings.poll_seconds);
                    p.hint { "Also the wait before retrying a failed push or forge call." }
                }
                (save_bar("Save"))
            }
        },
    )
}

pub fn settings_renovate(
    user: &str,
    nav: &SettingsNav,
    settings: &Settings,
    notice: Option<&str>,
    problem: Option<&str>,
) -> Markup {
    settings_page(
        user,
        nav,
        &SettingsPage::Renovate,
        "Renovate",
        notice,
        &html! {
            h1 { "Renovate" }
            p.meta { "Limits for Renovate pull requests, shared by every project. A change applies on the next look at a pull request." }
            @if let Some(problem) = problem {
                div.card.failed { (problem) }
            }
            form.card.settings-form method="post" action="/settings/renovate" {
                div.field {
                    label for="auto_reruns" { "Automatic reruns" }
                    input #auto_reruns type="number" name="auto_reruns" min="0" step="1" required
                        value=(settings.renovate_auto_reruns);
                    p.hint { "Reruns of failed CI on one head commit before an agent looks at the failure. 0 sends the first failure to the agent." }
                }
                div.field {
                    label for="total_reruns" { "Total reruns" }
                    input #total_reruns type="number" name="total_reruns" min="1" step="1" required
                        value=(settings.renovate_total_reruns);
                    p.hint { "Reruns one head commit gets before the factory comments and stops. Not less than the automatic reruns." }
                }
                (save_bar("Save"))
            }
        },
    )
}

/// What the Local agent page shows: the stored settings and model list, whether
/// a factory key is stored, and the form values a refused save submitted.
pub struct LocalAgentView<'a> {
    pub settings: &'a crate::LocalSettings,
    pub models: &'a [String],
    pub key_stored: bool,
    pub secrets_available: bool,
    /// The URL field's value: the stored URL, or what a refused save submitted.
    pub url: &'a str,
    pub cli: factory_core::Cli,
    pub load_timeout: &'a str,
    pub csrf: &'a str,
}

pub fn settings_local_agent(
    user: &str,
    nav: &SettingsNav,
    view: &LocalAgentView<'_>,
    notice: Option<&str>,
) -> Markup {
    let csrf = view.csrf;
    settings_page(
        user,
        nav,
        &SettingsPage::Agent("local".to_owned()),
        "Local",
        notice,
        &html! {
            h1 { "Local" }
            p.meta {
                "Runs a stage on a model served by an OpenAI-compatible server, such as a llama.cpp \
                 server. One CLI drives every Local run. A change applies from the next run."
            }
            form.card.settings-form method="post" action="/settings/agents/local" {
                input type="hidden" name="csrf" value=(csrf);
                div.field {
                    label for="local-url" { "Server URL" }
                    input #local-url.wide type="text" name="url" spellcheck="false"
                        placeholder="http://host:port" value=(view.url);
                    p.hint { "An http or https address with an optional port. A trailing slash or /v1 is dropped." }
                }
                div.field {
                    label for="local-key" { "Factory API key (optional)" }
                    input #local-key.wide type="password" name="key"
                        autocomplete="off" spellcheck="false"
                        placeholder=(if view.key_stored { "set" } else { "not set" })
                        disabled[!view.secrets_available];
                    p.hint {
                        @if view.key_stored { "A key is stored. " } @else { "No key is stored. " }
                        "Type a new key to replace it. An empty field keeps it. Stored encrypted and never shown. \
                         A job whose requester stored a key of their own uses that one. "
                        "The key reaches the worker, and the agent that runs there can read it."
                    }
                    @if !view.secrets_available {
                        p.hint.bad { "The master key is missing, so a key cannot be saved here. Remove still works." }
                    }
                }
                div.field {
                    label for="local-cli" { "CLI" }
                    select #local-cli name="cli" {
                        @for cli in factory_core::Cli::ALL {
                            option value=(cli.id()) selected[cli == view.cli] { (cli.display_name()) }
                        }
                    }
                    p.hint { "The CLI that drives the model in the worker. A session keeps the CLI it started on." }
                }
                div.field {
                    label for="local-timeout" { "Model load timeout (minutes)" }
                    input #local-timeout type="number" name="load_timeout" min="1"
                        max=(crate::store::MAX_LOCAL_LOAD_MINUTES) step="1" required
                        value=(view.load_timeout);
                    p.hint { "How long a run waits for the server to load the model, 1 to 60 minutes." }
                }
                div.save-bar {
                    span.save-bar-note { "No unsaved changes." }
                    button.primary { "Save" }
                }
                div.danger-zone {
                    button.danger formaction="/settings/agents/local/clear-key" formnovalidate { "Remove key" }
                }
            }
            div.card #local-models {
                h2 { "Models" }
                @if view.models.is_empty() {
                    p.meta { "No models are stored." }
                } @else {
                    ul.local-models {
                        @for id in view.models { li { code { (id) } } }
                    }
                }
                @if let Some(at) = view.settings.models_fetched_at {
                    p.meta { "Fetched " (crate::localtime::unix(at)) "." }
                }
                form method="post" action="/settings/agents/local/refresh" {
                    input type="hidden" name="csrf" value=(csrf);
                    button { "Refresh models" }
                }
            }
        },
    )
}

pub fn settings_claude(
    user: &str,
    nav: &SettingsNav,
    claude_token_set: bool,
    secrets_available: bool,
    notice: Option<&str>,
) -> Markup {
    settings_page(
        user,
        nav,
        &SettingsPage::Agent("claude".to_owned()),
        "Claude",
        notice,
        &html! {
            h1 { "Claude" }
            (claude_token_card(claude_token_set, secrets_available))
        },
    )
}

/// Whether a Codex login is stored and, when it is, the time Codex last
/// refreshed it, as a `YYYY-MM-DD HH:MM:SS` string for the dashboard time
/// helper. The token itself never leaves the handler.
pub struct CodexAccount {
    pub stored: bool,
    pub last_refresh: Option<String>,
}

pub fn settings_codex(
    user: &str,
    nav: &SettingsNav,
    account: &CodexAccount,
    login: &crate::device_login::LoginView,
    prices: &[CodexPrice],
    secrets_available: bool,
    notice: Option<&str>,
) -> Markup {
    settings_page(
        user,
        nav,
        &SettingsPage::Agent("codex".to_owned()),
        "Codex",
        notice,
        &html! {
            h1 { "Codex" }
            (codex_account_card(account))
            (codex_login_card(login, secrets_available))
            (codex_auth_card(account.stored, secrets_available))
            (codex_prices_card(prices))
        },
    )
}

/// The Codex price table: one row per model with its USD-per-million-token input,
/// cached input and output prices, each editable, plus a form to add a model.
/// The factory estimates a Codex run's cost from the row for the model it ran,
/// so a price change here needs no code change or deploy. The table starts empty,
/// and a run on a model with no row adds no cost.
fn codex_prices_card(prices: &[CodexPrice]) -> Markup {
    html! {
        div.card {
            h2 { "Model prices" }
            p.hint {
                "Prices in USD per million tokens. The factory estimates a Codex run's cost from \
                 the row for the model it ran, since Codex reports token counts and no dollar \
                 figure. A change applies to runs after it, with no restart. A run on a model with \
                 no row here adds no cost."
            }
            @if prices.is_empty() {
                p.meta { "No model prices are set." }
            } @else {
                div.codex-prices {
                    @for price in prices {
                        (codex_price_row(price))
                    }
                }
            }
            (codex_price_add())
        }
    }
}

/// The three price fields for a model, shared by the edit rows and the add form.
/// `values` prefills the inputs when editing an existing row.
fn codex_price_fields(values: Option<&CodexPrice>) -> Markup {
    let field = |name: &str, label: &str, value: Option<f64>| {
        html! {
            div.field {
                label { (label) }
                input type="number" step="any" min="0" required
                    name=(name) value=[value] {}
            }
        }
    };
    html! {
        (field("input", "Input", values.map(|price| price.input_usd_per_mtok)))
        (field("cached_input", "Cached input", values.map(|price| price.cached_input_usd_per_mtok)))
        (field("output", "Output", values.map(|price| price.output_usd_per_mtok)))
    }
}

/// One model's row in the price table: its id, the three editable prices, and
/// Save and Delete buttons. Save upserts the row; Delete removes it.
fn codex_price_row(price: &CodexPrice) -> Markup {
    html! {
        form.price-row method="post" action="/settings/agents/codex/prices" {
            input type="hidden" name="model" value=(price.model);
            div.price-model { code { (price.model) } }
            (codex_price_fields(Some(price)))
            div.price-actions {
                button.primary { "Save" }
                button.danger formaction="/settings/agents/codex/prices/delete" formnovalidate { "Delete" }
            }
        }
    }
}

/// The form that adds a model to the price table. It posts to the same upsert
/// route, so typing a model that already has a row edits that row.
fn codex_price_add() -> Markup {
    html! {
        form.price-add method="post" action="/settings/agents/codex/prices" {
            h3 { "Add a model" }
            div.field {
                label for="codex-price-model" { "Model" }
                input #codex-price-model type="text" name="model" required
                    autocomplete="off" spellcheck="false" list="codex-price-models"
                    placeholder="gpt-6.1-sol" {}
                datalist #codex-price-models {
                    @for model in CODEX_PRICE_MODELS {
                        option value=(model) {}
                    }
                }
            }
            (codex_price_fields(None))
            div.save-bar {
                button.primary { "Add price" }
            }
        }
    }
}

/// Codex model ids suggested in the add form's datalist. They track the pinned
/// CLI's `gpt-6` family, and any other id can still be typed.
const CODEX_PRICE_MODELS: [&str; 2] = ["gpt-6.1-sol", "gpt-6.1-codex"];

/// The card that states whether a login is stored and when Codex last refreshed
/// it. It never shows the file, a token or the account id.
fn codex_account_card(account: &CodexAccount) -> Markup {
    html! {
        div.card {
            h2 { "ChatGPT login" }
            p.hint {
                "The factory runs Codex on a ChatGPT Plus or Pro subscription. The login should \
                 belong to the factory alone: a file copied from a laptop shares its refresh token \
                 with that laptop's Codex, and a refresh on one side can sign the other out."
            }
            @if account.stored {
                p.meta {
                    "A login is stored."
                    @if let Some(at) = &account.last_refresh {
                        " Last refreshed " (crate::localtime::date_time(at)) "."
                    }
                }
            } @else {
                p.meta { "No login is stored." }
            }
        }
    }
}

/// The "Sign in with `ChatGPT`" card. It starts the device login and holds the
/// fragment that polls for the URL, the code, success or a failure reason. The
/// button is disabled when the store is unavailable, since the login could not
/// be saved.
fn codex_login_card(login: &crate::device_login::LoginView, secrets_available: bool) -> Markup {
    html! {
        div.card {
            h2 { "Sign in with ChatGPT" }
            p.hint {
                "The factory starts a container that runs " code { "codex login --device-auth" }
                ". Open the link it shows and enter the code to sign the factory in."
            }
            @if !secrets_available {
                p.hint.bad { "The master key is missing, so a login cannot be saved. Clear still works." }
            }
            (codex_login_fragment(login, secrets_available))
        }
    }
}

/// The login area the page polls every five seconds while a login runs. It
/// stops polling once the login is signed in or failed, so a terminal state
/// stays put.
pub fn codex_login_fragment(
    login: &crate::device_login::LoginView,
    secrets_available: bool,
) -> Markup {
    login_fragment(&CODEX_LOGIN, login, secrets_available)
}

struct LoginUi {
    base: &'static str,
    id: &'static str,
    button: &'static str,
    /// The heading when the container exited but nothing was stored.
    not_stored: &'static str,
}

const CODEX_LOGIN: LoginUi = LoginUi {
    base: "/settings/agents/codex",
    id: "codex-login",
    button: "Sign in with ChatGPT",
    not_stored: "The login finished, but the factory could not store the login.",
};

const COPILOT_LOGIN: LoginUi = LoginUi {
    base: "/settings/agents/copilot",
    id: "copilot-login",
    button: "Sign in with GitHub",
    not_stored: "The login finished, but the factory could not store a token.",
};

fn login_fragment(
    ui: &LoginUi,
    login: &crate::device_login::LoginView,
    secrets_available: bool,
) -> Markup {
    use crate::device_login::LoginView;
    let polling = login.active();
    let start = format!("{}/login", ui.base);
    html! {
        div.agent-login id=(ui.id)
            hx-get=(format!("{}/login/fragment", ui.base))
            hx-trigger=[polling.then_some(POLL_TRIGGER)]
            hx-swap="outerHTML"
        {
            @match login {
                LoginView::Idle => {
                    form method="post" action=(start) {
                        button.primary disabled[!secrets_available] { (ui.button) }
                    }
                }
                LoginView::Starting => {
                    p.meta { "Starting the login container…" }
                    (login_cancel(ui))
                }
                LoginView::Waiting { url, code } => {
                    p { "Open " (external_link(url, url, None, None)) " and enter this code:" }
                    p.code-prompt { code { (code) } }
                    p.meta { "Waiting for you to finish in the browser…" }
                    (login_cancel(ui))
                }
                LoginView::SignedIn => {
                    p.good { "Signed in." }
                    form method="post" action=(start) {
                        button.primary disabled[!secrets_available] { "Sign in again" }
                    }
                }
                LoginView::Failed { reason } | LoginView::NotStored { reason } => {
                    p.bad {
                        @if matches!(login, LoginView::NotStored { .. }) {
                            (ui.not_stored)
                        } @else {
                            "The login did not finish."
                        }
                    }
                    pre.login-reason { (reason) }
                    form method="post" action=(start) {
                        button.primary disabled[!secrets_available] { "Try again" }
                    }
                }
            }
        }
    }
}

fn login_cancel(ui: &LoginUi) -> Markup {
    html! {
        form method="post" action=(format!("{}/login/cancel", ui.base)) {
            button.danger { "Cancel" }
        }
    }
}

pub fn copilot_login_fragment(
    login: &crate::device_login::LoginView,
    secrets_available: bool,
) -> Markup {
    login_fragment(&COPILOT_LOGIN, login, secrets_available)
}

pub fn settings_copilot(
    user: &str,
    nav: &SettingsNav,
    stored: bool,
    login: &crate::device_login::LoginView,
    secrets_available: bool,
    notice: Option<&str>,
) -> Markup {
    settings_page(
        user,
        nav,
        &SettingsPage::Agent("copilot".to_owned()),
        "Copilot",
        notice,
        &html! {
            h1 { "Copilot" }
            div.card {
                h2 { "GitHub login" }
                p.hint {
                    "The factory runs the GitHub Copilot CLI. The GitHub account needs a Copilot \
                     plan. Do not use moonthebot unless that is intended. The stored token goes \
                     to Copilot workers, which run it in their container."
                }
                @if stored {
                    p.meta { "A token is stored." }
                } @else {
                    p.meta { "No token is stored." }
                }
            }
            div.card {
                h2 { "Sign in with GitHub" }
                p.hint {
                    "The factory starts a container that runs " code { "copilot login --device-code" }
                    ". Open the link it shows and enter the code to sign the factory in."
                }
                @if !secrets_available {
                    p.hint.bad { "The master key is missing, so a login cannot be saved. Clear still works." }
                }
                (login_fragment(&COPILOT_LOGIN, login, secrets_available))
            }
            (copilot_token_card(stored, secrets_available))
        },
    )
}

fn copilot_token_card(stored: bool, secrets_available: bool) -> Markup {
    html! {
        form.card.settings-form method="post" action="/settings/agents/copilot/token" {
            h2 { "Paste a token" }
            p.hint {
                "A fine-grained personal access token with the Copilot Requests permission, or a \
                 " code { "gho_" } " or " code { "ghu_" } " token. Classic tokens are not \
                 supported. Stored encrypted and never shown."
            }
            @if !secrets_available {
                p.hint.bad { "The master key is missing, so a token cannot be saved here." }
            }
            div.field {
                label for="copilot-token" { "Token" }
                input #copilot-token.wide type="password" name="token"
                    autocomplete="off" spellcheck="false"
                    placeholder=(if stored { "set" } else { "not set" })
                    disabled[!secrets_available];
                p.hint {
                    @if stored { "A token is stored. " } @else { "No token is stored. " }
                    "Paste a new one to replace it. An empty field keeps it."
                }
            }
            div.save-bar {
                span.save-bar-note { "No unsaved changes." }
                button.primary disabled[!secrets_available] { "Save token" }
            }
            div.danger-zone {
                button.danger formaction="/settings/agents/copilot/token/clear" formnovalidate { "Clear token" }
            }
        }
    }
}

/// The paste card, the fallback for when device login fails: an operator runs
/// `codex login` on a trusted machine and pastes the contents of its
/// `auth.json`. The textarea carries no value, so a stored login is never in the
/// page source. An empty save keeps the stored login and Clear removes it.
fn codex_auth_card(stored: bool, secrets_available: bool) -> Markup {
    html! {
        form.card.settings-form method="post" action="/settings/agents/codex/auth" {
            h2 { "Paste a login" }
            p.hint {
                "Run " code { "codex login" } " on a machine with a browser and paste the contents \
                 of its " code { "~/.codex/auth.json" } " here. Stored encrypted and never shown."
            }
            @if !secrets_available {
                p.hint.bad { "The master key is missing, so a login cannot be saved here." }
            }
            div.field {
                label for="codex-auth" { "auth.json" }
                textarea #codex-auth.wide name="auth" rows="6"
                    autocomplete="off" spellcheck="false"
                    placeholder=(if stored { "set" } else { "not set" })
                    disabled[!secrets_available] {}
                p.hint {
                    @if stored { "A login is stored. " } @else { "No login is stored. " }
                    "Paste a new one to replace it. An empty field keeps it."
                }
            }
            // Save comes before Clear, so Enter in the field saves.
            div.save-bar {
                span.save-bar-note { "No unsaved changes." }
                button.primary disabled[!secrets_available] { "Save login" }
            }
            div.danger-zone {
                button.danger formaction="/settings/agents/codex/auth/clear" formnovalidate { "Clear login" }
            }
        }
    }
}

/// A forge's page for the login's own token. `account` is the account the
/// stored token belongs to. The token is never rendered: the field has no
/// value, an empty save keeps the stored token and Clear removes it.
pub fn settings_forge(
    user: &str,
    nav: &SettingsNav,
    kind: &ForgeKind,
    account: Option<&ForgeAccount>,
    secrets_available: bool,
    csrf: &str,
    notice: Option<&str>,
) -> Markup {
    let base = format!("/settings/forges/{}", kind.as_str());
    let name = kind.display_name();
    settings_page(
        user,
        nav,
        &SettingsPage::Forge(kind.clone()),
        name,
        notice,
        &html! {
            h1 { (name) }
            form.card.settings-form method="post" action=(format!("{base}/token")) {
                input type="hidden" name="csrf" value=(csrf);
                @if let ForgeKind::Forgejo(server) = kind {
                    input type="hidden" name="server" value=(server.registration);
                }
                h2 { "Your " (name) " token" }
                p.hint {
                    "The factory uses this token for the onboarding checks and for jobs in \
                     projects where you turned on \"Use my account\". The account needs push \
                     access to those repositories, and the factory ignores pull request \
                     comments from that account, so use a bot account rather than your own. \
                     Stored encrypted and never shown."
                }
                @if let ForgeKind::Forgejo(server) = kind {
                    p.hint.forgejo-scopes {
                        "A personal access token for " (server.base_url) " with the scopes "
                        code { "read:user" } ", " code { "write:repository" } " and "
                        code { "write:issue" } "."
                    }
                }
                @if !secrets_available {
                    p.hint.bad { "The master key is missing, so a token cannot be saved here. Clear still works." }
                }
                div.field {
                    label for="forge-token" { "Token" }
                    input #forge-token.wide type="password" name="token"
                        autocomplete="off" spellcheck="false"
                        placeholder=(if account.is_some() { "set" } else { "not set" })
                        disabled[!secrets_available];
                    p.hint {
                        @if let Some(account) = account {
                            "A token is stored. It belongs to the account " strong.forge-account { (account.login) } ". "
                        } @else {
                            "No token is stored. "
                        }
                        "Paste a new one to replace it. An empty field keeps it."
                    }
                }
                // Save comes before Clear, so Enter in the field saves.
                div.save-bar {
                    span.save-bar-note { "No unsaved changes." }
                    button.primary disabled[!secrets_available] { "Save token" }
                }
                div.danger-zone {
                    button.danger formaction=(format!("{base}/token/clear")) formnovalidate { "Clear token" }
                }
            }
        },
    )
}

/// The login's own Local API key page. The server URL and the CLI show as
/// text, so the login knows which server the key is for. The key is never
/// rendered: the field has no value, an empty save keeps it and Clear removes
/// it. The page says nothing about the factory key.
pub fn settings_local_key(
    user: &str,
    nav: &SettingsNav,
    server: Option<(&str, factory_core::Cli)>,
    key_set: bool,
    secrets_available: bool,
    csrf: &str,
    notice: Option<&str>,
) -> Markup {
    settings_page(
        user,
        nav,
        &SettingsPage::LocalKey,
        "Local API key",
        notice,
        &html! {
            h1 { "Local API key" }
            form.card.settings-form method="post" action="/settings/local" {
                input type="hidden" name="csrf" value=(csrf);
                input type="hidden" name="server" value=(server.map_or("", |(url, _)| url));
                h2 { "Your key for the local server" }
                p.hint {
                    "The factory sends this key to the local server for jobs you requested. \
                     The key reaches the worker, and the agent \
                     that runs there can read it. Stored encrypted and never shown."
                }
                @if let Some((url, cli)) = server {
                    p #local-server { "Server: " code { (url) } }
                    p #local-cli { "CLI: " (cli.display_name()) }
                } @else {
                    p #local-server { "no local server is set" }
                }
                @if !secrets_available {
                    p.hint.bad { "The master key is missing, so a key cannot be saved here. Clear still works." }
                }
                div.field {
                    label for="local-user-key" { "Key" }
                    input #local-user-key.wide type="password" name="key"
                        autocomplete="off" spellcheck="false"
                        placeholder=(if key_set { "set" } else { "not set" })
                        disabled[!secrets_available];
                    p.hint {
                        @if key_set { "A key is stored. " } @else { "No key is stored. " }
                        "Paste a new one to replace it. An empty field keeps it."
                    }
                }
                div.save-bar {
                    span.save-bar-note { "No unsaved changes." }
                    button.primary disabled[!secrets_available] { "Save key" }
                }
                div.danger-zone {
                    button.danger formaction="/settings/local/clear" formnovalidate { "Clear key" }
                }
            }
        },
    )
}

/// The admin page that lists the registered Forgejo servers and adds or
/// deletes one. `typed` is the name and URL a refused add had, put back in the
/// form. A name and a URL cannot be edited, because a stored token is bound to
/// the name.
#[allow(
    clippy::too_many_arguments,
    reason = "one argument per part of the page"
)]
pub fn settings_forgejo(
    user: &str,
    nav: &SettingsNav,
    instances: &[ForgejoInstance],
    csrf: &str,
    typed: (&str, &str),
    problem: Option<&str>,
    notice: Option<&str>,
) -> Markup {
    settings_page(
        user,
        nav,
        &SettingsPage::Forgejo,
        "Forgejo servers",
        notice,
        &html! {
            h1 { "Forgejo servers" }
            p.meta { "The Forgejo servers a factory admin has registered, such as a self-hosted server or Codeberg. A project, a token page and the upstream forge of a packaging project refer to a server by its name." }
            @if let Some(problem) = problem {
                div.card.failed { (problem) }
            }
            div.card {
                h2 { "Registered servers" }
                @if instances.is_empty() {
                    p.meta { "No servers yet." }
                }
                @for instance in instances {
                    div.forgejo-row {
                        div.server-row {
                            span.server-name { (instance.name) }
                            span.hint.server-url {
                                (external_link(&instance.base_url, &instance.base_url, None, None))
                                " · version " (instance.version)
                            }
                            form method="post" action={ "/settings/forgejo/" (crate::web::query_escape(&instance.name)) "/delete" } {
                                input type="hidden" name="csrf" value=(csrf);
                                button.danger { "Delete" }
                            }
                        }
                        @for warning in instance.warnings() {
                            p.hint.bad.forgejo-warning { (warning) }
                        }
                    }
                }
                p.hint { "A server cannot be edited. Delete it and add it again to change its URL. Deleting a server removes every token stored for it, and is refused while a project uses it." }
            }
            form.card method="post" action="/settings/forgejo" {
                input type="hidden" name="csrf" value=(csrf);
                h2 { "Add a server" }
                div.field {
                    label for="forgejo-new-name" { "Name" }
                    // The pattern and maxlength mirror `ForgejoInstance::validate`.
                    // `-` is escaped for the `v` flag, as in the MCP name field.
                    input #forgejo-new-name type="text" name="name" required
                        pattern=r"[a-z0-9\-]{1,32}" maxlength="32" value=(typed.0)
                        title="1 to 32 characters of a-z, 0-9 and -"
                        autocapitalize="none" autocomplete="off" spellcheck="false";
                    p.hint { "1 to 32 characters of a-z, 0-9 and -, and not github or salsa. Stored tokens and projects refer to it, so it cannot change." }
                }
                div.field {
                    label for="forgejo-new-url" { "URL" }
                    input #forgejo-new-url.wide type="text" inputmode="url" name="url" required
                        placeholder="https://codeberg.org" value=(typed.1);
                    p.hint { "An https URL, with a port or a sub-path if the server has one, and no user name, query or fragment. The factory reads " code { "/api/v1/version" } " to check it." }
                }
                div.save-bar { button.primary { "Add server" } }
            }
        },
    )
}

pub fn settings_mcp(
    user: &str,
    nav: &SettingsNav,
    servers: &[McpServer],
    secrets_available: bool,
    notice: Option<&str>,
) -> Markup {
    settings_page(
        user,
        nav,
        &SettingsPage::Mcp,
        "MCP servers",
        notice,
        &html! {
            h1 { "MCP servers" }
            (mcp_servers(servers, secrets_available))
            script { (PreEscaped(MCP_ROWS)) }
        },
    )
}

/// One project's use of a server, for the "Used by" list on the server's page.
pub struct ServerUsage {
    pub project: ProjectLabel,
    pub enabled: bool,
    pub stages: McpStages,
    pub headers: Vec<HeaderState>,
}

/// A server edit held back because saving it deletes stored values. The page
/// shows the posted `url` and `headers`, names each value it deletes in a
/// project the viewer collaborates on as `(project, header)`, and counts the
/// rest under `total`, with a button that saves anyway. `total` is every value
/// the edit deletes, so the button states the full effect, while `deletes`
/// names only the ones the viewer may see.
pub struct PendingEdit {
    pub url: String,
    pub headers: Vec<String>,
    pub deletes: Vec<(ProjectLabel, String)>,
    pub total: usize,
}

/// The state of a server's page beyond the server itself.
pub struct ServerPage<'a> {
    pub server: &'a McpServer,
    pub usage: &'a [ServerUsage],
    pub pending: Option<&'a PendingEdit>,
    /// A problem with the last submit, such as a delete with the wrong name.
    pub problem: Option<&'a str>,
}

pub fn settings_mcp_server(
    user: &str,
    nav: &SettingsNav,
    page: &ServerPage<'_>,
    notice: Option<&str>,
) -> Markup {
    let server = page.server;
    let stored: usize = page
        .usage
        .iter()
        .map(|usage| usage.headers.iter().filter(|header| header.set).count())
        .sum();
    settings_page(
        user,
        nav,
        &SettingsPage::McpServer(server.name.clone()),
        &server.name,
        notice,
        &html! {
            p.crumbs { a href="/settings/mcp" { "MCP servers" } " › " (server.name) }
            h1 { (server.name) }
            p.meta { "Kind: " span.server-kind { (server.transport.kind()) } }
            @if server.as_stdio().is_some() {
                p.meta { "Shared by every project. The command and base arguments run inside the worker of every project that turns the server on." }
            } @else {
                p.meta { "Shared by every project. Each project stores its own header values on its own page." }
            }
            @if let Some(problem) = page.problem {
                div.card.failed { (problem) }
            }
            (mcp_server_form(server, page.pending))
            (server_usage_card(&server.name, page.usage, server.as_stdio().is_some()))
            form.card.danger-card method="post" action={ "/settings/mcp/" (crate::web::query_escape(&server.name)) "/delete" } {
                h2 { "Delete " (server.name) }
                @if server.as_stdio().is_some() {
                    p.hint { "Removes the server from every project. Type the server's name to confirm." }
                } @else {
                    p.hint {
                        "Removes the server from every project and deletes the "
                        (stored) " stored "
                        (if stored == 1 { "value" } else { "values" })
                        " for it. Type the server's name to confirm."
                    }
                }
                div.row {
                    input.grow type="text" name="confirm_name" aria-label="Server name"
                        autocomplete="off" autocapitalize="none" spellcheck="false"
                        placeholder=(server.name);
                    button.danger { "Delete " (server.name) }
                }
            }
            script { (PreEscaped(MCP_ROWS)) }
        },
    )
}

/// The "Used by" card: each project with whether it runs the server, in how
/// many stages, and which of its header values are stored, with a link to the
/// project's page for the server.
fn server_usage_card(server: &str, usage: &[ServerUsage], stdio: bool) -> Markup {
    html! {
        div.card {
            h2 { "Used by" }
            @if usage.is_empty() {
                p.meta { "No project is onboarded yet." }
            }
            @for entry in usage {
                div.server-row {
                    span.server-name { (entry.project.shown) }
                    span.hint.server-url { (usage_line(entry.enabled, entry.stages)) }
                    (header_badges(&entry.headers))
                    @if !stdio {
                        a.link-button href=(project_mcp_href(&entry.project.id, server)) { "Open" }
                    }
                }
            }
        }
    }
}

fn external_link(
    href: &str,
    label: impl maud::Render,
    class: Option<&str>,
    title: Option<&str>,
) -> Markup {
    html! { a class=[class] href=(href) title=[title] target="_blank" rel="noopener noreferrer" { (label) } }
}

/// The repository of a project on a forge the factory supports, which builds
/// the links to it. A project on an unknown forge has none.
#[derive(Clone, Copy)]
struct Links<'a> {
    forge: &'a ForgeKind,
    repo: &'a str,
}

impl<'a> Links<'a> {
    fn of(project: Option<&'a Project>) -> Option<Links<'a>> {
        let project = project?;
        Some(Links {
            forge: project.forge.kind()?,
            repo: &project.repo,
        })
    }

    fn repo_url(self) -> String {
        self.forge.web_url(self.repo)
    }

    fn pr_url(self, number: u64) -> String {
        self.forge.pr_url(self.repo, number)
    }
}

fn source_link(links: Links<'_>) -> Markup {
    external_link(
        &links.repo_url(),
        html! { (branch_icon()) span { (links.repo) } },
        Some("source-link"),
        Some(&format!("Source on {}", links.forge.display_name())),
    )
}

fn branch_icon() -> Markup {
    html! {
        svg viewBox="0 0 24 24" width="16" height="16" fill="none" stroke="currentColor"
            stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true" {
            circle cx="6" cy="6" r="2.5" {}
            circle cx="6" cy="18" r="2.5" {}
            circle cx="18" cy="8" r="2.5" {}
            path d="M6 8.5v7M18 10.5c0 4-5 4-10.5 7" {}
        }
    }
}

fn project_mcp_href(project: &str, server: &str) -> String {
    format!(
        "/settings/projects/{}/mcp/{}",
        crate::web::query_escape(project),
        crate::web::query_escape(server)
    )
}

fn usage_line(enabled: bool, stages: McpStages) -> String {
    if !enabled {
        return "Off".into();
    }
    let on = [
        stages.clarify,
        stages.spec,
        stages.plan,
        stages.implement,
        stages.review,
    ]
    .into_iter()
    .filter(|on| *on)
    .count();
    format!("On in {on} of 5 stages")
}

/// A badge per declared header: stored, or missing.
fn header_badges(headers: &[HeaderState]) -> Markup {
    html! {
        @for header in headers {
            @if header.set {
                span.badge.ok title=(header.name) { "Key stored" }
            } @else {
                span.badge.you title=(header.name) { "Key missing" }
            }
        }
    }
}

pub fn settings_project(
    user: &str,
    nav: &SettingsNav,
    project: &ProjectSettings,
    selected: PipelinePart,
    notice: Option<&str>,
    problem: Option<&str>,
) -> Markup {
    settings_page(
        user,
        nav,
        &SettingsPage::Project(project.name.clone()),
        &project.shown,
        notice,
        &html! {
            h1 { (project.shown) }
            p.meta { "The path a job takes. Select a stage to set its model, effort and MCP servers, or a ✋ to change that approval gate. " (RUNS_KEEP_VALUES) }
            (project_form(project, selected, nav.admin))
            @if let Some(problem) = &project.forge_problem {
                div.card.failed { (problem) }
            }
            (project_name_card(project))
            @if let Some(card) = &project.forge_account {
                @if card.project_account.is_none() {
                    (missing_project_token_notice(&project.name))
                }
                (project_forge_account_card(&project.name, card))
            }
            (project_checks_card(project))
            (project_packages_card(project))
            (project_maintenance_card(project))
            (project_renovate_agent_card(project))
            (project_servers_card(project, nav.admin))
            @if let Some(problem) = problem {
                div.card.failed { (problem) }
            }
            (project_collaborators_card(project))
            (remove_project_card(project))
            script { (PreEscaped(SETTINGS_PIPELINE)) }
        },
    )
}

fn remove_project_card(project: &ProjectSettings) -> Markup {
    html! {
        form.card.danger-card method="post"
            action={ "/settings/projects/" (crate::web::query_escape(&project.name)) "/remove" } {
            h2 { "Remove project" }
            p.hint {
                "Cancels any running job, which stops its container, and deletes this "
                "project's settings, its maintenance settings, its collaborators, its project and upstream tokens, its stored MCP header values and the "
                "cost history of its jobs. The job rows stay until they expire, and open pull "
                "requests are left alone. This cannot be undone. Type the project name to confirm."
            }
            div.row {
                input.grow type="text" name="confirm" aria-label="Project name"
                    autocomplete="off" autocapitalize="none" spellcheck="false"
                    placeholder=(project.shown);
                button.danger { "Remove " (project.shown) }
            }
        }
    }
}

fn project_name_card(project: &ProjectSettings) -> Markup {
    html! {
        form.card.settings-form method="post"
            action={ "/settings/projects/" (crate::web::query_escape(&project.name)) "/name" } {
            h2 { "Project name" }
            div.field {
                label for="display_name" { "Name" }
                input #display_name.wide type="text" name="display_name" value=(project.shown);
                p.hint { "The name the factory's pages show for this project. Empty it to show the original name again." }
            }
            input type="hidden" name="csrf" value=(project.checks_csrf);
            (save_bar("Save name"))
        }
    }
}

fn project_checks_card(project: &ProjectSettings) -> Markup {
    html! {
        form.card.settings-form method="post"
            action={ "/settings/projects/" (crate::web::query_escape(&project.name)) "/checks" } {
            h2 { "Local checks" }
            div.field {
                label for="checks" { (CHECKS_LABEL) }
                input #checks.wide type="text" name="checks"
                    value=(project.checks.as_deref().unwrap_or(""));
                p.hint { (CHECKS_HINT) }
                p.hint { "A change applies the next time checks start for any job. A run already in the worker keeps the command it started with." }
            }
            input type="hidden" name="csrf" value=(project.checks_csrf);
            (save_bar("Save checks"))
        }
    }
}

fn project_packages_card(project: &ProjectSettings) -> Markup {
    html! {
        form.card.settings-form method="post"
            action={ "/settings/projects/" (crate::web::query_escape(&project.name)) "/packages" } {
            h2 { "Worker packages" }
            div.field {
                label for="packages" { "nixpkgs packages" }
                textarea #packages.wide name="packages" rows="4"
                    autocapitalize="none" spellcheck="false" placeholder="playwright-mcp" {
                    (project.packages.join("\n"))
                }
                p.hint {
                    "One nixpkgs package per line, such as playwright-mcp or python3Packages.requests. "
                    "The factory puts them on PATH in every agent and checks run of this project, "
                    "which is how a stdio MCP server gets the tools it needs. The first run with a new "
                    "package takes longer while nix downloads it."
                }
                p.hint { "A change applies at the next run. A run already in the worker keeps the packages it started with." }
            }
            input type="hidden" name="csrf" value=(project.checks_csrf);
            (save_bar("Save packages"))
        }
    }
}

fn project_renovate_agent_card(project: &ProjectSettings) -> Markup {
    let (agent_efforts, agent_models) = agent_data(&project.agents);
    let stored = project.renovate_agent.as_ref();
    let (agent, model, effort) = match stored {
        Some(stored) => (
            stored.agent.as_str(),
            stored.model.as_deref().unwrap_or_default(),
            stored.effort.as_str(),
        ),
        None => (
            stage_agent(&project.agent_selection, "review"),
            stage_model(&project.models, "review").unwrap_or_default(),
            stage_effort(&project.efforts, "review"),
        ),
    };
    html! {
        form.card.settings-form.renovate-agent-form method="post"
            action={ "/settings/projects/" (crate::web::query_escape(&project.name)) "/renovate-agent" }
            data-agent-efforts=(agent_efforts) data-agent-models=(agent_models) {
            h2 { "Renovate agent" }
            input type="hidden" name="csrf" value=(project.checks_csrf);
            div.field {
                label.check {
                    input type="checkbox" name="renovate_override" value="on" checked[stored.is_some()];
                    " Use a separate agent for Renovate runs"
                }
                p.hint { "Off by default. When off, Renovate runs use the review stage's agent, model and effort." }
                p.hint { "Covers the failure analysis, the diff check and the packaging analysis of Renovate pull requests. Fix jobs and gap jobs use the normal stages." }
            }
            (agent_fields(
                project,
                "renovate",
                agent,
                model,
                effort,
                "The agent that runs Renovate's analyses. A run already in flight keeps the agent it started on.",
                "Renovate",
            ))
            (save_bar("Save Renovate agent"))
        }
    }
}

fn project_maintenance_card(project: &ProjectSettings) -> Markup {
    let maintenance = &project.maintenance;
    let packaging = maintenance.packaging.as_ref();
    html! {
        form.card.settings-form method="post"
            action={ "/settings/projects/" (crate::web::query_escape(&project.name)) "/maintenance" } {
            h2 { "Maintenance" }
            input type="hidden" name="csrf" value=(project.checks_csrf);
            @if let Some(problem) = &project.upstream_problem {
                p.hint.bad.upstream-problem { (problem) }
            }
            div.field {
                label.check {
                    input type="checkbox" name="renovate" value="on" checked[maintenance.renovate];
                    " Renovate pull requests"
                }
                p.hint { "Off by default. Packaging projects need it on: the packaging analysis runs after a merged Renovate pull request." }
            }
            @if project.forge_account.as_ref().is_some_and(|card| !card.kind.has_fixed_renovate_account()) {
                div.field {
                    label for="renovate_author" { "Renovate account" }
                    input #renovate_author type="text" name="renovate_author" autocomplete="off"
                        autocapitalize="none" spellcheck="false" placeholder="renovate-bot"
                        value=(maintenance.renovate_author.as_deref().unwrap_or(""));
                    p.hint { "Required with Renovate pull requests on. Only GitHub has a fixed Renovate account. Self-hosted Renovate opens its requests as the user that owns its token. Give that " (project.forge_account.as_ref().map_or("", |card| card.kind.display_name())) " username." }
                }
            }
            div.field {
                label.check {
                    input type="checkbox" name="packaging" value="on" checked[packaging.is_some()];
                    " Packaging project"
                }
                p.hint { "Save with this ticked to set the packaging fields." }
            }
            @if let Some(packaging) = packaging {
                div.field {
                    label for="packaging_type" { "Type" }
                    select #packaging_type name="packaging_type" {
                        @for kind in [PackagingType::Snap, PackagingType::Deb, PackagingType::Other] {
                            option value=(kind.as_str()) selected[kind == packaging.kind] { (kind.as_str()) }
                        }
                    }
                }
                div.field {
                    label for="manifest_files" { "Manifest files" }
                    textarea #manifest_files name="manifest_files" rows="3"
                        autocapitalize="none" spellcheck="false" {
                        (packaging.manifest_files.join("\n"))
                    }
                    p.hint { "Repository paths, one per line. At least one." }
                }
                div.field {
                    label for="build_config_files" { "Build config files" }
                    textarea #build_config_files name="build_config_files" rows="3"
                        autocapitalize="none" spellcheck="false" {
                        (packaging.build_config_files.join("\n"))
                    }
                    p.hint { "Paths in the upstream repository, one per line. May be empty." }
                }
                div.field {
                    label for="upstream_repo" { "Upstream repository" }
                    input #upstream_repo type="text" name="upstream_repo" autocomplete="off"
                        autocapitalize="none" spellcheck="false"
                        placeholder=(if packaging.upstream_forge == ForgeKind::Salsa { "group/name" } else { "owner/name" })
                        value=(packaging.upstream_repo);
                }
                div.field {
                    label for="upstream_forge" { "Upstream forge" }
                    select #upstream_forge name="upstream_forge" {
                        @for (kind, _) in &project.upstream.accounts {
                            option value=(kind.as_str()) selected[*kind == packaging.upstream_forge] { (kind.display_name()) }
                        }
                    }
                    p.hint { "Where the upstream repository lives. It defaults to the project's own forge." }
                }
                div.field.upstream-check {
                    button type="submit" formnovalidate
                        formaction={ "/settings/projects/" (crate::web::query_escape(&project.name)) "/maintenance/check" } {
                        "Check connection"
                    }
                    p.hint { "Reads the upstream repository the way a job would and stores nothing." }
                    @if let Some(check) = &project.upstream.check {
                        p.hint.upstream-check-result.bad[check.readable != Some(true)] role="status" { (check.text) }
                    }
                }
                @for (forge, account) in &project.upstream.accounts {
                    (upstream_token_field(project, forge, account.as_ref()))
                }
            }
            (save_bar("Save maintenance"))
        }
    }
}

/// The optional token for an upstream on `forge`. Each forge has its own
/// field, status and buttons, and the buttons name the forge in their
/// `formaction`, so what a button acts on is what sits beside it however the
/// upstream forge select is set. They sit in the Maintenance form, so the card
/// can hold a token before it is saved.
fn upstream_token_field(
    project: &ProjectSettings,
    forge: &ForgeKind,
    account: Option<&ForgeAccount>,
) -> Markup {
    let base = format!(
        "/settings/projects/{}/upstream-token",
        crate::web::query_escape(&project.name)
    );
    let secrets = project.upstream.secrets_available;
    let id = format!("upstream-token-{}", forge.as_str());
    html! {
        div.field.upstream-token id=(id) {
            @if let ForgeKind::Forgejo(server) = forge {
                input type="hidden" name={ "server_" (forge.as_str()) } value=(server.registration);
            }
            label for={ (id) "-field" } { (forge.display_name()) " upstream token" }
            input id={ (id) "-field" } .wide type="password" name={ "token_" (forge.as_str()) }
                autocomplete="off" spellcheck="false"
                placeholder=(if account.is_some() { "set" } else { "not set" })
                disabled[!secrets];
            p.hint {
                @if let Some(account) = account {
                    "A " (forge.display_name()) " token is stored. It belongs to the account " strong.forge-account { (account.login) } ". "
                } @else {
                    "No " (forge.display_name()) " token is stored. "
                }
                "Optional. It reads an upstream on " (forge.display_name()) ". A project keeps one upstream token, so saving one replaces a token stored for the other forge. Most upstreams are public and are read with no token. Save a token whose account can read the upstream repository when it is private, or to lift the rate limit on unauthenticated reads. \
                 Stored encrypted and never shown. An empty field keeps the stored token."
            }
            @if !secrets {
                p.hint.bad { "The master key is missing, so a token cannot be saved here. Clear still works." }
            }
            button type="submit" formnovalidate disabled[!secrets] formaction={ (base) "?forge=" (forge.as_str()) } {
                "Save " (forge.display_name()) " upstream token"
            }
            button.danger type="submit" formnovalidate formaction={ (base) "/clear?forge=" (forge.as_str()) } {
                "Clear " (forge.display_name()) " upstream token"
            }
        }
    }
}

fn missing_project_token_notice(project: &str) -> Markup {
    html! {
        div.card.missing-project-token {
            "This project has no project token. Jobs do not use project tokens yet, but once they do, a job that does not run as a personal account cannot run without one. "
            a href={ "/settings/projects/" (crate::web::query_escape(project)) "#forge-account" } {
                "Save a project token"
            }
            "."
        }
    }
}

fn project_forge_account_card(project: &str, card: &ForgeAccountCard) -> Markup {
    let base = format!("/settings/projects/{}", crate::web::query_escape(project));
    let name = card.kind.display_name();
    let secrets = card.secrets_available;
    html! {
        div.card #forge-account {
            h2 { "Forge account" }
            p.hint {
                "This project is on " (name) ", repository "
                (external_link(&card.kind.web_url(&card.repo), &card.repo, None, None))
                ". Jobs do not use these tokens yet. Any collaborator can replace the project token. \
                 Stored encrypted and never shown."
            }
            form.settings-form method="post" action={ (base) "/forge-token" } {
                input type="hidden" name="csrf" value=(card.csrf);
                @if !secrets {
                    p.hint.bad { "The master key is missing, so a token cannot be saved here. Clear still works." }
                }
                div.field {
                    label for="project-token" { "Project token" }
                    input #project-token.wide type="password" name="token"
                        autocomplete="off" spellcheck="false"
                        placeholder=(if card.project_account.is_some() { "set" } else { "not set" })
                        disabled[!secrets];
                    p.hint {
                        @if let Some(account) = &card.project_account {
                            "A token is stored. It belongs to the account " strong.forge-account { (account.login) } ". "
                        } @else {
                            "No token is stored. "
                        }
                        "Paste a new one to replace it. An empty field keeps it."
                    }
                }
                div.save-bar {
                    span.save-bar-note { "No unsaved changes." }
                    button.primary disabled[!secrets] { "Save project token" }
                }
                div.danger-zone {
                    button.danger formaction={ (base) "/forge-token/clear" } formnovalidate { "Clear project token" }
                }
            }
            form.settings-form.forge-switch method="post" action={ (base) "/use-my-account" } {
                input type="hidden" name="csrf" value=(card.csrf);
                div.field {
                    label.check {
                        input type="checkbox" name="on" value="on" checked[card.use_my_account];
                        " Use my account"
                    }
                    p.hint {
                        "Records that you want the jobs you request to run as your own " (name) " account once jobs use it. Off by default. "
                        "This switch is yours alone."
                    }
                    @if card.use_my_account && !card.has_user_token {
                        p.hint.bad {
                            "You have no " (name) " token, so this switch has nothing to run as. "
                            a href={ "/settings/forges/" (card.kind.as_str()) } { "Save your " (name) " token" }
                            "."
                        }
                    }
                }
                (save_bar("Save switch"))
            }
        }
    }
}

fn project_collaborators_card(project: &ProjectSettings) -> Markup {
    let base = format!(
        "/settings/projects/{}/collaborators",
        crate::web::query_escape(&project.name)
    );
    let removable = project.collaborators.len() > 1;
    html! {
        div.card {
            h2 { "Collaborators" }
            p.hint { "Everyone who can see this project and its jobs. Add a collaborator by their Tailscale login. Any collaborator can add or remove others." }
            @for collaborator in &project.collaborators {
                div.server-row {
                    span.server-name { (collaborator.login) }
                    span.hint.server-url {
                        "added by " (collaborator.added_by) " · " (crate::localtime::date_time(&collaborator.added_at))
                    }
                    @if removable {
                        form method="post" action={ (base) "/remove" } {
                            input type="hidden" name="login" value=(collaborator.login);
                            button.danger { "Remove" }
                        }
                    }
                }
            }
            form.row method="post" action=(base) {
                input.grow type="text" name="login" aria-label="Tailscale login"
                    autocomplete="off" autocapitalize="none" spellcheck="false"
                    placeholder="name@example.com";
                button { "Add collaborator" }
            }
        }
    }
}

/// The MCP servers card on a project's page: each server with its state for
/// this project and a link to the project's page for it.
fn project_servers_card(project: &ProjectSettings, admin: bool) -> Markup {
    html! {
        div.card {
            h2 { "MCP servers" }
            @if project.servers.is_empty() {
                p.meta { "No servers yet." @if admin { " " a href="/settings/mcp" { "Add one" } "." } }
            }
            @for server in &project.servers {
                @let on = server.tools.iter().filter(|tool| !tool.disabled).count();
                div.server-row {
                    span.server-name { (server.name) }
                    span.hint.server-url {
                        (usage_line(server.enabled, server.stages))
                        @if server.stdio.is_some() {
                            " · stdio"
                        }
                        @if !server.tools.is_empty() {
                            " · " (on) " of " (server.tools.len()) " tools on"
                        }
                    }
                    (header_badges(&server.headers))
                    a.link-button href=(project_mcp_href(&project.name, &server.name)) { "Configure" }
                }
            }
        }
    }
}

/// A project's page for one MCP server: whether it runs, its stages, its
/// header values, the connection check and the tool list, in one form.
pub fn settings_project_mcp(
    user: &str,
    nav: &SettingsNav,
    project: &ProjectLabel,
    server: &ServerSettings,
    secrets_available: bool,
    notice: Option<&str>,
) -> Markup {
    settings_page(
        user,
        nav,
        &SettingsPage::Project(project.id.clone()),
        &format!("{} for {}", server.name, project.shown),
        notice,
        &html! {
            p.crumbs {
                a href={ "/settings/projects/" (crate::web::query_escape(&project.id)) } { (project.shown) }
                " › MCP › " (server.name)
            }
            h1 { (server.name) " for " (project.shown) }
            p.meta {
                @if nav.admin {
                    @if server.stdio.is_some() {
                        "The server's command and base arguments are on "
                    } @else {
                        "The server's URL and header names are on "
                    }
                    a href={ "/settings/mcp/" (crate::web::query_escape(&server.name)) } { "its own page" }
                    ". "
                }
                "The values here are this project's alone."
            }
            @if !secrets_available && server.stdio.is_none() {
                p.hint.bad { "The master key is missing, so header values cannot be saved. The enabled switch, stages and tools still save." }
            }
            form.card.settings-form method="post" action=(project_mcp_href(&project.id, &server.name)) {
                (project_server_block(project, server, secrets_available))
                (save_bar("Save"))
            }
            @if server.stdio.is_some() {
                script { (PreEscaped(MCP_ROWS)) }
            } @else {
                script { (PreEscaped(MCP_TOOLS)) }
            }
        },
    )
}

fn stdio_block(stdio: &StdioSettings) -> Markup {
    let line = std::iter::once(&stdio.command)
        .chain(&stdio.args)
        .chain(&stdio.extra_args)
        .map(|part| shell_quote(part))
        .collect::<Vec<_>>()
        .join(" ");
    html! {
        div.field {
            span.label { "Command" }
            p { code { (stdio.command) } }
            @if !stdio.args.is_empty() {
                span.label { "Base arguments" }
                p { @for arg in &stdio.args { code.mcp-arg { (arg) } " " } }
            }
            p.hint { "Set by an admin on the server's own page. They run inside the worker of every project that turns the server on." }
        }
        div.field {
            span.label { "Extra arguments" }
            div.mcp-args {
                @for arg in &stdio.extra_args {
                    @if arg.contains(['\r', '\n']) {
                        (escaped_arg_row(arg))
                    } @else {
                        (arg_row(arg))
                    }
                }
                @for _ in 0..3 {
                    (arg_row(""))
                }
            }
            template { (arg_row("")) }
            button.add-arg type="button" { "Add argument" }
            p.hint { "Appended after the server's own arguments, in this order, for this project's runs only. The agent can read them, so no secret belongs here. An argument is passed as one value, with no shell splitting, and may not contain ${. Runs do not start stdio servers yet, so a saved list takes effect once worker support lands." }
        }
        div.field {
            span.label { "Command line a run starts" }
            p { code.mcp-command-line { (line) } }
        }
        p.hint { "This server runs inside the worker, so the factory cannot check its connection or turn off single tools." }
    }
}

fn shell_quote(part: &str) -> String {
    if part.contains(['\r', '\n']) {
        let escaped = part
            .replace('\\', "\\\\")
            .replace('\'', "\\'")
            .replace('\r', "\\r")
            .replace('\n', "\\n");
        return format!("$'{escaped}'");
    }
    let plain = !part.is_empty()
        && part
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_=./:@%+,".contains(&b));
    if plain {
        part.to_owned()
    } else {
        format!("'{}'", part.replace('\'', "'\\''"))
    }
}

/// The header-name rows a server form shares: one text box per stored name, each
/// with a Remove button, then one empty box for a new name, then an "Add header"
/// button and the given `hint`. The names post as a repeated `header` field. The
/// box's `pattern` refuses anything but an HTTP header name in the browser, so a
/// pasted `Authorization: x` is caught before the form posts; the server
/// validates again. "Add header" and "Remove" run `MCP_ROWS`. Without
/// JavaScript the form still works: you add one name per save and remove a name
/// by clearing its box.
fn header_rows(names: &[String], hint: &Markup, validate: bool) -> Markup {
    html! {
        div.field {
            span.label { "Header names" }
            div.mcp-headers {
                @for name in names {
                    (header_row(name, validate))
                }
                (header_row("", validate))
            }
            template { (header_row("", validate)) }
            button.add-header type="button" { "Add header" }
            (hint)
        }
    }
}

/// One header-name row: a text box named `header` and a Remove button.
fn header_row(value: &str, validate: bool) -> Markup {
    html! {
        div.mcp-header-row {
            input.grow type="text" name="header" placeholder="Authorization"
                pattern=[validate.then_some(HEADER_NAME_PATTERN)] aria-label="Header name"
                title="Only the header name, with no colon or value."
                value=(value);
            button.remove-header type="button" { "Remove" }
        }
    }
}

/// The "Claude token" card on the Claude agent page. It shows only whether a token is stored, never the value:
/// the input carries no `value`, so the token is never in the page source. A
/// non-empty save replaces the token, an empty save keeps it, and the Clear
/// button removes it. When the secret store is unavailable the input and Save
/// are disabled with a message, the way MCP header values are, and the factory
/// falls back to `CLAUDE_CODE_OAUTH_TOKEN`. Clear stays enabled, so a stranded
/// token can still be removed after the master key is lost.
fn claude_token_card(claude_token_set: bool, secrets_available: bool) -> Markup {
    html! {
        form.card.settings-form method="post" action="/settings/agents/claude/token" {
            h2 { "Claude token" }
            p.hint { "The subscription token from " code { "claude setup-token" } ", used for every agent run and the plan usage tile. Stored encrypted and never shown." }
            @if !secrets_available {
                p.hint.bad { "The master key is missing, so the token cannot be changed here. The factory falls back to " code { "CLAUDE_CODE_OAUTH_TOKEN" } " from its environment." }
            }
            div.field {
                label for="claude-token" { "Token" }
                input #claude-token.wide type="password" name="claude_token"
                    autocomplete="off" placeholder=(if claude_token_set { "set" } else { "not set" })
                    disabled[!secrets_available];
                p.hint {
                    @if claude_token_set { "A token is stored. " } @else { "No token is stored. " }
                    "Type a new token to replace it. An empty field keeps it."
                }
            }
            // Save comes before Clear, so Enter in the field saves.
            div.save-bar {
                span.save-bar-note { "No unsaved changes." }
                button.primary disabled[!secrets_available] { "Save token" }
            }
            div.danger-zone {
                button.danger formaction="/settings/agents/claude/token/clear" formnovalidate { "Clear token" }
            }
        }
    }
}

/// The MCP servers page: a link per registered server to its own page, then a
/// form to add one. A project turns a server on and gives its header values on
/// the project's own page, so this page carries no per-project state.
fn mcp_servers(servers: &[McpServer], secrets_available: bool) -> Markup {
    html! {
        div.card {
            h2 { "MCP servers" }
            p.hint { "Remote MCP servers over HTTPS, and stdio servers the agent starts inside the worker. A project turns one on and picks its stages on its own page. Header values of a remote server are entered per project." }
            @if !secrets_available {
                p.hint.bad { "The master key is missing, so header values cannot be saved. The factory still runs, and a project's enabled servers and stages still save." }
            }
            @if servers.is_empty() {
                p.meta { "No servers yet." }
            }
            @for server in servers {
                div.server-row {
                    span.server-name { (server.name) }
                    span.hint.server-url { (server.transport.kind()) ": " (server_summary(server)) }
                    a.link-button href={ "/settings/mcp/" (crate::web::query_escape(&server.name)) } { "Open" }
                }
            }
        }
        form.card method="post" action="/settings/mcp" {
            h2 { "Add a server" }
            div.field {
                label for="mcp-new-name" { "Name" }
                // The pattern and maxlength mirror `validate_name` in mcp.rs, so
                // the browser blocks a bad name before submit. Change both together.
                // A browser compiles `pattern` with the `v` flag, where `-` and `|`
                // are syntax inside a `[...]` class and must be escaped; an invalid
                // pattern is silently ignored, so the trailing `-` is written `\-`.
                input #mcp-new-name type="text" name="name" required
                    pattern=r"[a-z0-9\-]{1,32}" maxlength="32"
                    title="1 to 32 characters of a-z, 0-9 and -"
                    autocapitalize="none" autocomplete="off" spellcheck="false";
                p.hint { "1 to 32 characters of a-z, 0-9 and -. It is the key in a run's MCP config and part of the proxy path." }
            }
            fieldset.kind-choice {
                legend { "Kind" }
                label { input type="radio" name="kind" value="remote" checked; " remote" }
                label { input type="radio" name="kind" value="stdio"; " stdio" }
                p.hint { "A kind cannot change after the server is added." }
            }
            div.kind-remote {
                div.field {
                    label for="mcp-new-url" { "URL" }
                    // No browser validation on the add form's remote fields: a
                    // hidden field that fails it would block a stdio submit, and
                    // without JavaScript it cannot be disabled. The server validates.
                    input #mcp-new-url.wide type="text" inputmode="url" name="url"
                        placeholder="https://mcp.context7.com/mcp";
                    p.hint { "An https URL to the server's streamable-HTTP endpoint." }
                }
                (header_rows(&[], &html! {
                    p.hint { "Only the header name, such as " code { "Authorization" } ", with no colon or value. Each project enters its own value on its own page, stored encrypted. A keyless server needs none." }
                }, false))
            }
            div.kind-stdio {
                div.field {
                    label for="mcp-new-command" { "Command" }
                    input #mcp-new-command.wide type="text" name="command"
                        placeholder="npx" maxlength="256"
                        autocapitalize="none" autocomplete="off" spellcheck="false";
                }
                (arg_rows(&[], &html! { (STDIO_HINT) }))
            }
            button.primary { "Add server" }
        }
    }
}

/// What the server list shows after the kind: a remote server's URL or a stdio
/// server's command line.
fn server_summary(server: &McpServer) -> String {
    match &server.transport {
        McpTransport::Remote { url, .. } => url.clone(),
        McpTransport::Stdio { command, args } => std::iter::once(command.as_str())
            .chain(args.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join(" "),
    }
}

const STDIO_HINT: &str = "The command and base arguments run inside the worker of every project that turns the server on, and each project can append arguments of its own. The agent can read every argument, so no secret belongs here. Each project supplies the command's dependencies through its worker packages. An argument is passed as one value, with no shell splitting, and may not contain ${.";

/// The argument rows a stdio form shares: one text box per stored argument,
/// each with a Remove button, then three empty boxes for new ones so the form
/// works without JavaScript, then an "Add argument" button and the given
/// `hint`. The arguments post as a repeated `arg` field, and the server drops
/// the empty ones.
fn arg_rows(args: &[String], hint: &Markup) -> Markup {
    html! {
        div.field {
            span.label { "Base arguments" }
            div.mcp-args {
                @for arg in args {
                    @if arg.contains(['\r', '\n']) {
                        (escaped_arg_row(arg))
                    } @else {
                        (arg_row(arg))
                    }
                }
                @for _ in 0..3 {
                    (arg_row(""))
                }
            }
            template { (arg_row("")) }
            button.add-arg type="button" { "Add argument" }
            (hint)
        }
    }
}

/// A stored argument with a line break, which a one-line input would strip. The
/// input shows it with `\`, line feed and carriage return escaped and posts it
/// as `arg_escaped`, so the value survives a save and an admin can edit it or
/// clear the box to remove it.
fn escaped_arg_row(value: &str) -> Markup {
    let shown = value
        .replace('\\', "\\\\")
        .replace('\r', "\\r")
        .replace('\n', "\\n");
    html! {
        div.mcp-arg-row {
            input.grow type="text" name="arg_escaped" aria-label="Argument with a line break"
                autocapitalize="none" autocomplete="off" spellcheck="false"
                value=(shown);
            button.remove-arg type="button" { "Remove" }
        }
    }
}

fn arg_row(value: &str) -> Markup {
    html! {
        div.mcp-arg-row {
            input.grow type="text" name="arg" aria-label="Argument"
                autocapitalize="none" autocomplete="off" spellcheck="false"
                value=(value);
            button.remove-arg type="button" { "Remove" }
        }
    }
}

/// One server's edit form on its own page: its URL and header names, a Delete
/// button that posts the same form to the delete route, and the save bar. The name is fixed,
/// because it keys the stored secrets and the proxy path.
fn mcp_server_form(server: &McpServer, pending: Option<&PendingEdit>) -> Markup {
    let action = format!("/settings/mcp/{}", server.name);
    let (stored_url, stored_headers) = match &server.transport {
        McpTransport::Remote { url, headers } => (url, headers),
        McpTransport::Stdio { command, args } => {
            return html! {
                form.card.settings-form method="post" action=(action) {
                    div.field {
                        label for={ "mcp-" (server.name) "-command" } { "Command" }
                        input id={ "mcp-" (server.name) "-command" }.wide type="text" name="command"
                            required maxlength="256" value=(command)
                            autocapitalize="none" autocomplete="off" spellcheck="false";
                    }
                    (arg_rows(args, &html! {
                        p.hint { (STDIO_HINT) }
                        p.hint { "An edit keeps every project's settings for the server and applies at each job's next run." }
                    }))
                    (save_bar("Save"))
                }
            };
        }
    };
    let url = pending.map_or(stored_url.as_str(), |pending| pending.url.as_str());
    let headers = pending.map_or(stored_headers.as_slice(), |pending| {
        pending.headers.as_slice()
    });
    let label = pending.map_or_else(
        || "Save".to_owned(),
        |pending| match pending.total {
            1 => "Save and delete 1 value".to_owned(),
            n => format!("Save and delete {n} values"),
        },
    );
    html! {
        form.card.settings-form method="post" action=(action) {
            @if let Some(pending) = pending {
                @let hidden = pending.total - pending.deletes.len();
                div.confirm {
                    p { strong { "Not saved yet." } " Saving this change deletes these stored values, and each project must enter them again:" }
                    ul {
                        @for (project, header) in &pending.deletes {
                            li { (project.shown) ": " code { (header) } }
                        }
                        @if hidden == 1 {
                            li { "1 value in a project you cannot see" }
                        } @else if hidden > 1 {
                            li { (hidden) " values in projects you cannot see" }
                        }
                    }
                    input type="hidden" name="confirm" value="on";
                }
            }
            div.field {
                label for={ "mcp-" (server.name) "-url" } { "URL" }
                input id={ "mcp-" (server.name) "-url" }.wide type="url" name="url" required
                    value=(url);
            }
            (header_rows(headers, &html! {
                p.hint { "Only the header name, with no colon or value. Each project enters its own value on its own page, stored encrypted." }
                p.hint { "Removing or renaming a header deletes every project's stored value for it, and changing the URL's host or port deletes all of them. The page asks before it saves such a change. Changing only the path keeps them." }
            }, true))
            @if pending.is_some() {
                div.save-bar.dirty {
                    span.save-bar-note { "Unsaved changes." }
                    button.primary { (label) }
                }
            } @else {
                (save_bar(&label))
            }
        }
    }
}

/// A part of a project's pipeline that can be selected: a stage, a gate or
/// merge. It is the `?stage=` value of the project's settings page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PipelinePart {
    Stage(&'static str),
    Gate(&'static str),
    Merge,
}

/// The five agent stages, in the order a job runs them.
const STAGES: [&str; 5] = ["clarify", "spec", "plan", "implement", "review"];
/// The gate after each stage that has one.
const GATE_AFTER: [(&str, &str); 3] =
    [("spec", "spec"), ("plan", "plan"), ("implement", "publish")];
impl PipelinePart {
    /// The part named by a `?stage=` value. An unknown or missing value
    /// selects the first stage.
    #[must_use]
    pub fn parse(value: Option<&str>) -> Self {
        let value = value.unwrap_or_default();
        if value == "merge" {
            return PipelinePart::Merge;
        }
        if let Some(gate) = value.strip_prefix("gate-")
            && let Some((_, gate)) = GATE_AFTER.iter().find(|(_, name)| *name == gate)
        {
            return PipelinePart::Gate(gate);
        }
        PipelinePart::Stage(
            STAGES
                .iter()
                .find(|stage| **stage == value)
                .copied()
                .unwrap_or(STAGES[0]),
        )
    }

    /// The `?stage=` value of this part.
    #[must_use]
    pub fn key(self) -> String {
        match self {
            PipelinePart::Stage(stage) => stage.to_owned(),
            PipelinePart::Gate(gate) => format!("gate-{gate}"),
            PipelinePart::Merge => "merge".to_owned(),
        }
    }
}

fn gate_label(gate: &str) -> &'static str {
    match gate {
        "spec" => "Approve the spec",
        "plan" => "Approve the plan",
        _ => "Approve before pushing",
    }
}

fn gate_help(gate: &str) -> &'static str {
    match gate {
        "spec" => {
            "The job stops after the spec is written and waits for you to approve it or ask for changes."
        }
        "plan" => "The job stops after the plan is written, before any code, and waits for you.",
        _ => {
            "The job stops once the checks pass, before the factory pushes the branch and opens the pull request. You see the commits and the diff stat first."
        }
    }
}

fn gate_on(gates: Gates, gate: &str) -> bool {
    match gate {
        "spec" => gates.spec,
        "plan" => gates.plan,
        _ => gates.publish,
    }
}

fn stage_model<'a>(models: &'a StageModels, stage: &str) -> Option<&'a str> {
    match stage {
        "clarify" => models.clarify.as_deref(),
        "spec" => models.spec.as_deref(),
        "plan" => models.plan.as_deref(),
        "implement" => models.implement.as_deref(),
        _ => models.review.as_deref(),
    }
}

fn stage_effort<'a>(efforts: &'a StageEfforts, stage: &str) -> &'a str {
    match stage {
        "clarify" => &efforts.clarify,
        "spec" => &efforts.spec,
        "plan" => &efforts.plan,
        "implement" => &efforts.implement,
        _ => &efforts.review,
    }
}

fn stage_agent<'a>(agents: &'a StageAgents, stage: &str) -> &'a str {
    match stage {
        "clarify" => &agents.clarify,
        "spec" => &agents.spec,
        "plan" => &agents.plan,
        "implement" => &agents.implement,
        _ => &agents.review,
    }
}

/// The effort levels the stage's selected agent offers, lowest first. The panel
/// draws the effort radios and the node's pips from these, so a Codex stage
/// shows Codex's levels and a Claude stage shows Claude's. An empty slice, for a
/// saved agent the registry no longer lists, draws no radios and no pips.
fn stage_efforts<'a>(project: &'a ProjectSettings, stage: &str) -> &'a [String] {
    agent_effort_levels(project, stage_agent(&project.agent_selection, stage))
}

fn agent_effort_levels<'a>(project: &'a ProjectSettings, agent: &str) -> &'a [String] {
    project
        .agents
        .iter()
        .find(|choice| choice.id == agent)
        .map_or(&[], |choice| choice.efforts.as_slice())
}

/// Each agent's effort levels and default, then its model suggestions, hint and
/// whether a model is required, as the JSON the agent fields' script reads from
/// `data-agent-efforts` and `data-agent-models` to redraw them on a switch.
fn agent_data(agents: &[AgentChoice]) -> (String, String) {
    let by_agent = |value: &dyn Fn(&AgentChoice) -> serde_json::Value| {
        serde_json::Value::Object(
            agents
                .iter()
                .map(|choice| (choice.id.clone(), value(choice)))
                .collect(),
        )
        .to_string()
    };
    let efforts = by_agent(&|choice| {
        serde_json::json!({
            "efforts": choice.efforts,
            "default": choice.default_effort,
        })
    });
    let models = by_agent(&|choice| {
        serde_json::json!({
            "models": choice.models,
            "hint": choice.model_hint,
            "required": choice.model_required,
        })
    });
    (efforts, models)
}

fn stage_on(stages: McpStages, stage: &str) -> bool {
    match stage {
        "clarify" => stages.clarify,
        "spec" => stages.spec,
        "plan" => stages.plan,
        "implement" => stages.implement,
        _ => stages.review,
    }
}

/// A project's settings drawn as the path a job takes: a node per stage, a
/// gate between the stages that have one, and merge at the end. Each node is
/// a `?stage=` link that opens its panel under the pipeline. Every panel is in
/// the one form and only the selected one shows, so a save posts every field
/// and a hidden panel's values are kept. `SETTINGS_PIPELINE` switches panels
/// without a reload and keeps the nodes in step with the fields.
fn project_form(project: &ProjectSettings, selected: PipelinePart, admin: bool) -> Markup {
    let name = &project.name;
    let href = |part: PipelinePart| {
        format!(
            "/settings/projects/{}?stage={}",
            crate::web::query_escape(name),
            part.key()
        )
    };
    let (agent_efforts, agent_models) = agent_data(&project.agents);
    html! {
        form.card.settings-form.pipeline-form method="post" action=(href(selected))
            data-agent-efforts=(agent_efforts) data-agent-models=(agent_models) {
            input type="hidden" name="gates" value="on";
            @for server in &project.servers {
                input type="hidden" name={ "mcp." (server.name) ".pipeline" } value="on";
            }
            ol.pipeline {
                @for stage in STAGES {
                    li.pipe-step {
                        (stage_node(project, stage, selected == PipelinePart::Stage(stage), &href(PipelinePart::Stage(stage))))
                    }
                    @if let Some((_, gate)) = GATE_AFTER.iter().find(|(after, _)| *after == stage) {
                        li.pipe-link {
                            (gate_node(project, gate, selected == PipelinePart::Gate(gate), &href(PipelinePart::Gate(gate))))
                        }
                    } @else {
                        li.pipe-link aria-hidden="true" {}
                    }
                }
                li.pipe-step {
                    a.pipe-node href=(href(PipelinePart::Merge)) data-part="merge"
                        aria-current=[(selected == PipelinePart::Merge).then_some("step")] {
                        span.pipe-name { "merge" }
                        span.pipe-model data-merge-label {
                            (if project.auto_merge { "Automatic" } else { "You, on GitHub" })
                        }
                        span.pipe-sub { "after green CI" }
                    }
                }
            }
            @for stage in STAGES {
                (stage_panel(project, stage, selected == PipelinePart::Stage(stage), admin))
            }
            @for (_, gate) in GATE_AFTER {
                (gate_panel(project, gate, selected == PipelinePart::Gate(gate)))
            }
            div.pipe-panel data-panel="merge" hidden[selected != PipelinePart::Merge] {
                h2 { "Merge" }
                div.field {
                    label.check {
                        input type="checkbox" name="auto_merge" value="on" checked[project.auto_merge];
                        " Merge pull requests when their checks pass"
                    }
                    p.hint { "The factory squash-merges once every check passed and no review comment waits for the agent. Off, you merge on GitHub." }
                }
            }
            (save_bar(&format!("Save {}", project.shown)))
        }
    }
}

fn stage_node(project: &ProjectSettings, stage: &str, selected: bool, href: &str) -> Markup {
    let off = stage == "review" && !project.llm_review;
    let model = stage_model(&project.models, stage).filter(|model| !model.is_empty());
    let required = project
        .agents
        .iter()
        .find(|choice| choice.id == stage_agent(&project.agent_selection, stage))
        .is_some_and(|choice| choice.model_required);
    let effort = stage_effort(&project.efforts, stage);
    let efforts = stage_efforts(project, stage);
    let level = efforts
        .iter()
        .position(|level| level == effort)
        .unwrap_or(0);
    html! {
        a.pipe-node.off[off] href=(href) data-part=(stage)
            aria-current=[selected.then_some("step")] {
            span.pipe-name { (stage) }
            span.pipe-model.dflt[model.is_none() || off] data-model-label=(stage) {
                @if off { "Off" } @else if let Some(model) = model { (model) }
                @else if required { "Model required" } @else { "CLI default" }
            }
            span.pipe-effort data-effort-label=(stage) title={ "effort " (effort) } {
                @for (index, _) in efforts.iter().enumerate() {
                    i.on[index <= level] {}
                }
            }
            span.pipe-mcp {
                @for server in &project.servers {
                    span data-server-label={ (server.name) "." (stage) }
                        hidden[!(server.enabled && stage_on(server.stages, stage))] {
                        (server.name)
                    }
                }
            }
        }
    }
}

fn gate_node(project: &ProjectSettings, gate: &str, selected: bool, href: &str) -> Markup {
    let on = gate_on(project.gates, gate);
    let changed = on != gate_on(project.default_gates, gate);
    html! {
        a.gate-node.on[on].changed[changed] href=(href) data-part={ "gate-" (gate) }
            data-gate-node=(gate) title=(gate_label(gate))
            aria-current=[selected.then_some("step")] {
            span aria-hidden="true" { "✋" }
            span.visually-hidden {
                (gate_label(gate)) ": " (if on { "on" } else { "off" })
            }
        }
    }
}

/// The agent, model and effort fields of one panel. `key` names the fields:
/// `{key}_agent`, `{key}` for the model and `{key}_effort`, plus the ids and
/// `data-*` hooks the panel script finds them by. `aria` prefixes the radio
/// groups' labels.
fn agent_fields(
    project: &ProjectSettings,
    key: &str,
    agent: &str,
    model: &str,
    effort: &str,
    agent_hint: &str,
    aria: &str,
) -> Markup {
    let choice = project.agents.iter().find(|choice| choice.id == agent);
    let suggestions: &[String] = choice.map_or(&[], |choice| &choice.models);
    let model_required = choice.is_some_and(|choice| choice.model_required);
    let model_hint = choice.map_or_else(
        || {
            format!(
                "An alias or any model ID. Empty runs the model the installed {agent} CLI picks."
            )
        },
        |choice| choice.model_hint.clone(),
    );
    html! {
        div.field {
            span.label { "Agent" }
            div.seg role="radiogroup" aria-label={ (aria) " agent" } {
                @for choice in &project.agents {
                    label.disabled[!choice.available] {
                        input type="radio" name={ (key) "_agent" } value=(choice.id)
                            checked[choice.id == agent] disabled[!choice.available];
                        span { (choice.display_name) }
                        @if !choice.available {
                            span.tag { "Not available yet" }
                        }
                    }
                }
            }
            p.hint { (agent_hint) }
        }
        div.field {
            label for={ (project.name) "-" (key) } { "Model" }
            div.model-chips data-model-chips=(key) data-agent=(agent) {
                @if !model_required {
                    button.chip type="button" data-model="" data-model-for=(key) { "CLI default" }
                }
                @for suggestion in suggestions {
                    button.chip type="button" data-model=(suggestion) data-model-for=(key) { (suggestion) }
                }
            }
            input id={ (project.name) "-" (key) } type="text" name=(key) list={ "model-suggestions-" (key) }
                placeholder=(if model_required { "Model ID" } else { "CLI default" }) value=(model) data-model-input=(key);
            datalist id={ "model-suggestions-" (key) } {
                @for suggestion in suggestions { option value=(suggestion) {} }
            }
            p.hint data-model-hint=(key) { (model_hint) }
        }
        div.field {
            span.label { "Effort" }
            div.seg role="radiogroup" aria-label={ (aria) " effort" } data-effort-seg=(key) {
                @for level in agent_effort_levels(project, agent) {
                    label {
                        input type="radio" name={ (key) "_effort" } value=(level)
                            checked[level == effort];
                        span { (level) }
                    }
                }
            }
        }
    }
}

fn stage_panel(project: &ProjectSettings, stage: &str, selected: bool, admin: bool) -> Markup {
    let name = &project.name;
    html! {
        div.pipe-panel data-panel=(stage) hidden[!selected] {
            h2 { (stage[..1].to_uppercase()) (stage[1..]) " stage" }
            @if stage == "review" {
                div.field {
                    label.check {
                        input type="checkbox" name="llm_review" value="on" checked[project.llm_review];
                        " Review with an LLM before merge"
                    }
                    p.hint { "When the checks pass, the review model reads the branch and asks for changes or approves. Changes go back to the agent. Off, the review stage does not run." }
                }
            }
            (agent_fields(
                project,
                stage,
                stage_agent(&project.agent_selection, stage),
                stage_model(&project.models, stage).unwrap_or_default(),
                stage_effort(&project.efforts, stage),
                "The agent that runs this stage. A run already in flight keeps the agent it started on.",
                stage,
            ))
            div.field {
                span.label { "MCP servers in this stage" }
                @if project.servers.is_empty() {
                    p.hint { "No servers yet." @if admin { " " a href="/settings/mcp" { "Add one" } "." } }
                }
                @for server in &project.servers {
                    label.check {
                        input type="checkbox" name={ "mcp." (server.name) ".stage." (stage) } value="on"
                            checked[server.enabled && stage_on(server.stages, stage)];
                        " " (server.name)
                    }
                    p.hint.indent {
                        @if server.headers.iter().any(|header| !header.set) {
                            span.bad { "Key missing. " }
                        }
                        @if server.stdio.is_some() {
                            a href=(project_mcp_href(name, &server.name)) { "Arguments for " (server.name) }
                        } @else {
                            a href=(project_mcp_href(name, &server.name)) { "Key and tools for " (server.name) }
                        }
                    }
                }
            }
        }
    }
}

fn gate_panel(project: &ProjectSettings, gate: &str, selected: bool) -> Markup {
    let on = gate_on(project.gates, gate);
    let default = gate_on(project.default_gates, gate);
    html! {
        div.pipe-panel data-panel={ "gate-" (gate) } hidden[!selected] {
            h2 { (gate_label(gate)) }
            div.field {
                label.check {
                    input type="checkbox" name={ "gate_" (gate) } value="on" checked[on]
                        data-gate=(gate) data-default=(if default { "on" } else { "off" });
                    " The job waits for you here"
                }
                p.hint { (gate_help(gate)) }
                p.hint {
                    "Factory default: " strong { (if default { "on" } else { "off" }) } ". "
                    span.gate-changed hidden[on == default] { "Changed for " (project.shown) ". " }
                    button.link-like type="button" data-gate-reset=(gate) { "Use the default" }
                }
                p.hint { "Applies to jobs created after you save. A job keeps the gates it started with." }
            }
        }
    }
}

/// One MCP server's block inside a project's form: an enabled switch, a
/// checkbox per stage, and a write-only field per declared header. The header
/// field shows only whether a value is stored, never the value, and its field
/// is disabled when the secret store is unavailable. The field names carry the
/// server as `mcp.<server>.…`, which the save handler splits back out.
fn project_server_block(
    project: &ProjectLabel,
    server: &ServerSettings,
    secrets_available: bool,
) -> Markup {
    let base = format!("mcp.{}", server.name);
    let stages = server.stages;
    html! {
           div.mcp-project {
               div.field {
                   label.check {
                       input type="checkbox" name={ (base) ".enabled" } value="on"
                           checked[server.enabled];
                       " Run this server for " (project.shown)
                   }
               }
               div.field {
                   p.hint { "The stages it runs in. An enabled server needs at least one." }
                   @for (stage, on) in [
                       ("clarify", stages.clarify),
                       ("spec", stages.spec),
                       ("plan", stages.plan),
                       ("implement", stages.implement),
                       ("review", stages.review),
                   ] {
                       label.check {
                           input type="checkbox" name={ (base) ".stage." (stage) } value="on"
                               checked[on];
                           " " (stage)
                       }
                   }
               }
    @if let Some(stdio) = &server.stdio {
                   (stdio_block(stdio))
               } @else {
               @for header in &server.headers {
                   div.field {
                       label for={ (base) "-header-" (header.name) } { (header.name) }
                       input id={ (base) "-header-" (header.name) }.wide type="password"
                           name={ (base) ".header." (header.name) }
                           autocomplete="off" placeholder=(if header.set { "set" } else { "not set" })
                           disabled[!secrets_available];
                       label.check {
                           input type="checkbox" name={ (base) ".clear." (header.name) } value="on";
                           " Clear the stored value"
                       }
                       p.hint {
                           @if header.set { "A value is stored. " } @else { "No value is stored. " }
                           "Type a new value to replace it. An empty field keeps it."
                       }
                   }
               }
               (mcp_refresh(&project.id, &server.name, &server.tools, None))
               }
           }
       }
}

/// The "Refresh connection" button, its result area and the server's tool list
/// in a project's server block. The button posts to the check route through
/// htmx, which replaces this whole block with the returned one, so the result
/// and the refreshed tool list arrive together without a page reload. The rest
/// of the form keeps its unsaved input; this block's tool checkboxes are drawn
/// again from the stored state. `type="button"` keeps it from submitting the
/// project form. The check uses the saved URL and values, so the hint says to
/// save before checking new ones. `result` is `None` on the settings page and
/// the check's outcome in the route's reply.
pub fn mcp_refresh(
    project: &str,
    server: &str,
    tools: &[McpToolState],
    result: Option<Markup>,
) -> Markup {
    let route = format!(
        "/settings/projects/{}/mcp/{}/check",
        crate::web::query_escape(project),
        crate::web::query_escape(server),
    );
    html! {
        div.mcp-refresh {
            div.field.mcp-check {
                button.secondary type="button"
                    hx-post=(route) hx-target="closest .mcp-refresh" hx-swap="outerHTML"
                    hx-disabled-elt="this" { "Refresh connection" }
                div.mcp-check-result { @if let Some(result) = result { (result) } }
                p.hint {
                    "Uses the saved URL and values, and lists the server's tools. "
                    "Save first to check new values."
                }
            }
            (mcp_tool_list(server, tools))
        }
    }
}

/// The most characters of a tool's description the settings page shows.
const MAX_TOOL_DESCRIPTION: usize = 200;

/// One checkbox per stored tool, checked when the tool is on. Each tool also
/// posts a hidden `mcp.<server>.shown.<tool>` field, so the save handler turns
/// off only the shown tools left unchecked. The server name has no `.`, so the
/// handler splits the tool name back out after the prefix.
fn mcp_tool_list(server: &str, tools: &[McpToolState]) -> Markup {
    let base = format!("mcp.{server}");
    html! {
        div.field.mcp-tools {
            @if tools.is_empty() {
                p.hint { "No tools listed yet. Refresh the connection to list them." }
            } @else {
                p.hint {
                    "The tools this project may call. The factory refuses a call to an "
                    "unchecked tool. A tool the server adds later starts checked. "
                    "Refreshing reloads this list and drops unsaved changes to it."
                }
                // `MCP_TOOLS` shows these controls and runs them. Without it they
                // stay hidden and each box is set by hand.
                div.mcp-tool-controls {
                    input.grow.mcp-tool-filter type="search" aria-label="Filter tools"
                        placeholder={ "Filter " (tools.len()) " tools" };
                    span.hint.mcp-tool-count {
                        (tools.iter().filter(|tool| !tool.disabled).count())
                        " of " (tools.len()) " on"
                    }
                    button.mcp-tools-all type="button" { "All on" }
                    button.mcp-tools-none type="button" { "All off" }
                }
                @for tool in tools {
                    div.mcp-tool data-tool=(tool.name.to_lowercase()) {
                        input type="hidden" name={ (base) ".shown." (tool.name) } value="on";
                        label.check {
                            input type="checkbox" name={ (base) ".tool." (tool.name) } value="on"
                                checked[!tool.disabled];
                            " " code { (tool.name) }
                            @if tool.new { " " span.badge.agent { "new" } }
                            @if !tool.offered { " " span.badge.factory { "no longer offered" } }
                        }
                        @if let Some(description) = &tool.description {
                            p.hint { (short_description(description)) }
                        }
                    }
                }
            }
        }
    }
}

fn short_description(description: &str) -> String {
    let line = description.lines().next().unwrap_or_default();
    if line.chars().count() > MAX_TOOL_DESCRIPTION {
        let mut cut: String = line.chars().take(MAX_TOOL_DESCRIPTION).collect();
        cut.push_str("...");
        cut
    } else {
        line.to_owned()
    }
}

/// The check's outcome as shown in the server block's result area: one line
/// styling the outcome like the page's ok/bad hints, a line on the tool
/// listing, and a line naming any declared header the project has no stored
/// value for. It carries no header value, so nothing a project stored leaks
/// into the page.
pub fn mcp_check_result(outcome: &CheckOutcome, missing: &[String]) -> Markup {
    html! {
        (check_outcome_line(outcome))
        @if let CheckOutcome::Ok { tools, .. } = outcome {
            @match tools {
                ToolList::Listed(tools) => p.hint.ok { "Listed " (tools.len()) " tools." },
                ToolList::Failed(reason) => p.hint.bad {
                    "Could not list the tools: " (reason) ". The stored list is unchanged."
                },
            }
        }
        @if !missing.is_empty() {
            p.hint { "No value stored for: " (missing.join(", ")) "." }
        }
    }
}

fn check_outcome_line(outcome: &CheckOutcome) -> Markup {
    match outcome {
        CheckOutcome::Ok {
            server_name,
            server_version,
            protocol_version,
            ..
        } => {
            let mut server = String::new();
            if let Some(name) = server_name {
                server.push_str(name);
            }
            if let Some(version) = server_version {
                if !server.is_empty() {
                    server.push(' ');
                }
                server.push_str(version);
            }
            html! {
                p.hint.ok {
                    "Connected"
                    @if !server.is_empty() { ": " (server) }
                    ", protocol " (protocol_version) "."
                }
            }
        }
        CheckOutcome::Unreachable => html! {
            p.hint.bad { "Could not reach the server: the connection was refused, the host is unknown, or TLS failed." }
        },
        CheckOutcome::Timeout => html! {
            p.hint.bad { "The server did not answer within 15 s." }
        },
        CheckOutcome::Refused { status } => html! {
            p.hint.bad {
                "The server refused the request (HTTP " (status) "). Check this project's header values."
            }
        },
        CheckOutcome::HttpStatus { status, location } => html! {
            p.hint.bad {
                "The server answered HTTP " (status) "."
                @if let Some(location) = location {
                    " It redirected to " (location) "; use the final URL as the server's endpoint."
                }
            }
        },
        CheckOutcome::RpcError { code, message } => html! {
            p.hint.bad {
                "The server rejected the handshake with JSON-RPC error " (code) ": " (message)
            }
        },
        CheckOutcome::NotMcp => html! {
            p.hint.bad {
                "The server answered, but not with an MCP reply. Check that the URL is the streamable-HTTP endpoint."
            }
        },
    }
}

/// The values the onboarding form holds on a redraw, so a failed submit keeps
/// what the operator typed.
pub struct OnboardView<'a> {
    /// The forges the selector lists, in the order the pages show them.
    pub forges: Vec<ForgeKind>,
    /// The forge to preselect. `None` after a redraw that found the form
    /// stale: the operator must choose again, so a changed instance is never
    /// picked by default.
    pub forge: Option<ForgeKind>,
    /// The project name as typed. Empty means the repository name is used.
    pub name: &'a str,
    pub repo: &'a str,
    pub base: &'a str,
    pub checks: &'a str,
    pub models: &'a StageModels,
    pub auto_merge: bool,
    pub llm_review: bool,
    /// The forge token page to link when the checks could not run for want of
    /// a token.
    pub token_page: Option<&'a str>,
}

/// The onboarding wizard. Step 1 is the project, step 2 the optional models and
/// flags. The second step appears only after the checks in `report` pass, so
/// the operator cannot create a project the checks reject. Before the first
/// submit `report` is `None` and only step 1 shows.
pub fn onboard(
    user: &str,
    view: &OnboardView<'_>,
    report: Option<&RepoReport>,
    notice: Option<&str>,
) -> Markup {
    let show_step2 = report.is_some_and(|report| report.errors.is_empty());
    layout(
        user,
        "Onboard a project",
        &html! {
            h1 { "Onboard a project" }
            @if let Some(notice) = notice {
                div.card.saved { (notice) }
            }
            p.meta { "The factory checks the repository as your own account on the selected forge before it creates anything. It creates no project while a required check fails." }
            form.card method="post" action="/onboard" {
                @for kind in &view.forges {
                    @if let ForgeKind::Forgejo(server) = kind {
                        input type="hidden" name={ "server_" (kind.as_str()) } value=(server.registration);
                    }
                }
                @if view.forge.is_none() {
                    input type="hidden" name="choose_forge" value="1";
                }
                h2 { "Project" }
                (forge_radios(view))
                div.field {
                    label for="repo" { "Repository" }
                    input #repo.wide type="text" name="repo" required
                        placeholder=(view.forge.as_ref().map_or("owner/name", repo_placeholder)) value=(view.repo);
                    @for kind in &view.forges {
                        p.hint data-forge=(kind.as_str()) hidden[view.forge.as_ref() != Some(kind)] {
                            (repo_hint(kind))
                        }
                    }
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
                    label for="checks" { (CHECKS_LABEL) }
                    input #checks.wide type="text" name="checks" value=(view.checks);
                    p.hint { (CHECKS_HINT) }
                }
                @if let Some(report) = report {
                    (onboard_results(report, view.token_page))
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
            script { (PreEscaped(ONBOARD_FORGE)) }
        },
    )
}

fn forge_radios(view: &OnboardView<'_>) -> Markup {
    html! {
        div.field {
            label { "Forge" }
            @for kind in &view.forges {
                label.check {
                    input type="radio" name="forge" value=(kind.as_str())
                        data-placeholder=(repo_placeholder(kind))
                        checked[view.forge.as_ref() == Some(kind)]
                        required[view.forge.is_none()];
                    " " (kind.display_name())
                }
            }
            p.hint { "Where the repository lives. The checks run as your own token for this forge." }
        }
    }
}

fn repo_hint(forge: &ForgeKind) -> String {
    match forge {
        ForgeKind::GitHub => "On GitHub, owner/name, such as Badger-Embedded/maestro.".to_owned(),
        ForgeKind::Salsa => {
            "On Salsa, the full project path, such as python-team/packages/foo.".to_owned()
        }
        ForgeKind::Forgejo(server) => {
            format!("On {}, owner/name, such as alice/project.", server.name)
        }
    }
}

fn repo_placeholder(forge: &ForgeKind) -> &'static str {
    match forge {
        ForgeKind::GitHub | ForgeKind::Forgejo(_) => "owner/name",
        ForgeKind::Salsa => "group/subgroup/project",
    }
}

/// The checks panel under step 1: the required checks that failed, or a line
/// that they passed, then any warnings, which do not block creation.
fn onboard_results(report: &RepoReport, token_page: Option<&str>) -> Markup {
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
                @if let Some(page) = token_page {
                    p { a href=(page) { "Open the token page" } }
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
    waits: &HashMap<i64, Wait>,
    auto_merge: bool,
    project: &str,
    repo: Option<&Project>,
) -> Region {
    let links = Links::of(repo);
    let in_section = |section: Section| -> Vec<&JobRow> {
        rows.iter()
            .copied()
            .filter(|row| {
                Section::of(status(&row.job, auto_merge, wait_of(waits, row.id)).lane) == section
            })
            .collect()
    };
    let live = rows.iter().any(|row| !row.job.is_finished());
    let inner = html! {
        @if !live {
            p.board-quiet { "Nothing needs you and no job is running." }
        }
        @for section in Section::ALL {
            @let jobs = in_section(section);
            @if !jobs.is_empty() {
                section class={ "inbox inbox-" (section.name()) } {
                    h2 { (section.title()) " " span.count { (jobs.len()) } }
                    @if section == Section::Done && jobs.len() > DONE_SHOWN {
                        (job_rows(&jobs[..DONE_SHOWN], costs, waits, auto_merge, links))
                        details.older data-key=(section.name()) {
                            summary { "Show " (jobs.len() - DONE_SHOWN) " older" }
                            (job_rows(&jobs[DONE_SHOWN..], costs, waits, auto_merge, links))
                        }
                    } @else {
                        (job_rows(&jobs, costs, waits, auto_merge, links))
                    }
                }
            }
        }
    };
    let version = version_of(&inner);
    let markup = html! {
        div.board #board hx-get=(fragment_url("board", project, &version))
            hx-trigger=(POLL_TRIGGER) hx-swap="outerHTML" {
            (inner)
        }
    };
    Region { version, markup }
}

fn job_rows(
    rows: &[&JobRow],
    costs: &HashMap<i64, f64>,
    waits: &HashMap<i64, Wait>,
    auto_merge: bool,
    links: Option<Links<'_>>,
) -> Markup {
    html! {
        ul.jobs {
            @for row in rows {
                @let wait = wait_of(waits, row.id);
                @let lane = status(&row.job, auto_merge, wait).lane;
                li class={ "job job-" (Section::of(lane).name()) } {
                    span.id { "#" (row.id) }
                    a.title href={ "/jobs/" (row.id) } title=(job_title(&row.job)) {
                        (job_title(&row.job))
                    }
                    div.job-sub {
                        (renovate_link(&row.job, links))
                        (picked_link(&row.job))
                        (turn_badge(&row.job, auto_merge, wait))
                        @if !row.job.is_finished() { (flow_steps(&row.job)) }
                        @if let (Stage::Requested, Some(waits_for)) = (row.job.stage(), waits_for(wait)) {
                            span.stage { (waits_for) }
                        } @else if let Some(detail) = row_detail(&row.job) {
                            span.stage { (detail) }
                        }
                        span.requester title=(row.requested_by) { (row.requested_by) }
                        span.cost { (cost_label(costs.get(&row.id).copied())) }
                    }
                    (row_actions(row, lane))
                }
            }
        }
    }
}

fn waits_for(wait: &Wait) -> Option<Markup> {
    let entries: Vec<(i64, &str)> = match wait {
        Wait::Ready => return None,
        Wait::Waiting(ids) => ids.iter().map(|id| (*id, "")).collect(),
        Wait::Blocked(entries) => entries
            .iter()
            .map(|(id, label)| (*id, label.as_str()))
            .collect(),
    };
    Some(html! {
        "waits for "
        @for (index, (id, label)) in entries.into_iter().enumerate() {
            @if index > 0 { ", " }
            @if label == DELETED_PREREQUISITE {
                "#" (id)
            } @else {
                a href={ "/jobs/" (id) } { "#" (id) }
            }
            @if !label.is_empty() { " (" (label) ")" }
        }
    })
}

/// The stage of a live job, or why a failed one failed. A merged or
/// cancelled job has none, because its badge already says so.
fn row_detail(job: &Job) -> Option<String> {
    match job.stage() {
        Stage::Failed {
            reason,
            dismissed: false,
            ..
        } => Some(first_line(reason)),
        Stage::Failed { .. } | Stage::Merged | Stage::Cancelled | Stage::Dismissed => None,
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
                } @else if *row.job.stage() == Stage::Proposed {
                    a.link-button href=(page) { "Open" }
                    form.approve method="post" action={ (page) "/approve" } {
                        input type="hidden" name="project" value=(row.project);
                        button.primary { "Approve" }
                    }
                } @else {
                    a.link-button href=(page) { "Open" }
                }
            }
        }
        @if lane == Lane::Failed {
            div.job-actions {
                @if row.job.can_retry() {
                    form.retry method="post" action={ (page) "/retry" } {
                        input type="hidden" name="project" value=(row.project);
                        button.primary { "Retry" }
                    }
                }
                form.dismiss method="post" action={ (page) "/dismiss" } {
                    input type="hidden" name="project" value=(row.project);
                    button { "Dismiss" }
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
        Stage::Renovate
        | Stage::Packaging
        | Stage::Proposed
        | Stage::Dismissed
        | Stage::Cancelled
        | Stage::Failed { .. } => return None,
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

/// The cost as the pill shows it. A job with no cost row, such as one that only
/// ran Copilot, reads "–"; a Local run writes a row of $0, so a job that only
/// ran on Local reads `$0.00`.
fn cost_label(usd: Option<f64>) -> String {
    match usd {
        Some(usd) => format_cost(usd),
        None => "–".to_owned(),
    }
}

/// Whether any of the job's cost rows is an estimate, read from the events the
/// page already loaded, so the cost pill can flag a total that includes one.
fn any_estimated(events: &[JobEvent]) -> bool {
    events
        .iter()
        .any(|event| event.event == "cost" && event.estimated)
}

#[allow(
    clippy::too_many_arguments,
    reason = "the page takes each value its handler read"
)]
pub fn job(
    user: &str,
    row: &JobRow,
    label: &ProjectLabel,
    project: Option<&Project>,
    settings: &Settings,
    logs: &[LogEntry],
    events: &[JobEvent],
    images: &[String],
    files: &[JobFile],
    cost_usd: Option<f64>,
    models: &StageModels,
    efforts: &StageEfforts,
    auto_merge: bool,
    links: &JobLinks,
    prerequisites: &[PrerequisiteView],
) -> Markup {
    let job = &row.job;
    let repo_url = Links::of(project);
    let id = row.id;
    let estimated = any_estimated(events);
    page(
        user,
        &format!("Job #{id}"),
        &html! {},
        &html! {
            (job_poll(false, id, job))
            (job_header(id, row, label, links, repo_url, cost_usd, estimated))
            @if let Some(problem) = project.and_then(|project| project.forge.problem()) {
                div.card.failed { (problem) }
            }
            div.job-layout {
                div.job-now {
                    (status_region(false, id, label, job, settings, auto_merge, links, prerequisites))
                    (queued_after_region(false, job, prerequisites))
                    p.meta { "Approval gates: " (gates_line(job.gates())) }
                }
                aside.job-side { (timeline_region(false, job, events, models, efforts)) }
                div.job-main data-job=(id) {
                    (request_card(id, job, images, files))
                    (spec_region(false, job))
                    (plan_region(false, job, repo_url))
                    (reviews_region(false, job))
                    (mcp_calls_region(false, job, events))
                    (log_region(false, logs))
                    @if *job.turn() != Turn::Done {
                        form.footer-actions method="post" action={ "/jobs/" (id) "/cancel" } {
                            button.danger { "Cancel job" }
                        }
                    }
                }
            }
            script { (PreEscaped(JOB_UPDATES)) }
            script { (PreEscaped(JOB_SECTIONS)) }
            script { (PreEscaped(DETAILS_STATE)) }
            script { (PreEscaped(LOCAL_TIME)) }
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
    label: &ProjectLabel,
    project: Option<&Project>,
    settings: &Settings,
    logs: &[LogEntry],
    events: &[JobEvent],
    cost_usd: Option<f64>,
    models: &StageModels,
    efforts: &StageEfforts,
    auto_merge: bool,
    links: &JobLinks,
    prerequisites: &[PrerequisiteView],
) -> Markup {
    let job = &row.job;
    let repo_url = Links::of(project);
    html! {
        @if terminal(job) { (job_poll(true, row.id, job)) }
        (cost_badge(true, cost_usd, any_estimated(events)))
        (timeline_region(true, job, events, models, efforts))
        (status_region(true, row.id, label, job, settings, auto_merge, links, prerequisites))
        (queued_after_region(true, job, prerequisites))
        (spec_region(true, job))
        (plan_region(true, job, repo_url))
        (reviews_region(true, job))
        (mcp_calls_region(true, job, events))
        (log_region(true, logs))
    }
}

/// Merged, failed and cancelled jobs do not change again, so their page and
/// its fragments carry no polling attribute.
fn terminal(job: &Job) -> bool {
    matches!(
        job.stage(),
        Stage::Merged | Stage::Failed { .. } | Stage::Cancelled | Stage::Dismissed
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
/// stays put while the figure changes. When any of the job's cost rows is an
/// estimate, such as a Codex run's cost from the price table, the figure carries
/// a `≈` and a title that says it includes an estimate.
fn cost_badge(oob: bool, cost_usd: Option<f64>, estimated: bool) -> Markup {
    let estimated = estimated && cost_usd.is_some_and(|usd| usd > 0.0);
    html! {
        span #job-cost.badge.cost
            hx-swap-oob=[oob.then_some("true")]
            title=[estimated.then_some("Includes an estimate for Codex runs.")]
        {
            @if estimated { "≈ " }
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
#[allow(
    clippy::too_many_arguments,
    reason = "the region takes each value the page read"
)]
fn status_region(
    oob: bool,
    id: i64,
    project: &ProjectLabel,
    job: &Job,
    settings: &Settings,
    auto_merge: bool,
    links: &JobLinks,
    prerequisites: &[PrerequisiteView],
) -> Markup {
    html! {
        div #job-status hx-swap-oob=[oob.then_some("true")] {
            @let wait = prerequisite_wait(prerequisites);
            @if *job.stage() == Stage::Requested && wait != Wait::Ready {
                (waiting_card(id, job, &wait, prerequisites))
            } @else {
                (status_card(id, project, job, settings, auto_merge, links, prerequisites))
            }
        }
    }
}

fn queued_after_region(oob: bool, job: &Job, prerequisites: &[PrerequisiteView]) -> Markup {
    html! {
        div #job-queued-after hx-swap-oob=[oob.then_some("true")] {
            @if *job.stage() != Stage::Requested && !prerequisites.is_empty() {
                p.queued-after {
                    "Queued after "
                    @for (index, prerequisite) in prerequisites.iter().enumerate() {
                        @if index > 0 { ", " }
                        (prerequisite_link(prerequisite))
                    }
                }
            }
        }
    }
}

fn spec_region(oob: bool, job: &Job) -> Markup {
    html! {
        div #job-spec hx-swap-oob=[oob.then_some("true")] { (spec_card(job)) }
    }
}

fn plan_region(oob: bool, job: &Job, repo_url: Option<Links<'_>>) -> Markup {
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

/// The MCP tool calls a job made, in a region a poll swaps. The proxy records
/// a row per call, so the card grows while a job runs and a call appears
/// without a reload.
fn mcp_calls_region(oob: bool, job: &Job, events: &[JobEvent]) -> Markup {
    html! {
        div #job-mcp-calls hx-swap-oob=[oob.then_some("true")] { (mcp_calls_card(job, events)) }
    }
}

/// Every MCP tool call the proxy recorded for the job, grouped by server, with
/// the servers in order of each server's first call and the calls oldest first
/// inside a group. A server with one call is a plain row; a server with more is
/// a collapsed group, see [`mcp_call_group`]. It reads the `job_events` the page
/// already loads, so it needs no extra query. An `mcp-unavailable` row stays a
/// timeline note and is not listed here. The header counts every call across
/// servers and, when any, the refused ones. The card renders only once the job
/// has a call or a refused call, so the OOB wrapper has a target before the
/// first one.
fn mcp_calls_card(job: &Job, events: &[JobEvent]) -> Markup {
    let rows: Vec<&JobEvent> = events
        .iter()
        .filter(|event| event.event == "mcp-tool-call" || event.event == "mcp-tool-blocked")
        .collect();
    let refused = rows
        .iter()
        .filter(|event| event.event == "mcp-tool-blocked")
        .count();
    // Group the rows by server, keeping the servers in order of their first
    // call and the calls oldest first within a server.
    let mut groups: Vec<(&str, Vec<&JobEvent>)> = Vec::new();
    for event in &rows {
        let (server, _) = split_detail(event.detail.as_deref());
        match groups.iter_mut().find(|(name, _)| *name == server) {
            Some((_, calls)) => calls.push(event),
            None => groups.push((server, vec![event])),
        }
    }
    html! {
        @if !rows.is_empty() {
            (section_card("mcp-calls", "MCP tool calls", &html! {
                span.log-count {
                    (rows.len()) " " (plural(rows.len(), "call"))
                    @if refused > 0 { " · " (refused) " refused" }
                }
            }, &html! {
                ol.mcp-calls {
                    @for (server, calls) in &groups {
                        li { (mcp_call_group(job, server, calls)) }
                    }
                }
            }))
        }
    }
}

/// One server's MCP calls. A server with a single call renders as a plain row
/// showing the server and tool, with no disclosure and no badge. A server with
/// two or more renders as a collapsed `<details>`: the summary shows the mono
/// server name, a counter badge with the call count, a bad-style `N refused`
/// badge when any call was refused, and the latest call's time. Expanding it
/// shows the server's calls as rows that drop the server name the summary
/// already carries. `details-state.js` reopens a group a poll left open, keyed
/// by the group's `data-key`.
fn mcp_call_group(job: &Job, server: &str, calls: &[&JobEvent]) -> Markup {
    let [single] = calls else {
        let refused = calls
            .iter()
            .filter(|event| event.event == "mcp-tool-blocked")
            .count();
        let latest = calls.last().expect("a group has at least one call");
        return html! {
            details.mcp-call-group data-server=(server) data-key=(server) {
                summary.mcp-call {
                    div.mcp-call-main {
                        span.mcp-call-tool { (server) }
                    }
                    div.mcp-call-aside {
                        span.badge.neutral { (calls.len()) }
                        @if refused > 0 {
                            span.badge.bad { (refused) " refused" }
                        }
                        span.when { (crate::localtime::time(&latest.at)) }
                    }
                }
                ol.mcp-call-list {
                    @for call in calls {
                        li.mcp-call { (mcp_call_body(job, call, false)) }
                    }
                }
            }
        };
    };
    html! {
        div.mcp-call { (mcp_call_body(job, single, true)) }
    }
}

/// One recorded MCP call: the tool, with its server when `with_server` is set
/// and the server and tool split from the row's `detail` at the first `": "`,
/// the stage and turn the job was in, the call's time, and a badge that says
/// whether the proxy forwarded or refused the call. A call inside a server
/// group drops the server, which its summary already shows.
fn mcp_call_body(job: &Job, event: &JobEvent, with_server: bool) -> Markup {
    let (server, tool) = split_detail(event.detail.as_deref());
    let refused = event.event == "mcp-tool-blocked";
    html! {
        div.mcp-call-main {
            span.mcp-call-tool {
                @if with_server { (server) " · " }
                (tool)
            }
            span.mcp-call-where { (event_stage_label(job, event)) " · " (turn_label(&event.turn)) }
        }
        div.mcp-call-aside {
            @if refused {
                span.badge.bad { "refused: tool turned off" }
            } @else {
                span.badge.neutral { "called" }
            }
            span.when { (crate::localtime::time(&event.at)) }
        }
    }
}

/// Splits an MCP row's `detail`, `"<server>: <tool>"`, at the first `": "`.
/// A detail with no separator is taken as the tool with no server.
fn split_detail(detail: Option<&str>) -> (&str, &str) {
    match detail {
        Some(detail) => detail.split_once(": ").unwrap_or(("", detail)),
        None => ("", ""),
    }
}

/// The stage a recorded event left the job in, with the pull request number
/// when the stage works on one, like the timeline's step labels.
fn event_stage_label(job: &Job, event: &JobEvent) -> String {
    match event.pr {
        Some(pr) => format!("{} · PR {} of {}", event.stage, pr + 1, job.plan().len()),
        None => event.stage.clone(),
    }
}

/// A readable label for the turn a recorded event left the job in, from the
/// stored `Turn::name` string. An agent turn reads like the timeline's actor
/// row, through the same wording as [`task_label`].
fn turn_label(turn: &str) -> &'static str {
    match turn {
        "agent:clarify" => "reading the repository",
        "agent:write-spec" => "writing the spec",
        "agent:write-plan" => "writing the plan",
        "agent:implement" => "implementing the pull request",
        "agent:answer" => "reading your answers",
        "agent:revise" => "revising after your feedback",
        "agent:fix-checks" => "fixing failed checks",
        "agent:fix-ci" => "fixing the CI failure",
        "agent:address-review" => "addressing review comments",
        "agent:rebase" => "rebasing onto the newest base branch to resolve a conflict",
        "agent:review" => "reviewing the pull request",
        "factory" | "rerun-ci" => "factory",
        _ => "waiting for you",
    }
}

/// `word` with an `s` unless `n` is 1, for a count label such as "1 call" or
/// "3 calls".
fn plural(n: usize, word: &str) -> String {
    if n == 1 {
        word.to_owned()
    } else {
        format!("{word}s")
    }
}

/// The log, in a region a poll swaps. `job-updates.js` keeps the reader's
/// scroll place across the swap: pinned to the bottom, or where they scrolled.
fn log_region(oob: bool, logs: &[LogEntry]) -> Markup {
    html! {
        div #job-log hx-swap-oob=[oob.then_some("true")] { (log_card(logs)) }
    }
}

fn job_header(
    id: i64,
    row: &JobRow,
    label: &ProjectLabel,
    links: &JobLinks,
    repo_url: Option<Links<'_>>,
    cost_usd: Option<f64>,
    estimated: bool,
) -> Markup {
    let job = &row.job;
    html! {
        div.title-row {
            h1 { "#" (id) " " (job_title(job)) }
            (cost_badge(false, cost_usd, estimated))
        }
        @if job.renovate_pr().is_some() {
            p.meta { (renovate_link(job, repo_url)) }
        }
        @if job.picked_pr().is_some() {
            p.meta {
                (picked_link(job))
                @if job.stacked_on().is_some()
                    && let Some(number) = job.plan().first().and_then(|pr| pr.number)
                {
                    " · fixed in "
                    @if let Some(links) = repo_url {
                        (external_link(&links.pr_url(number), html! { "#" (number) }, Some("stacked-pr"), None))
                    } @else {
                        span.stacked-pr { "#" (number) }
                    }
                }
            }
        }
        @if let Some((source, number)) = job.fix_source() {
            p.meta {
                "Fixes Renovate pull request #" (number) ", in "
                @if links.source == Some(source) {
                    a href={ "/jobs/" (source) } { "job " (source) }
                } @else {
                    "a job that is no longer available"
                }
                "."
            }
        }
        p.meta {
            @if let Some(links) = repo_url {
                (external_link(&links.repo_url(), &label.shown, None, None))
            } @else {
                (label.shown)
            }
            " · requested by " (row.requested_by) " · " (crate::localtime::date_time(&row.created_at))
        }
    }
}

/// The pull request a job works on, as "#N" linking to it on the forge.
fn picked_link(job: &Job) -> Markup {
    html! {
        @if let Some(pr) = job.picked_pr() {
            @if matches!(pr.mode, PrMode::QuickReview { .. } | PrMode::ReviewAndFix) {
                span.badge.factory.review-badge { "review" }
            }
            (external_link(&pr.url, html! { "#" (pr.number) }, Some("picked-pr"), Some(&pr.title)))
        }
    }
}

fn renovate_link(job: &Job, links: Option<Links<'_>>) -> Markup {
    html! {
        @if let Some(pr) = job.renovate_pr() {
            span.badge.factory { "Renovate" }
            @if let Some(links) = links {
                (external_link(&links.pr_url(pr.number), html! { "#" (pr.number) }, Some("renovate-pr"), None))
            } @else {
                span.renovate-pr { "#" (pr.number) }
            }
        }
    }
}

/// `links.gaps` holds the gap jobs that still belong to this Renovate job. A
/// job id can be reused after its job expires, so any other id gets no link.
fn renovate_details(pr: &RenovatePr, settings: &Settings, links: &JobLinks) -> Markup {
    html! {
        ul.renovate-details {
            li {
                "Reruns on this head commit: " (pr.reruns) " of " (settings.renovate_total_reruns)
                " (" (pr.auto_reruns) " of " (settings.renovate_auto_reruns)
                " automatic, before an agent reads the failure)"
            }
            @if let Some(verdict) = &pr.verdict {
                li {
                    "Diff check: "
                    @if verdict.safe { "safe" } @else { "unsafe" }
                    ". " (verdict.reason)
                }
                li { "Summary: " (verdict.summary) }
            }
            @if let Some(fix) = &pr.fix {
                li {
                    "Fix job: "
                    @if links.fix == Some(fix.job) {
                        a href={ "/jobs/" (fix.job) } { "job " (fix.job) }
                    } @else {
                        "a job that is no longer available"
                    }
                }
            }
            @match &pr.gaps {
                Some(jobs) if jobs.is_empty() => li { "No gaps found" },
                Some(jobs) => li {
                    "Packaging gaps found: "
                    @for (i, job) in jobs.iter().enumerate() {
                        @if i > 0 { ", " }
                        @if links.gaps.contains(job) {
                            a href={ "/jobs/" (job) } { "job " (job) }
                        } @else {
                            "a job that is no longer available"
                        }
                    }
                },
                None => {}
            }
        }
    }
}

/// The aside stays outside the toggle button, so a click on it does not
/// collapse the card. The button has an id so htmx keeps focus on it across a
/// poll swap. `data-section`, not `data-key`, keeps the card out of
/// `details-state.js`.
fn section_card(section: &str, heading: &str, aside: &Markup, body: &Markup) -> Markup {
    html! {
        div.card.section data-section=(section) {
            div.card-head {
                h2 {
                    button.section-toggle type="button" id={ "toggle-" (section) } aria-expanded="true" aria-controls={ "section-" (section) } {
                        (heading)
                    }
                }
                (aside)
            }
            div.card-body id={ "section-" (section) } { (body) }
        }
    }
}

fn request_card(id: i64, job: &Job, images: &[String], files: &[JobFile]) -> Markup {
    html! {
        (section_card("request", "Request", &html! {
            @if *job.stage() == Stage::Requested {
                a.nav href={ "/jobs/" (id) "/edit" } { "Edit" }
            }
        }, &html! {
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
            @if !files.is_empty() {
                ul.request-files {
                    @for file in files {
                        li {
                            a.file-tile href={ "/jobs/" (id) "/files/" (file.name) } {
                                (file_icon(is_zip(&file.name)))
                                span.file-name { (file.original_name) }
                            }
                        }
                    }
                }
            }
        }))
    }
}

/// The page to edit a queued job's request. It posts to `/jobs/{id}/edit` as
/// multipart, the way the new-request form posts. The textarea starts with the
/// current text, each current image and file carries a Remove checkbox that
/// posts its stored name, and `paste-images.js` drives the file input for new
/// uploads. The "Start after" picker ticks the job's current `prerequisites`
/// and lists the other eligible `candidates`. The form works without
/// JavaScript; the checkboxes and file input post on their own.
pub fn edit_request(
    user: &str,
    id: i64,
    job: &Job,
    images: &[String],
    files: &[JobFile],
    prerequisites: &[PrerequisiteView],
    candidates: &[JobRow],
) -> Markup {
    let mut options: Vec<StartAfterOption> = prerequisites
        .iter()
        .map(|prerequisite| StartAfterOption {
            name: "keep",
            value: prerequisite.row,
            id: prerequisite.id,
            title: first_line(
                prerequisite
                    .job
                    .as_ref()
                    .map_or(prerequisite.title.as_str(), Job::request),
            ),
            checked: true,
            state: match &prerequisite.job {
                Some(current) => {
                    (!current.can_be_prerequisite()).then(|| prerequisite_state(prerequisite))
                }
                None => Some(prerequisite_state(prerequisite)),
            },
        })
        .collect();
    let current: Vec<i64> = prerequisites
        .iter()
        .filter(|prerequisite| prerequisite.job.is_some())
        .map(|prerequisite| prerequisite.id)
        .collect();
    options.extend(
        candidates
            .iter()
            .filter(|row| !current.contains(&row.id))
            .map(StartAfterOption::candidate),
    );
    layout(
        user,
        &format!("Edit request · Job #{id}"),
        &html! {
            div.card {
                h1 { "Edit request" }
                form.edit-request method="post" action={ "/jobs/" (id) "/edit" }
                    enctype="multipart/form-data" {
                    label.visually-hidden for="request" { "Request" }
                    textarea #request name="request" required rows="8" { (job.request()) }
                    @if !images.is_empty() || !files.is_empty() {
                        fieldset.current-attachments {
                            legend { "Current attachments" }
                            @if !images.is_empty() {
                                ul.request-images {
                                    @for name in images {
                                        li {
                                            img src={ "/jobs/" (id) "/images/" (name) } alt=(name);
                                            label.remove-attachment {
                                                input type="checkbox" name="remove" value=(name);
                                                " Remove"
                                            }
                                        }
                                    }
                                }
                            }
                            @if !files.is_empty() {
                                ul.request-files {
                                    @for file in files {
                                        li {
                                            span.file-tile {
                                                (file_icon(is_zip(&file.name)))
                                                span.file-name { (file.original_name) }
                                            }
                                            label.remove-attachment {
                                                input type="checkbox" name="remove" value=(file.name);
                                                " Remove"
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    div.file-picker {
                        input #images type="file" name="images" multiple class="visually-hidden";
                        label.file-button for="images" { "Attach files" }
                        span.hint {
                            "Images (PNG, JPEG, GIF, WebP), text files or ZIP archives, up to 10 MiB \
                             each and 5 in total, counting the ones you keep. You can also paste \
                             images into the box above."
                        }
                    }
                    ul #pasted-images.pasted-images {}
                    (start_after(&options))
                    div.form-actions {
                        button.primary { "Save" }
                        a.nav href={ "/jobs/" (id) } { "Back to job" }
                    }
                }
                script { (PreEscaped(PASTE_IMAGES)) }
            }
        },
    )
}

/// Whether a stored file name is a ZIP archive, by its extension. The server
/// generates the name, so it ends in `.zip` only for an archive.
fn is_zip(name: &str) -> bool {
    std::path::Path::new(name)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("zip"))
}

/// An inline icon for a request file: an archive icon for a ZIP file, a
/// document icon otherwise. The icon is inline SVG in the factory's own markup,
/// so the job page fetches nothing from another host.
fn file_icon(zip: bool) -> Markup {
    let body = if zip {
        html! {
            path d="M13 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8z" {}
            path d="M13 2v6h6" {}
            path d="M10 4h2v2h-2zM10 8h2v2h-2zM10 12h2v2h-2z" {}
        }
    } else {
        html! {
            path d="M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8z" {}
            path d="M14 2v6h6" {}
            path d="M8 13h8M8 17h8M8 9h2" {}
        }
    };
    html! {
        svg.file-icon viewBox="0 0 24 24" width="40" height="40"
            fill="none" stroke="currentColor" stroke-width="1.5"
            stroke-linecap="round" stroke-linejoin="round" aria-hidden="true" {
            (body)
        }
    }
}

fn spec_card(job: &Job) -> Markup {
    html! {
        @if let Some(spec) = job.spec() {
            (section_card("spec", "Spec", &html! {}, &html! { pre.doc { (spec) } }))
        }
    }
}

fn plan_card(job: &Job, repo_url: Option<Links<'_>>) -> Markup {
    html! {
        @if !job.plan().is_empty() {
            (section_card("plan", "Plan", &html! {}, &html! {
                ol.plan {
                    @for (index, pr) in job.plan().iter().enumerate() {
                        li {
                            strong { (pr.title) }
                            @if let Some(number) = pr.number {
                                " · "
                                @if let Some(links) = repo_url {
                                    (external_link(&links.pr_url(number), html! { "#" (number) }, None, None))
                                } @else {
                                    "#" (number)
                                }
                            }
                            @if job.is_skipped(index) { " · skipped: nothing to publish" }
                            div.summary { (pr.summary) }
                            @if let Some(notes) = &pr.notes {
                                details.notes data-key={ "plan-notes-" (index) } {
                                    summary { "Notes for the implementer" }
                                    pre.doc { (notes) }
                                }
                            }
                        }
                    }
                }
            }))
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
            (section_card("reviews", "LLM review", &html! {}, &html! {
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
            }))
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
            (section_card("log", "Log", &html! {
                span.log-count { (logs.len()) @if logs.len() == 1 { " line" } @else { " lines" } }
            }, &html! {
                ol.log {
                    @for entry in logs {
                        li class={ "log-" (entry.kind) } {
                            span.log-at { (crate::localtime::time_seconds(&entry.at)) }
                            span.log-text { (entry.text) }
                        }
                    }
                }
            }))
        }
    }
}

/// The jobs a job page links to, each checked against the stored job, since
/// SQLite reuses the id of a deleted job.
#[derive(Default)]
pub struct JobLinks {
    /// Gap jobs that still belong to the Renovate job.
    pub gaps: Vec<i64>,
    /// The Renovate job's fix job, when it still is that job's fix job.
    pub fix: Option<i64>,
    /// The Renovate job a fix job is for, when it still is that pull request's job.
    pub source: Option<i64>,
    /// The other live job that may push to or merge a quick-reviewed pull
    /// request.
    pub held_by: Option<i64>,
    /// The token the job page's own forms carry.
    pub csrf: String,
}

fn renovate_wait_card(
    id: i64,
    job: &Job,
    settings: &Settings,
    reason: &str,
    state: RenovateWait,
    links: &JobLinks,
) -> Markup {
    let heading = match state {
        RenovateWait::Stopped => "Triage stopped",
        RenovateWait::Ready => "Ready to merge",
        RenovateWait::Unsafe => "The update is not safe to merge",
        RenovateWait::Fixing => "Waiting for a fix job",
        RenovateWait::RebaseFailed => "Rebase the pull request",
    };
    if state == RenovateWait::Fixing {
        return html! {
            div.card.status {
                span.badge.factory { "waiting for fix" }
                span.status-detail { (fix_reason(job, reason, links)) }
                @if let Some(pr) = job.renovate_pr() {
                    (renovate_details(pr, settings, links))
                }
            }
        };
    }
    html! {
        div.card.needs-you {
            div.card-head {
                span.badge.you { "needs you" }
                h2 { (heading) }
            }
            p.status-detail {
                @for (i, line) in reason.lines().enumerate() {
                    @if i > 0 { br; }
                    (line)
                }
            }
            @if let Some(pr) = job.renovate_pr() {
                (renovate_details(pr, settings, links))
            }
            @if job.can_retry() {
                div.row.actions {
                    form method="post" action={ "/jobs/" (id) "/retry" } {
                        button.primary { "Retry" }
                    }
                }
            }
            @if state == RenovateWait::Stopped {
                form.fix-form method="post" action={ "/jobs/" (id) "/fix" } {
                    input type="hidden" name="csrf" value=(links.csrf);
                    label for="fix-instructions" { "Fix it in a separate pull request" }
                    textarea #fix-instructions name="instructions" rows="3"
                        placeholder="What should the fix change? Optional." {}
                    p.hint {
                        "The factory starts a job that changes the repository, not the update. "
                        "When it merges, the factory asks Renovate to rebase the pull request."
                    }
                    button.primary { "Fix in a separate pull request" }
                }
            }
        }
    }
}

fn fix_reason(job: &Job, reason: &str, links: &JobLinks) -> Markup {
    let target = job
        .renovate_pr()
        .and_then(|pr| pr.fix.as_ref())
        .map(|fix| fix.job)
        .filter(|fix| links.fix == Some(*fix));
    let found = target.and_then(|job| {
        let mark = format!("#{job}");
        reason.find(&mark).map(|at| (job, at, mark.len()))
    });
    html! {
        @if let Some((fix, at, len)) = found {
            (&reason[..at])
            a href={ "/jobs/" (fix) } { "#" (fix) }
            (&reason[at + len..])
        } @else {
            (reason)
        }
    }
}

fn proposal_card(id: i64, job: &Job) -> Markup {
    html! {
        div.card.needs-you {
            div.card-head {
                span.badge.you { "proposal" }
                h2 { "The factory proposes this change" }
            }
            @if let Some((source, number)) = job.gap_source() {
                p.meta {
                    "Found after Renovate pull request #" (number) " merged, in "
                    a href={ "/jobs/" (source) } { "job " (source) }
                    "."
                }
            }
            pre.doc { (job.request()) }
            p.hint { "Approve to run it as a job of your own, with this project's gates. Dismiss to drop it for good." }
            div.row.actions {
                form method="post" action={ "/jobs/" (id) "/approve" } {
                    button.primary { "Approve" }
                }
                form method="post" action={ "/jobs/" (id) "/dismiss" } {
                    button { "Dismiss" }
                }
            }
        }
    }
}

fn quick_review_card(
    id: i64,
    job: &Job,
    links: &JobLinks,
    score: u8,
    review: &str,
    head: &str,
    comment: &CommentState,
) -> Markup {
    let fork = job.picked_pr().is_some_and(|picked| picked.fork);
    html! {
        div.card.needs-you.quick-review {
            div.card-head {
                span.badge.you { "needs you" }
                h2 { "Review of the pull request: " span.review-score { (score) "/5" } }
            }
            p.meta { "Reviewed commit " code title=(head) { (head.chars().take(12).collect::<String>()) } }
            @match comment {
                CommentState::Posted => { p.hint { "The review is posted on the pull request." } }
                CommentState::Pending => { p.hint { "The factory is posting the review on the pull request." } }
                CommentState::Failed { error, .. } => {
                    p.status-detail.comment-error { "Posting the review comment failed: " (error) }
                }
                CommentState::Off => {}
            }
            pre.review-text { (review) }
            @if *comment != CommentState::Pending {
            form.fix-form method="post" action={ "/jobs/" (id) "/fix-here" } {
                input type="hidden" name="csrf" value=(links.csrf);
                label for="review-instructions" { "What should the agent do?" }
                textarea #review-instructions name="instructions" rows="3"
                    placeholder="Instructions for the fix. Optional." {}
                @if fork {
                    p.hint { "The factory cannot push to a fork's branch, so it cannot fix this pull request in place." }
                } @else if let Some(holder) = links.held_by {
                    p.hint {
                        "Job " a href={ "/jobs/" (holder) } { "#" (holder) }
                        " may push to or merge this pull request, so it cannot be fixed in place."
                    }
                }
                div.row.actions {
                    @if !fork && links.held_by.is_none() {
                        button.primary { "Fix in this pull request" }
                    }
                    button.fix-elsewhere formaction={ "/jobs/" (id) "/fix-elsewhere" } { "Fix in another branch" }
                }
                p.hint {
                    @if fork {
                        "Another branch holds the fork's commits plus the fixes in a new pull request against its base branch."
                    } @else {
                        "Another branch opens a new pull request stacked on this one, which its author can merge in."
                    }
                }
            }
            }
            div.row.actions {
                form method="post" action={ "/jobs/" (id) "/cancel" } {
                    button { "Cancel" }
                }
            }
        }
    }
}

fn merge_left_card(job: &Job) -> Markup {
    html! {
        div.card.needs-you {
            div.card-head {
                span.badge.you { "needs you" }
                h2 { "Merge the pull request" }
            }
            p.status-detail {
                "CI is green and nothing waits for the agent. The factory never merges a \
                 pull request picked in the request form, so the merge is left to you. \
                 Merge it on the forge and the job ends. A comment on the pull request \
                 from someone with write access goes to the agent first, when the forge \
                 lets the factory check who can write."
            }
            @if let Some(picked) = job.picked_pr() {
                p { (external_link(&picked.url, format!("Pull request #{}", picked.number), None, None)) }
            }
        }
    }
}

#[allow(clippy::too_many_lines, reason = "one match arm per card")]
fn status_card(
    id: i64,
    project: &ProjectLabel,
    job: &Job,
    settings: &Settings,
    auto_merge: bool,
    links: &JobLinks,
    prerequisites: &[PrerequisiteView],
) -> Markup {
    let wait = prerequisite_wait(prerequisites);
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
        (_, Turn::User(Ask::Renovate { reason, state })) => {
            renovate_wait_card(id, job, settings, reason, *state, links)
        }
        (Stage::Proposed, _) => proposal_card(id, job),
        (
            Stage::Renovate
            | Stage::Packaging
            | Stage::Merged
            | Stage::Cancelled
            | Stage::Dismissed,
            _,
        ) => html! {
            div.card.status {
                (turn_badge(job, auto_merge, &wait))
                span.status-detail { (turn_detail(job, settings, auto_merge, &wait)) }
                @if let Some(pr) = job.renovate_pr() {
                    (renovate_details(pr, settings, links))
                }
            }
        },
        (_, Turn::User(Ask::MergeLeftToUser)) => merge_left_card(job),
        (
            _,
            Turn::User(Ask::QuickReview {
                score,
                review,
                head,
                comment,
            }),
        ) => quick_review_card(id, job, links, *score, review, head, comment),
        (_, Turn::User(Ask::ReviewStopped { reason, .. })) => html! {
            div.card.needs-you {
                div.card-head {
                    span.badge.you { "needs you" }
                    h2 { "The review stopped before merging" }
                }
                p.status-detail { (reason) }
                @if job.can_retry() {
                    div.row.actions {
                        form method="post" action={ "/jobs/" (id) "/retry" } {
                            button.primary { "Retry" }
                        }
                    }
                }
            }
        },
        (
            Stage::Failed {
                reason, dismissed, ..
            },
            _,
        ) => failed_card(id, project, job, reason, *dismissed),
        _ => html! {
            div.card.status {
                (turn_badge(job, auto_merge, &wait))
                span.status-detail { (turn_detail(job, settings, auto_merge, &wait)) }
            }
        },
    }
}

fn prerequisite_wait(prerequisites: &[PrerequisiteView]) -> Wait {
    Wait::fold(
        prerequisites
            .iter()
            .map(|p| (p.id, p.state, prerequisite_state(p))),
    )
}

fn prerequisite_state(prerequisite: &PrerequisiteView) -> String {
    match (&prerequisite.job, prerequisite.state) {
        (Some(job), _) => stage_label(job),
        (None, PrerequisiteState::Met) => "merged".into(),
        (None, _) => DELETED_PREREQUISITE.into(),
    }
}

fn prerequisite_link(prerequisite: &PrerequisiteView) -> Markup {
    html! {
        @if prerequisite.job.is_some() {
            a href={ "/jobs/" (prerequisite.id) } { "#" (prerequisite.id) }
        } @else {
            "#" (prerequisite.id)
        }
    }
}

fn waiting_card(id: i64, job: &Job, wait: &Wait, prerequisites: &[PrerequisiteView]) -> Markup {
    let list = html! {
        ul.prerequisites {
            @for prerequisite in prerequisites {
                li {
                    (prerequisite_link(prerequisite))
                    " "
                    @if let Some(job) = &prerequisite.job {
                        (job_title(job))
                    } @else {
                        (first_line(&prerequisite.title))
                    }
                    " · "
                    span.prerequisite-state { (prerequisite_state(prerequisite)) }
                }
            }
        }
    };
    let blocked: Vec<i64> = prerequisites
        .iter()
        .filter(|p| p.state == PrerequisiteState::Blocked)
        .map(|p| p.id)
        .collect();
    if matches!(wait, Wait::Blocked(_)) {
        html! {
            div.card.needs-you {
                div.card-head {
                    span.badge.you { "blocked" }
                    h2 { "Waiting for jobs that cannot merge yet" }
                }
                p.status-detail {
                    "The job will not start until "
                    @for (index, blocked_id) in blocked.iter().enumerate() {
                        @if index > 0 { ", " }
                        "#" (blocked_id)
                    }
                    @if blocked.len() == 1 {
                        " merges or is removed from the list."
                    } @else {
                        " merge or are removed from the list."
                    }
                }
                (list)
                div.row.actions {
                    a.nav href={ "/jobs/" (id) "/edit" } { "Edit the list" }
                }
            }
        }
    } else {
        html! {
            div.card.status {
                (turn_badge(job, false, wait))
                span.status-detail { "Waiting for these jobs to merge." }
                (list)
            }
        }
    }
}

fn failed_card(
    id: i64,
    project: &ProjectLabel,
    job: &Job,
    reason: &str,
    dismissed: bool,
) -> Markup {
    html! {
        div.card.failed {
            div.card-head {
                @if dismissed {
                    span.badge.factory { "dismissed" }
                    h2 { "The job was dismissed" }
                } @else {
                    span.badge.bad { "failed" }
                    h2 { "The job failed" }
                }
            }
            p.failure-reason { (reason) }
            @if job.checks_could_not_run() {
                p.hint {
                    "Revise the project's local checks command, then retry, or retry to run the checks again. "
                    a href={ "/settings/projects/" (crate::web::query_escape(&project.id)) } {
                        "Local checks for " (project.shown)
                    }
                }
            }
            @if job.can_retry() || !dismissed {
                div.row.actions {
                    @if job.can_retry() {
                        form method="post" action={ "/jobs/" (id) "/retry" } {
                            button.primary { "Retry" }
                        }
                    }
                    @if !dismissed {
                        form method="post" action={ "/jobs/" (id) "/dismiss" } {
                            button { "Dismiss" }
                        }
                    }
                }
            }
        }
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

static NO_WAIT: Wait = Wait::Ready;

fn wait_of(waits: &HashMap<i64, Wait>, id: i64) -> &Wait {
    waits.get(&id).unwrap_or(&NO_WAIT)
}

struct Status {
    lane: Lane,
    class: &'static str,
    text: &'static str,
}

/// One match decides the lane and the badge, so they cannot disagree.
fn status(job: &Job, auto_merge: bool, wait: &Wait) -> Status {
    let (lane, class, text) = match (job.stage(), job.turn()) {
        (Stage::Merged, _) => (Lane::Done, "ok", "merged"),
        (
            Stage::Dismissed
            | Stage::Failed {
                dismissed: true, ..
            },
            _,
        ) => (Lane::Done, "factory", "dismissed"),
        (Stage::Failed { .. }, _) => (Lane::Failed, "bad", "failed"),
        (Stage::Cancelled, _) => (Lane::Done, "factory", "cancelled"),
        (Stage::Proposed, _) => (Lane::You, "you", "proposal"),
        (Stage::Requested, _) => match wait {
            Wait::Ready => (Lane::Factory, "factory", "queued"),
            Wait::Waiting(_) => (Lane::Factory, "factory", "waiting"),
            Wait::Blocked(_) => (Lane::You, "you", "blocked"),
        },
        (
            _,
            Turn::User(Ask::Renovate {
                state: RenovateWait::Fixing,
                ..
            }),
        ) => (Lane::Factory, "factory", "waiting for fix"),
        (_, Turn::User(_)) if !collecting(job) => (Lane::You, "you", "needs you"),
        _ if in_review(job) && auto_merge && job.may_merge() => {
            (Lane::Factory, "factory", "merges when green")
        }
        _ if in_review(job) => (Lane::You, "you", "in review"),
        (_, Turn::Agent(_)) => (Lane::Agent, "agent", "agent working"),
        (_, Turn::User(_) | Turn::Factory | Turn::RerunCi { .. } | Turn::Done) => {
            (Lane::Factory, "factory", "factory working")
        }
    };
    Status { lane, class, text }
}

pub fn attention(job: &Job, auto_merge: bool, wait: &Wait) -> Option<&'static str> {
    match status(job, auto_merge, wait).lane {
        Lane::You => Some("needs-you"),
        Lane::Failed => Some("failed"),
        Lane::Agent | Lane::Factory | Lane::Done => None,
    }
}

pub fn job_title(job: &Job) -> String {
    first_line(&job.title())
}

fn turn_badge(job: &Job, auto_merge: bool, wait: &Wait) -> Markup {
    let Status { class, text, .. } = status(job, auto_merge, wait);
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
        Stage::Renovate => "renovate".into(),
        Stage::Packaging => "packaging".into(),
        Stage::Proposed => "proposal".into(),
        Stage::Dismissed => "dismissed".into(),
        Stage::Merged => "merged".into(),
        Stage::Cancelled => "cancelled".into(),
        Stage::Failed { .. } => "failed".into(),
    }
}

fn turn_detail(job: &Job, settings: &Settings, auto_merge: bool, wait: &Wait) -> String {
    match job.turn() {
        Turn::Agent(task) => format!("The agent is {}.", task_label(task)),
        Turn::Factory => match job.next_command() {
            Some(Command::RunChecks { .. }) => {
                "The factory is running the project's checks.".into()
            }
            Some(Command::Publish { pr })
                if job.picked_pr().is_some() && job.stacked_on().is_none() =>
            {
                let number = job.plan()[pr].number.unwrap_or_default();
                format!("The factory is pushing the commits to pull request #{number}.")
            }
            Some(Command::Publish { .. }) => {
                "The factory is pushing the branch and opening the pull request.".into()
            }
            Some(Command::WatchPr { number, .. }) if job.picked_pr().is_some() => format!(
                "Pull request #{number} waits for CI. The factory never merges it: merge it \
                 yourself on the forge when it is ready. Review comments go to the agent. \
                 The factory looks every {} seconds.",
                settings.poll_seconds
            ),
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
            Some(Command::WatchRenovate { number, .. }) => format!(
                "The factory watches pull request #{number}. It waits for CI, reruns failed \
                 runs within the limits, and looks every {} seconds.",
                settings.poll_seconds
            ),
            Some(Command::PostComment { number, .. }) => {
                format!("The factory is commenting on pull request #{number}.")
            }
            Some(Command::MergeRenovate { number, .. }) => {
                format!("The factory is approving and merging pull request #{number}.")
            }
            Some(Command::GatherPackaging { number }) => format!(
                "Pull request #{number} merged. The factory is reading the manifest and the \
                 upstream changes for the packaging analysis."
            ),
            Some(Command::ProposeGaps { .. }) => {
                "The factory is creating a job for each packaging gap.".into()
            }
            Some(Command::RequestRebase { number, .. }) => {
                format!("The factory is asking Renovate to rebase pull request #{number}.")
            }
            Some(
                Command::RunAgent { .. }
                | Command::Summarize { .. }
                | Command::RerunCi { .. }
                | Command::RerunRenovate { .. },
            )
            | None => match wait {
                Wait::Ready => "Waiting for a free worker.".into(),
                Wait::Waiting(_) | Wait::Blocked(_) => "Waiting for other jobs to merge.".into(),
            },
        },
        Turn::User(_) if collecting(job) => {
            "The factory is collecting the commits and the diff stat for your approval.".into()
        }
        Turn::RerunCi { .. } => "The factory is rerunning the failed CI runs.".into(),
        Turn::User(_) => "Waiting for you.".into(),
        Turn::Done => match job.stage() {
            Stage::Merged => "Every planned pull request is merged.".into(),
            Stage::Dismissed => "This proposal was dismissed.".into(),
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
        Task::MergeHead => "merging the commits someone else pushed to the pull request",
        Task::Rebase { .. } => "rebasing onto the newest base branch to resolve a conflict",
        Task::Review => "reviewing the pull request",
        Task::QuickReview => "reviewing the pull request for a score",
        Task::AnalyzeFailure { .. } => "reading the CI failure log",
        Task::CheckDiff { .. } => "checking the diff of the update",
        Task::FindGaps { .. } => "looking for packaging gaps",
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
