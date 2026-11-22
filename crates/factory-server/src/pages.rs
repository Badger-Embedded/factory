use std::collections::HashMap;

use axum::http::StatusCode;
use factory_core::{
    Ask, BranchSummary, Command, Gates, Job, PlanUsageSnapshot, Stage, Task, Turn, UsageWindow,
    Verdict,
};
use maud::{DOCTYPE, Markup, PreEscaped, html};

use crate::check::{CheckOutcome, ToolList};
use crate::cost::{format_cost, merge_fraction};
use crate::github::RepoReport;
use crate::mcp::{McpServer, McpStages, McpToolState};
use crate::projects::Project;
use crate::projects::{StageAgents, StageEfforts, StageModels};
use crate::settings::Settings;
use crate::store::{CodexPrice, Collaborator, JobEvent, JobFile, JobRow, LogEntry, ProjectTotals};
use crate::timeline::timeline;
use crate::workers::Slot;

pub const STYLESHEET: &str = include_str!("../static/style.css");
/// htmx 2.0.4, vendored so the dashboard loads no script from a CDN. The
/// tailnet host it runs on cannot reach one.
pub const HTMX: &str = include_str!("../static/htmx.min.js");
const PROJECT_PICKER: &str = include_str!("../static/project-picker.js");
const PASTE_IMAGES: &str = include_str!("../static/paste-images.js");
const MCP_HEADER_ROWS: &str = include_str!("../static/mcp-header-rows.js");
const SETTINGS_FORM: &str = include_str!("../static/settings-form.js");
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
    plan_usage: Option<&UsageTile>,
    auto_merge: bool,
    workers: &Workers,
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
            @if let Some(selected) = selected {
                div.status-row {
                    (self::workers(workers, jobs, project_names, selected))
                    (plan_usage_region(plan_usage))
                }
                (composer(selected))
                (board(&all, costs, auto_merge, selected))
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
pub fn workers(workers: &Workers, jobs: &[JobRow], visible: &[String], project: &str) -> Region {
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
                        .filter(|row| visible.iter().any(|name| name == &row.project));
                    @match (slot, row) {
                        (Some(slot), Some(row)) => {
                            li.worker.busy {
                                span.worker-name { "Worker " (number) }
                                a.worker-job href={ "/jobs/" (row.id) } title=(first_line(row.job.request())) {
                                    "#" (row.id)
                                    @if row.project != project { " " (row.project) }
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

/// The new-request form as a one-line bar. It grows, and shows the file
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
                    input #images type="file" name="images" multiple class="visually-hidden";
                    label.file-button for="images" { "Attach files" }
                    span.hint {
                        "Images (PNG, JPEG, GIF, WebP), text files or ZIP archives, up to 10 MiB \
                         each and 5 in total. You can also paste images into the box above."
                    }
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

/// A project's block on the settings page: its models, efforts and flags, and
/// its per-server MCP settings.
pub struct ProjectSettings {
    pub name: String,
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
    Workers,
    Gates,
    Github,
    /// An agent's account page, named by the agent's id.
    Agent(String),
    Mcp,
    McpServer(String),
    Project(String),
}

/// One agent in the sidebar's Agents group, as the registry reports it: the id
/// the open page marks itself with, the display name, the account page URL (none
/// for an agent with no page, which the sidebar lists without a link), and
/// whether the agent can run. A disabled entry shows "Not available yet".
pub struct AgentNav {
    pub id: String,
    pub name: String,
    pub url: Option<String>,
    pub available: bool,
}

/// What the settings sidebar lists besides its fixed entries.
pub struct SettingsNav {
    /// Onboarded project names, in name order.
    pub projects: Vec<String>,
    /// Registered MCP server names, in the registry's order.
    pub servers: Vec<String>,
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
            p.settings-group { "Factory" }
            (entry("/settings/workers".into(), "Workers", &SettingsPage::Workers))
            (entry("/settings/gates".into(), "Default gates", &SettingsPage::Gates))
            (entry("/settings/github".into(), "GitHub", &SettingsPage::Github))
            p.settings-group { "Agents" }
            @for agent in &nav.agents {
                @match (agent.available, &agent.url) {
                    (true, Some(url)) => (entry(url.clone(), &agent.name, &SettingsPage::Agent(agent.id.clone()))),
                    _ => span.settings-link.settings-disabled {
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
            p.settings-group { "Projects" }
            @for project in &nav.projects {
                (entry(
                    format!("/settings/projects/{}", crate::web::query_escape(project)),
                    project,
                    &SettingsPage::Project(project.clone()),
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

pub fn settings_github(
    user: &str,
    nav: &SettingsNav,
    settings: &Settings,
    notice: Option<&str>,
) -> Markup {
    settings_page(
        user,
        nav,
        &SettingsPage::Github,
        "GitHub",
        notice,
        &html! {
            h1 { "GitHub" }
            p.meta { (RUNS_KEEP_VALUES) }
            form.card.settings-form method="post" action="/settings/github" {
                div.field {
                    label for="poll_seconds" { "Seconds between looks at a pull request" }
                    input #poll_seconds type="number" name="poll_seconds" min="10" required
                        value=(settings.poll_seconds);
                    p.hint { "Also the wait before retrying a failed push or GitHub call." }
                }
                (save_bar("Save"))
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
    use crate::device_login::LoginView;
    let polling = login.active();
    html! {
        div.codex-login #codex-login
            hx-get="/settings/agents/codex/login/fragment"
            hx-trigger=[polling.then_some(POLL_TRIGGER)]
            hx-swap="outerHTML"
        {
            @match login {
                LoginView::Idle => {
                    form method="post" action="/settings/agents/codex/login" {
                        button.primary disabled[!secrets_available] { "Sign in with ChatGPT" }
                    }
                }
                LoginView::Starting => {
                    p.meta { "Starting the login container…" }
                    (codex_login_cancel())
                }
                LoginView::Waiting { url, code } => {
                    p { "Open " a href=(url) target="_blank" rel="noopener" { (url) } " and enter this code:" }
                    p.code-prompt { code { (code) } }
                    p.meta { "Waiting for you to finish in the browser…" }
                    (codex_login_cancel())
                }
                LoginView::SignedIn => {
                    p.good { "Signed in." }
                    form method="post" action="/settings/agents/codex/login" {
                        button.primary disabled[!secrets_available] { "Sign in again" }
                    }
                }
                LoginView::Failed { reason } => {
                    p.bad { "The login did not finish." }
                    pre.login-reason { (reason) }
                    form method="post" action="/settings/agents/codex/login" {
                        button.primary disabled[!secrets_available] { "Try again" }
                    }
                }
            }
        }
    }
}

/// The Cancel button shown while a login runs. It removes the container.
fn codex_login_cancel() -> Markup {
    html! {
        form method="post" action="/settings/agents/codex/login/cancel" {
            button.danger { "Cancel" }
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
            script { (PreEscaped(MCP_HEADER_ROWS)) }
        },
    )
}

/// One project's use of a server, for the "Used by" list on the server's page.
pub struct ServerUsage {
    pub project: String,
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
    pub deletes: Vec<(String, String)>,
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
            p.meta { "Shared by every project. Each project stores its own header values on its own page." }
            @if let Some(problem) = page.problem {
                div.card.failed { (problem) }
            }
            (mcp_server_form(server, page.pending))
            (server_usage_card(&server.name, page.usage))
            form.card.danger-card method="post" action={ "/settings/mcp/" (crate::web::query_escape(&server.name)) "/delete" } {
                h2 { "Delete " (server.name) }
                p.hint {
                    "Removes the server from every project and deletes the "
                    (stored) " stored "
                    (if stored == 1 { "value" } else { "values" })
                    " for it. Type the server's name to confirm."
                }
                div.row {
                    input.grow type="text" name="confirm_name" aria-label="Server name"
                        autocomplete="off" autocapitalize="none" spellcheck="false"
                        placeholder=(server.name);
                    button.danger { "Delete " (server.name) }
                }
            }
            script { (PreEscaped(MCP_HEADER_ROWS)) }
        },
    )
}

/// The "Used by" card: each project with whether it runs the server, in how
/// many stages, and which of its header values are stored, with a link to the
/// project's page for the server.
fn server_usage_card(server: &str, usage: &[ServerUsage]) -> Markup {
    html! {
        div.card {
            h2 { "Used by" }
            @if usage.is_empty() {
                p.meta { "No project is onboarded yet." }
            }
            @for entry in usage {
                div.server-row {
                    span.server-name { (entry.project) }
                    span.hint.server-url { (usage_line(entry.enabled, entry.stages)) }
                    (header_badges(&entry.headers))
                    a.link-button href=(project_mcp_href(&entry.project, server)) { "Open" }
                }
            }
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
        &project.name,
        notice,
        &html! {
            h1 { (project.name) }
            p.meta { "The path a job takes. Select a stage to set its model, effort and MCP servers, or a ✋ to change that approval gate. " (RUNS_KEEP_VALUES) }
            (project_form(project, selected))
            (project_servers_card(project))
            @if let Some(problem) = problem {
                div.card.failed { (problem) }
            }
            (project_collaborators_card(project))
            script { (PreEscaped(SETTINGS_PIPELINE)) }
        },
    )
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
fn project_servers_card(project: &ProjectSettings) -> Markup {
    html! {
        div.card {
            h2 { "MCP servers" }
            @if project.servers.is_empty() {
                p.meta { "No servers yet. " a href="/settings/mcp" { "Add one" } "." }
            }
            @for server in &project.servers {
                @let on = server.tools.iter().filter(|tool| !tool.disabled).count();
                div.server-row {
                    span.server-name { (server.name) }
                    span.hint.server-url {
                        (usage_line(server.enabled, server.stages))
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
    project: &str,
    server: &ServerSettings,
    secrets_available: bool,
    notice: Option<&str>,
) -> Markup {
    settings_page(
        user,
        nav,
        &SettingsPage::Project(project.to_owned()),
        &format!("{} for {project}", server.name),
        notice,
        &html! {
            p.crumbs {
                a href={ "/settings/projects/" (crate::web::query_escape(project)) } { (project) }
                " › MCP › " (server.name)
            }
            h1 { (server.name) " for " (project) }
            p.meta {
                "The server's URL and header names are on "
                a href={ "/settings/mcp/" (crate::web::query_escape(&server.name)) } { "its own page" }
                ". The values here are this project's alone."
            }
            @if !secrets_available {
                p.hint.bad { "The master key is missing, so header values cannot be saved. The enabled switch, stages and tools still save." }
            }
            form.card.settings-form method="post" action=(project_mcp_href(project, &server.name)) {
                (project_server_block(project, server, secrets_available))
                (save_bar("Save"))
            }
            script { (PreEscaped(MCP_TOOLS)) }
        },
    )
}

/// The header-name rows a server form shares: one text box per stored name, each
/// with a Remove button, then one empty box for a new name, then an "Add header"
/// button and the given `hint`. The names post as a repeated `header` field. The
/// box's `pattern` refuses anything but an HTTP header name in the browser, so a
/// pasted `Authorization: x` is caught before the form posts; the server
/// validates again. "Add header" and "Remove" run `MCP_HEADER_ROWS`. Without
/// JavaScript the form still works: you add one name per save and remove a name
/// by clearing its box.
fn header_rows(names: &[String], hint: &Markup) -> Markup {
    html! {
        div.field {
            span.label { "Header names" }
            div.mcp-headers {
                @for name in names {
                    (header_row(name))
                }
                (header_row(""))
            }
            button.add-header type="button" { "Add header" }
            (hint)
        }
    }
}

/// One header-name row: a text box named `header` and a Remove button.
fn header_row(value: &str) -> Markup {
    html! {
        div.mcp-header-row {
            input.grow type="text" name="header" placeholder="Authorization"
                pattern=(HEADER_NAME_PATTERN) aria-label="Header name"
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
            p.hint { "Remote MCP servers over HTTPS, offered to agent runs. A project turns one on and picks its stages on its own page. Header values are entered per project." }
            @if !secrets_available {
                p.hint.bad { "The master key is missing, so header values cannot be saved. The factory still runs, and a project's enabled servers and stages still save." }
            }
            @if servers.is_empty() {
                p.meta { "No servers yet." }
            }
            @for server in servers {
                div.server-row {
                    span.server-name { (server.name) }
                    span.hint.server-url { (server.url) }
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
            div.field {
                label for="mcp-new-url" { "URL" }
                input #mcp-new-url.wide type="url" name="url" required
                    placeholder="https://mcp.context7.com/mcp";
                p.hint { "An https URL to the server's streamable-HTTP endpoint." }
            }
            (header_rows(&[], &html! {
                p.hint { "Only the header name, such as " code { "Authorization" } ", with no colon or value. Each project enters its own value on its own page, stored encrypted. A keyless server needs none." }
            }))
            button.primary { "Add server" }
        }
    }
}

/// One server's edit form on its own page: its URL and header names, a Delete
/// button that posts the same form to the delete route, and the save bar. The name is fixed,
/// because it keys the stored secrets and the proxy path.
fn mcp_server_form(server: &McpServer, pending: Option<&PendingEdit>) -> Markup {
    let action = format!("/settings/mcp/{}", server.name);
    let url = pending.map_or(server.url.as_str(), |pending| pending.url.as_str());
    let headers = pending.map_or(server.headers.as_slice(), |pending| {
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
                            li { (project) ": " code { (header) } }
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
            }))
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
/// Model aliases the stage panel offers. Any other model ID can be typed.
const MODEL_ALIASES: [&str; 4] = ["fable", "opus", "sonnet", "haiku"];

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
    let agent = stage_agent(&project.agent_selection, stage);
    project
        .agents
        .iter()
        .find(|choice| choice.id == agent)
        .map_or(&[], |choice| choice.efforts.as_slice())
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
fn project_form(project: &ProjectSettings, selected: PipelinePart) -> Markup {
    let name = &project.name;
    let href = |part: PipelinePart| {
        format!(
            "/settings/projects/{}?stage={}",
            crate::web::query_escape(name),
            part.key()
        )
    };
    // Each agent's effort levels and default, read by the pipeline script to
    // redraw the effort radios when a stage's agent switches.
    let agent_efforts = serde_json::Value::Object(
        project
            .agents
            .iter()
            .map(|choice| {
                (
                    choice.id.clone(),
                    serde_json::json!({
                        "efforts": choice.efforts,
                        "default": choice.default_effort,
                    }),
                )
            })
            .collect(),
    )
    .to_string();
    html! {
        form.card.settings-form.pipeline-form method="post" action=(href(selected))
            data-agent-efforts=(agent_efforts) {
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
                (stage_panel(project, stage, selected == PipelinePart::Stage(stage)))
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
            datalist #model-aliases {
                @for alias in MODEL_ALIASES { option value=(alias) {} }
            }
            (save_bar(&format!("Save {name}")))
        }
    }
}

fn stage_node(project: &ProjectSettings, stage: &str, selected: bool, href: &str) -> Markup {
    let off = stage == "review" && !project.llm_review;
    let model = stage_model(&project.models, stage);
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
                @if off { "Off" } @else { (model.unwrap_or("CLI default")) }
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

fn stage_panel(project: &ProjectSettings, stage: &str, selected: bool) -> Markup {
    let name = &project.name;
    let model = stage_model(&project.models, stage).unwrap_or_default();
    let effort = stage_effort(&project.efforts, stage);
    let agent = stage_agent(&project.agent_selection, stage);
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
            div.field {
                span.label { "Agent" }
                div.seg role="radiogroup" aria-label={ (stage) " agent" } {
                    @for choice in &project.agents {
                        label.disabled[!choice.available] {
                            input type="radio" name={ (stage) "_agent" } value=(choice.id)
                                checked[choice.id == agent] disabled[!choice.available];
                            span { (choice.display_name) }
                            @if !choice.available {
                                span.tag { "Not available yet" }
                            }
                        }
                    }
                }
                p.hint { "The agent that runs this stage. A run already in flight keeps the agent it started on." }
            }
            div.field {
                label for={ (name) "-" (stage) } { "Model" }
                div.model-chips {
                    button.chip type="button" data-model="" data-model-for=(stage) { "CLI default" }
                    @for alias in MODEL_ALIASES {
                        button.chip type="button" data-model=(alias) data-model-for=(stage) { (alias) }
                    }
                }
                input id={ (name) "-" (stage) } type="text" name=(stage) list="model-aliases"
                    placeholder="CLI default" value=(model) data-model-input=(stage);
                p.hint { "An alias or any model ID. Empty runs the model the installed claude CLI picks." }
            }
            div.field {
                span.label { "Effort" }
                div.seg role="radiogroup" aria-label={ (stage) " effort" } data-effort-seg=(stage) {
                    @for level in stage_efforts(project, stage) {
                        label {
                            input type="radio" name={ (stage) "_effort" } value=(level)
                                checked[level == effort];
                            span { (level) }
                        }
                    }
                }
            }
            div.field {
                span.label { "MCP servers in this stage" }
                @if project.servers.is_empty() {
                    p.hint { "No servers yet. " a href="/settings/mcp" { "Add one" } "." }
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
                        a href=(project_mcp_href(name, &server.name)) { "Key and tools for " (server.name) }
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
                    span.gate-changed hidden[on == default] { "Changed for " (project.name) ". " }
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
fn project_server_block(project: &str, server: &ServerSettings, secrets_available: bool) -> Markup {
    let base = format!("mcp.{}", server.name);
    let stages = server.stages;
    html! {
        div.mcp-project {
            div.field {
                label.check {
                    input type="checkbox" name={ (base) ".enabled" } value="on"
                        checked[server.enabled];
                    " Run this server for " (project)
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
            (mcp_refresh(project, &server.name, &server.tools, None))
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
) -> Region {
    let in_section = |section: Section| -> Vec<&JobRow> {
        rows.iter()
            .copied()
            .filter(|row| Section::of(status(&row.job, auto_merge).lane) == section)
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
                        (job_rows(&jobs[..DONE_SHOWN], costs, auto_merge))
                        details.older data-key=(section.name()) {
                            summary { "Show " (jobs.len() - DONE_SHOWN) " older" }
                            (job_rows(&jobs[DONE_SHOWN..], costs, auto_merge))
                        }
                    } @else {
                        (job_rows(&jobs, costs, auto_merge))
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
        Stage::Failed {
            reason,
            dismissed: false,
            ..
        } => Some(first_line(reason)),
        Stage::Failed { .. } | Stage::Merged | Stage::Cancelled => None,
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
    project: Option<&Project>,
    settings: &Settings,
    logs: &[LogEntry],
    events: &[JobEvent],
    images: &[String],
    files: &[JobFile],
    cost_usd: f64,
    models: &StageModels,
    efforts: &StageEfforts,
    auto_merge: bool,
) -> Markup {
    let job = &row.job;
    let repo = project.map(|project| format!("https://github.com/{}", project.repo));
    let repo_url = repo.as_deref();
    let id = row.id;
    let estimated = any_estimated(events);
    page(
        user,
        &format!("Job #{id}"),
        &html! {},
        &html! {
            (job_poll(false, id, job))
            (job_header(id, row, repo_url, cost_usd, estimated))
            div.job-layout {
                div.job-now {
                    (status_region(false, id, job, settings, auto_merge))
                    p.meta { "Approval gates: " (gates_line(job.gates())) }
                }
                aside.job-side { (timeline_region(false, job, events, models, efforts)) }
                div.job-main {
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
        (cost_badge(true, cost_usd, any_estimated(events)))
        (timeline_region(true, job, events, models, efforts))
        (status_region(true, row.id, job, settings, auto_merge))
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
/// stays put while the figure changes. When any of the job's cost rows is an
/// estimate, such as a Codex run's cost from the price table, the figure carries
/// a `≈` and a title that says it includes an estimate.
fn cost_badge(oob: bool, cost_usd: f64, estimated: bool) -> Markup {
    let estimated = estimated && cost_usd > 0.0;
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
            div.card {
                div.card-head {
                    h2 { "MCP tool calls" }
                    span.log-count {
                        (rows.len()) " " (plural(rows.len(), "call"))
                        @if refused > 0 { " · " (refused) " refused" }
                    }
                }
                ol.mcp-calls {
                    @for (server, calls) in &groups {
                        li { (mcp_call_group(job, server, calls)) }
                    }
                }
            }
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
    repo_url: Option<&str>,
    cost_usd: f64,
    estimated: bool,
) -> Markup {
    let job = &row.job;
    html! {
        div.title-row {
            h1 { "#" (id) " " (first_line(job.request())) }
            (cost_badge(false, cost_usd, estimated))
        }
        p.meta {
            @if let Some(url) = repo_url {
                a href=(url) { (row.project) }
            } @else {
                (row.project)
            }
            " · requested by " (row.requested_by) " · " (crate::localtime::date_time(&row.created_at))
        }
    }
}

fn request_card(id: i64, job: &Job, images: &[String], files: &[JobFile]) -> Markup {
    html! {
        div.card {
            div.card-head {
                h2 { "Request" }
                @if *job.stage() == Stage::Requested {
                    a.nav href={ "/jobs/" (id) "/edit" } { "Edit" }
                }
            }
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
        }
    }
}

/// The page to edit a queued job's request. It posts to `/jobs/{id}/edit` as
/// multipart, the way the new-request form posts. The textarea starts with the
/// current text, each current image and file carries a Remove checkbox that
/// posts its stored name, and `paste-images.js` drives the file input for new
/// uploads. The form works without JavaScript; the checkboxes and file input
/// post on their own.
pub fn edit_request(
    user: &str,
    id: i64,
    job: &Job,
    images: &[String],
    files: &[JobFile],
) -> Markup {
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
                    @for (index, pr) in job.plan().iter().enumerate() {
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
                            @if let Some(notes) = &pr.notes {
                                details.notes data-key={ "plan-notes-" (index) } {
                                    summary { "Notes for the implementer" }
                                    pre.doc { (notes) }
                                }
                            }
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
                            span.log-at { (crate::localtime::time_seconds(&entry.at)) }
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
        ) => failed_card(id, job, reason, *dismissed),
        _ => html! {
            div.card.status {
                (turn_badge(job, auto_merge))
                span.status-detail { (turn_detail(job, settings, auto_merge)) }
            }
        },
    }
}

fn failed_card(id: i64, job: &Job, reason: &str, dismissed: bool) -> Markup {
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

struct Status {
    lane: Lane,
    class: &'static str,
    text: &'static str,
}

/// One match decides the lane and the badge, so they cannot disagree.
fn status(job: &Job, auto_merge: bool) -> Status {
    let (lane, class, text) = match (job.stage(), job.turn()) {
        (Stage::Merged, _) => (Lane::Done, "ok", "merged"),
        (
            Stage::Failed {
                dismissed: true, ..
            },
            _,
        ) => (Lane::Done, "factory", "dismissed"),
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
