//! Runs the real `factory-server` binary to check `FACTORY_MCP_CA_FILE`: the
//! dashboard's connection check trusts a server signed by the CA in the file,
//! and an invalid file stops the start.

mod support;

use std::net::TcpListener;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use axum::Router;
use axum::http::header;
use axum::routing::any;
use factory_server::Store;

const INIT_JSON: &str = r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18","serverInfo":{"name":"stub","version":"1.0"}}}"#;

async fn initialize() -> ([(header::HeaderName, &'static str); 1], &'static str) {
    ([(header::CONTENT_TYPE, "application/json")], INIT_JSON)
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn factory(dir: &Path, ca_file: &Path, dashboard: u16) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_factory-server"));
    command
        .env("FACTORY_LISTEN", format!("127.0.0.1:{dashboard}"))
        .env("FACTORY_PROXY_LISTEN", format!("127.0.0.1:{}", free_port()))
        .env("FACTORY_DATA", dir.join("data"))
        .env("FACTORY_DB", dir.join("factory.db"))
        .env("FACTORY_IMPORT_PROJECTS", dir.join("projects"))
        .env("FACTORY_MCP_CA_FILE", ca_file)
        .env_remove("FACTORY_WORKERS")
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    command
}

struct Running(Child);

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_dashboard_check_trusts_the_ca_in_the_file() {
    let tls = support::serve_tls(Router::new().route("/mcp", any(initialize))).await;
    let dir = tempfile::tempdir().unwrap();
    let ca_file = dir.path().join("ca.pem");
    std::fs::write(&ca_file, &tls.ca_pem).unwrap();

    let store = Store::open(&dir.path().join("factory.db")).unwrap();
    store
        .create_project("demo", "o/demo", "main", None, Some("local"))
        .unwrap();
    store
        .create_mcp_server("stub", &format!("{}/mcp", tls.base), &[])
        .unwrap();
    drop(store);

    let port = free_port();
    let mut command = factory(dir.path(), &ca_file, port);
    let _server = Running(command.spawn().unwrap());

    let client = reqwest::Client::new();
    let url = format!("http://127.0.0.1:{port}/settings/projects/demo/mcp/stub/check");
    let mut body = String::new();
    for _ in 0..100 {
        if let Ok(response) = client
            .post(&url)
            .header("Tailscale-User-Login", "local")
            .send()
            .await
        {
            body = response.text().await.unwrap();
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(body.contains("Connected: stub"), "{body}");
}

#[test]
fn an_invalid_file_stops_the_start_with_an_error_naming_it() {
    let dir = tempfile::tempdir().unwrap();
    let ca_file = dir.path().join("ca.pem");
    std::fs::write(&ca_file, "not a certificate").unwrap();

    let output = factory(dir.path(), &ca_file, free_port()).output().unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("FACTORY_MCP_CA_FILE"), "{stderr}");

    let missing = dir.path().join("missing.pem");
    let output = factory(dir.path(), &missing, free_port()).output().unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("cannot read"), "{stderr}");
}
