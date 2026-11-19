use std::env;
use std::error::Error;
use std::path::PathBuf;
use std::sync::Arc;

use factory_server::{Projects, Store, app};

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    tracing_subscriber::fmt::init();

    let listen = env::var("FACTORY_LISTEN").unwrap_or_else(|_| "127.0.0.1:8080".into());
    let db = env::var_os("FACTORY_DB").map_or_else(|| PathBuf::from("factory.db"), PathBuf::from);
    let projects_dir =
        env::var_os("FACTORY_PROJECTS").map_or_else(|| PathBuf::from("projects"), PathBuf::from);

    let projects = Projects::load(&projects_dir)?;
    let store = Store::open(&db)?;
    let listener = tokio::net::TcpListener::bind(&listen).await?;
    tracing::info!("listening on http://{listen}, database {}", db.display());

    axum::serve(listener, app(Arc::new(store), Arc::new(projects)))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
