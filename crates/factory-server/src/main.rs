use std::env;
use std::error::Error;
use std::path::PathBuf;
use std::sync::Arc;

use factory_runner::{Git, GitHub, WorkerConfig};
use factory_server::{
    CurlUsageSource, GhRepos, PLAN_USAGE_REFRESH, PlanUsageCache, PodmanExecutor, Projects,
    RepoChecker, Scheduler, Store, app, import_tomls, refresh_forever,
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

/// The Claude subscription token the usage fetch needs, from the same
/// `claude-token` secret the workers get. `None` when unset or empty, so the
/// plan usage tile stays off.
fn claude_token() -> Option<String> {
    trimmed_var("CLAUDE_CODE_OAUTH_TOKEN")
}

fn trimmed_var(name: &str) -> Option<String> {
    env::var(name)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
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

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    tracing_subscriber::fmt::init();

    let listen = env::var("FACTORY_LISTEN").unwrap_or_else(|_| "127.0.0.1:8080".into());
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
    let wake = Arc::new(Notify::new());

    if env::var("FACTORY_WORKERS").as_deref() == Ok("podman") {
        let executor = PodmanExecutor {
            base: WorkerConfig {
                podman: path_var("FACTORY_PODMAN", "podman"),
                plugin_dir: env::var_os("FACTORY_PLUGIN_DIR").map(PathBuf::from),
                volumes: env::var("FACTORY_WORKER_VOLUMES")
                    .unwrap_or_default()
                    .split_whitespace()
                    .map(str::to_owned)
                    .collect(),
                ..WorkerConfig::default()
            },
            git: git_config(),
            github: GitHub {
                gh: path_var("FACTORY_GH", "gh"),
                token: gh_token(),
                ..GitHub::default()
            },
            data_dir: data_dir.clone(),
        };
        let scheduler = Scheduler::new(
            Arc::clone(&store),
            Arc::clone(&projects),
            Arc::new(executor),
            Arc::clone(&wake),
            data_dir.clone(),
        );
        tokio::spawn(scheduler.run());
        tracing::info!("running jobs in podman workers");
    } else {
        tracing::info!("workers are off, set FACTORY_WORKERS=podman to run jobs");
    }

    let plan_usage = Arc::new(PlanUsageCache::new());
    if let Some(token) = claude_token() {
        let source = CurlUsageSource {
            curl: path_var("FACTORY_CURL", "curl"),
            token,
        };
        tokio::spawn(refresh_forever(source, Arc::clone(&plan_usage)));
        tracing::info!("refreshing Claude plan usage every {PLAN_USAGE_REFRESH:?}");
    } else {
        tracing::info!("no Claude token, the plan usage tile is off");
    }

    // The onboarding wizard runs its repository checks as the bot, with the
    // same `gh` and token the factory uses for pull requests.
    let github: Arc<dyn RepoChecker> = Arc::new(GhRepos {
        gh: path_var("FACTORY_GH", "gh"),
        token: gh_token(),
    });

    let listener = tokio::net::TcpListener::bind(&listen).await?;
    tracing::info!("listening on http://{listen}, database {}", db.display());
    axum::serve(
        listener,
        app(store, projects, wake, data_dir, plan_usage, github),
    )
    .with_graceful_shutdown(async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await?;
    Ok(())
}
