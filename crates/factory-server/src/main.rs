use std::env;
use std::error::Error;
use std::path::PathBuf;
use std::sync::Arc;

use factory_runner::{Git, GitHub, WorkerConfig};
use factory_server::{
    AgentRegistry, ClaudeToken, CodexLogin, GhRepos, HttpMcpChecker, LocalSecrets, McpChecker,
    McpRuntime, McpWarnings, PlanUsageCache, PodmanExecutor, Projects, ProxyEvent, ProxyEventFn,
    RepoChecker, RunTokens, Scheduler, SecretStore, Store, StoreUpstreams, UnavailableSecrets,
    Upstreams, WorkerSlots, app, import_tomls, master_key_from_env, proxy_app,
};
use tokio::sync::Notify;

fn path_var(name: &str, default: &str) -> PathBuf {
    env::var_os(name).map_or_else(|| PathBuf::from(default), PathBuf::from)
}

/// `GH_TOKEN` without surrounding whitespace, or `None` when unset or
/// empty. A token read from a file or a secret often ends in a newline.
fn gh_token() -> Option<String> {
    trimmed_var("GH_TOKEN")
}

fn trimmed_var(name: &str) -> Option<String> {
    env::var(name)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

/// Starts the MCP proxy on its own listener and returns the run-token registry
/// it honours. The proxy keeps a server credential out of a worker: a worker
/// reaches a server through it with a per-run token, and the proxy swaps the
/// token for the project's stored headers. A proxy-side failure to reach a
/// server logs a warning.
async fn start_mcp_proxy(
    listen: &str,
    store: Arc<Store>,
    secrets: Arc<dyn SecretStore>,
    warnings: Arc<McpWarnings>,
) -> Result<Arc<RunTokens>, Box<dyn Error>> {
    let run_tokens = Arc::new(RunTokens::new());
    let upstreams: Arc<dyn Upstreams> = Arc::new(StoreUpstreams::new(store, secrets));
    // A proxy-side failure to reach a server records a warning on the job, the
    // same path the runner's init-line check uses, deduped so a server is warned
    // about at most once per run.
    let report: ProxyEventFn = Arc::new(move |event: ProxyEvent| match event {
        ProxyEvent::Unavailable {
            job,
            server,
            reason,
        } => warnings.record(job, &server, &reason),
        ProxyEvent::ToolBlocked { job, server, tool } => {
            warnings.record_tool_blocked(job, &server, &tool);
        }
        ProxyEvent::ToolCalled { job, server, tool } => {
            warnings.record_tool_called(job, &server, &tool);
        }
    });
    let listener = tokio::net::TcpListener::bind(listen).await?;
    tracing::info!("MCP proxy listening on http://{listen}");
    let app = proxy_app(Arc::clone(&run_tokens), upstreams, report);
    tokio::spawn(async move {
        if let Err(err) = axum::serve(listener, app).await {
            tracing::error!("the MCP proxy stopped: {err}");
        }
    });
    Ok(run_tokens)
}

fn git_config() -> Git {
    let default = Git::default();
    Git {
        token: gh_token(),
        user_name: env::var("FACTORY_GIT_NAME").unwrap_or(default.user_name.clone()),
        user_email: env::var("FACTORY_GIT_EMAIL").unwrap_or(default.user_email.clone()),
        ..default
    }
}

fn worker_base(network: Option<String>) -> WorkerConfig {
    WorkerConfig {
        podman: path_var("FACTORY_PODMAN", "podman"),
        plugin_dir: env::var_os("FACTORY_PLUGIN_DIR").map(PathBuf::from),
        volumes: env::var("FACTORY_WORKER_VOLUMES")
            .unwrap_or_default()
            .split_whitespace()
            .map(str::to_owned)
            .collect(),
        network,
        ..WorkerConfig::default()
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    tracing_subscriber::fmt::init();

    let listen = env::var("FACTORY_LISTEN").unwrap_or_else(|_| "127.0.0.1:8080".into());
    let proxy_listen = env::var("FACTORY_PROXY_LISTEN").unwrap_or_else(|_| "127.0.0.1:8081".into());
    // The base URL a worker reaches the proxy at, over the shared podman
    // network. The container name resolves to the factory on that network.
    let proxy_url = env::var("FACTORY_PROXY_URL").unwrap_or_else(|_| "http://factory:8081".into());
    // The podman network every worker joins, so it can reach the proxy but not
    // the dashboard, which is published to the host loopback alone. Empty leaves
    // podman's default network, for a local run with no proxy.
    let worker_network = trimmed_var("FACTORY_WORKER_NETWORK");
    let data_dir = path_var("FACTORY_DATA", "data");
    let db = env::var_os("FACTORY_DB").map_or_else(|| data_dir.join("factory.db"), PathBuf::from);

    std::fs::create_dir_all(&data_dir)?;
    let store = Arc::new(Store::open(&db)?);
    // The one-time move from `project.toml` files to the `projects` table. It
    // runs only on the first start of a database with no projects, then deletes
    // the files it imported.
    import_tomls(&store, &path_var("FACTORY_IMPORT_PROJECTS", "projects"))?;
    let projects = Arc::new(Projects::from_store(Arc::clone(&store)));
    let names = projects.names()?;
    store.migrate_global_auto_merge(&names.iter().map(String::as_str).collect::<Vec<_>>())?;
    store.migrate_project_gates()?;
    let wake = Arc::new(Notify::new());

    // Per-project MCP header values are encrypted under the `factory-master-key`
    // secret. A missing or malformed key leaves the secret store unavailable
    // rather than stopping the factory, so the dashboard still runs, reports
    // secrets as off and refuses to save a header value.
    let secrets: Arc<dyn SecretStore> = if let Some(key) = master_key_from_env() {
        tracing::info!("MCP secret store is ready");
        Arc::new(LocalSecrets::new(&key, Arc::clone(&store)))
    } else {
        tracing::warn!("factory-master-key is missing or invalid, MCP header secrets are off");
        Arc::new(UnavailableSecrets::new(Arc::clone(&store)))
    };

    // The Claude token comes from the secret store when it is available, and
    // from `CLAUDE_CODE_OAUTH_TOKEN` when it is not. The one-time import moves
    // the env token into the store on the first start that finds the store
    // empty, so the podman secret needs no manual step. Once a token is stored
    // the env var is ignored.
    let claude_token = Arc::new(ClaudeToken::new(
        Arc::clone(&secrets),
        trimmed_var("CLAUDE_CODE_OAUTH_TOKEN"),
    ));
    claude_token.import_env_token();

    // The Codex login is the account-wide `auth.json` in the secret store. The
    // coordinator resolves it for a run, serialises runs while it is due for a
    // refresh, and keeps the file Codex rewrites mid-run.
    let codex_login = Arc::new(CodexLogin::new(Arc::clone(&secrets)));

    // The registry maps each agent to its credential, worker image and config
    // directory. The executor runs agents through it; the plan usage fetch keeps
    // its own handle on the Claude token.
    let agents = Arc::new(AgentRegistry::new(
        Arc::clone(&claude_token),
        Arc::clone(&codex_login),
    ));

    // An unreachable MCP server does not fail a run; it records a warning on the
    // job. The proxy and the scheduler share this recorder so a server is warned
    // about at most once per run.
    let mcp_warnings = Arc::new(McpWarnings::new(Arc::clone(&store)));

    // The MCP proxy stands between a worker and a remote MCP server, so a
    // credential never enters a worker. It listens on its own port, separate
    // from the dashboard, and honours per-run tokens held only in memory. No
    // token exists until a run issues one, so every request before that is 401.
    let run_tokens = start_mcp_proxy(
        &proxy_listen,
        Arc::clone(&store),
        Arc::clone(&secrets),
        Arc::clone(&mcp_warnings),
    )
    .await?;

    // Without podman workers no job takes a slot, so the board shows every
    // slot idle.
    let mut slots = Arc::new(WorkerSlots::new());
    let plan_usage = Arc::new(PlanUsageCache::new());
    if env::var("FACTORY_WORKERS").as_deref() == Ok("podman") {
        let executor = PodmanExecutor {
            base: worker_base(worker_network.clone()),
            git: git_config(),
            github: GitHub {
                gh: path_var("FACTORY_GH", "gh"),
                token: gh_token(),
                ..GitHub::default()
            },
            data_dir: data_dir.clone(),
            registry: Arc::clone(&agents),
            store: Arc::clone(&store),
            plan_usage: Arc::clone(&plan_usage),
        };
        let scheduler = Scheduler::new(
            Arc::clone(&store),
            Arc::clone(&projects),
            Arc::new(executor),
            Arc::clone(&wake),
            data_dir.clone(),
            McpRuntime {
                tokens: Arc::clone(&run_tokens),
                secrets: Arc::clone(&secrets),
                proxy_url: proxy_url.clone(),
                warnings: Arc::clone(&mcp_warnings),
            },
        );
        slots = scheduler.slots();
        tokio::spawn(scheduler.run());
        tracing::info!("running jobs in podman workers");
    } else {
        tracing::info!("workers are off, set FACTORY_WORKERS=podman to run jobs");
    }

    // The onboarding wizard runs its repository checks as the bot, with the
    // same `gh` and token the factory uses for pull requests.
    let github: Arc<dyn RepoChecker> = Arc::new(GhRepos {
        gh: path_var("FACTORY_GH", "gh"),
        token: gh_token(),
    });

    // The per-project MCP connection check uses a real HTTP client that follows
    // no redirect and bounds the wait, the same way the proxy's client does.
    let checker: Arc<dyn McpChecker> = Arc::new(HttpMcpChecker::new());

    let listener = tokio::net::TcpListener::bind(&listen).await?;
    tracing::info!("listening on http://{listen}, database {}", db.display());
    axum::serve(
        listener,
        app(
            store, projects, wake, data_dir, plan_usage, github, slots, secrets, checker, agents,
        ),
    )
    .with_graceful_shutdown(async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await?;
    Ok(())
}
