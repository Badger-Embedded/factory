//! Tests for the per-agent device login: a `LoginSpec` other than Codex's has
//! its own container name, preparation, read-back, validation and storage, and
//! `PodmanLogin` runs the spec's command with its environment variable and mount.

use std::future::Future;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use factory_runner::WorkerConfig;
use factory_server::{
    DeviceLogin, LocalSecrets, LoginProcess, LoginSpawner, LoginSpec, LoginView, MASTER_KEY_LEN,
    PodmanLogin, ReadBackError, SecretStore, Store,
};

fn spec() -> LoginSpec {
    LoginSpec {
        container_name: "factory-test-login",
        scratch_prefix: "test-login-",
        command: &["tool", "login", "--code"],
        home_env: "TOOL_HOME",
        home_mount: "/tool",
        prepare: |home| std::fs::write(home.join("prepared"), "yes"),
        read_back: |home| {
            std::fs::read_to_string(home.join("token.txt")).map_err(|error| {
                ReadBackError::Unreadable {
                    file: "token.txt",
                    error,
                }
            })
        },
        validate: |credential| {
            if credential.starts_with("gho_") {
                Ok(())
            } else {
                Err("the test validator wants gho_".to_owned())
            }
        },
        store: |secrets, credential| secrets.set_copilot_token(credential),
        timeout_reason: "the test login timed out",
    }
}

struct Seen {
    name: String,
    prepared: bool,
}

struct CredentialSpawner {
    credential: String,
    seen: Arc<Mutex<Vec<Seen>>>,
}

struct Writer {
    home: PathBuf,
    credential: String,
}

impl LoginSpawner for CredentialSpawner {
    fn spawn(
        &self,
        name: String,
        _image: String,
        home: PathBuf,
    ) -> Pin<Box<dyn Future<Output = std::io::Result<Box<dyn LoginProcess>>> + Send + '_>> {
        self.seen.lock().unwrap().push(Seen {
            name,
            prepared: home.join("prepared").exists(),
        });
        let credential = self.credential.clone();
        Box::pin(async move { Ok(Box::new(Writer { home, credential }) as Box<dyn LoginProcess>) })
    }
}

impl LoginProcess for Writer {
    fn next_line(&mut self) -> Pin<Box<dyn Future<Output = Option<String>> + Send + '_>> {
        std::fs::write(self.home.join("token.txt"), &self.credential).unwrap();
        Box::pin(std::future::ready(None))
    }

    fn wait(&mut self) -> Pin<Box<dyn Future<Output = bool> + Send + '_>> {
        Box::pin(std::future::ready(true))
    }

    fn kill(&mut self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(std::future::ready(()))
    }
}

async fn run(
    credential: &str,
) -> (
    LoginView,
    Vec<Seen>,
    Arc<dyn SecretStore>,
    tempfile::TempDir,
) {
    let store = Arc::new(Store::open_in_memory().unwrap());
    let secrets: Arc<dyn SecretStore> = Arc::new(LocalSecrets::new(&[7; MASTER_KEY_LEN], store));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let spawner = Arc::new(CredentialSpawner {
        credential: credential.to_owned(),
        seen: Arc::clone(&seen),
    });
    let data = tempfile::tempdir().unwrap();
    let login = DeviceLogin::for_spec(
        spec(),
        Arc::clone(&secrets),
        spawner,
        data.path().join("scratch"),
    );
    login.start("image".to_owned()).await;
    let mut view = login.snapshot().await;
    for _ in 0..200 {
        if !view.active() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
        view = login.snapshot().await;
    }
    let seen = std::mem::take(&mut *seen.lock().unwrap());
    (view, seen, secrets, data)
}

#[tokio::test]
async fn a_custom_spec_prepares_reads_back_validates_and_stores() {
    let (view, seen, secrets, data) = run("gho_abcdefghijklmnopqrstuvwxyz0123456789").await;
    assert_eq!(view, LoginView::SignedIn);
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].name, "factory-test-login");
    assert!(seen[0].prepared, "prepare ran before the container started");
    assert_eq!(
        secrets.copilot_token().unwrap().as_deref(),
        Some("gho_abcdefghijklmnopqrstuvwxyz0123456789")
    );
    assert!(secrets.codex_auth().unwrap().is_none());
    let left = std::fs::read_dir(data.path().join("scratch"))
        .unwrap()
        .count();
    assert_eq!(left, 0);
}

#[tokio::test]
async fn a_custom_spec_refuses_a_credential_its_validator_rejects() {
    let (view, _seen, secrets, _data) = run("ghp_classic").await;
    let LoginView::NotStored { reason } = view else {
        panic!("{view:?}");
    };
    assert!(reason.contains("the test validator wants gho_"), "{reason}");
    assert!(!reason.contains("ghp_classic"), "{reason}");
    assert!(secrets.copilot_token().unwrap().is_none());
}

fn fake_podman(dir: &Path, log: &Path) -> PathBuf {
    let path = dir.join("podman");
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\nfor a in \"$@\"; do echo \"$a\" >> {log}; done\necho >> {log}\n",
            log = log.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[tokio::test]
async fn podman_login_runs_the_specs_command_env_and_mount() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("calls.log");
    let config = WorkerConfig {
        podman: fake_podman(dir.path(), &log),
        ..WorkerConfig::default()
    };
    let spawner = PodmanLogin::for_spec(spec(), config);
    let mut process = spawner
        .spawn(
            "factory-test-login".to_owned(),
            "the-image".to_owned(),
            PathBuf::from("/host/home"),
        )
        .await
        .unwrap();
    assert!(process.wait().await);

    let calls = std::fs::read_to_string(&log).unwrap();
    let run: Vec<&str> = calls
        .split("\n\n")
        .map(|call| call.lines().collect::<Vec<_>>())
        .find(|args| args.first() == Some(&"run"))
        .map(|args| args.into_iter().collect())
        .expect("a podman run call");
    let joined = run.join(" ");
    assert!(joined.contains("--name factory-test-login"), "{joined}");
    assert!(joined.contains("--env TOOL_HOME=/tool"), "{joined}");
    assert!(joined.contains("--volume /host/home:/tool"), "{joined}");
    assert!(joined.ends_with("the-image tool login --code"), "{joined}");
    assert!(
        !joined.contains("CODEX_HOME") && !joined.contains("codex"),
        "{joined}"
    );
}
