//! The one-time copy of `GH_TOKEN` into the project tokens at startup.

mod support;

use std::io::Write;
use std::sync::{Arc, Mutex};

use factory_runner::{ForgeAccount, ForgeKind, GitHub};
use factory_server::forge_tokens::{LOOKUP_TIMEOUT, copy_gh_token};
use factory_server::{ForgeTokenOwner, LocalSecrets, MASTER_KEY_LEN, SecretStore, Store};
use tracing_subscriber::fmt::MakeWriter;

const TOKEN: &str = "seed-token-value";

#[derive(Clone, Default)]
struct Logs(Arc<Mutex<Vec<u8>>>);

impl Write for Logs {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Logs {
    type Writer = Logs;

    fn make_writer(&'a self) -> Logs {
        self.clone()
    }
}

impl Logs {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}

struct Fixture {
    dir: tempfile::TempDir,
    store: Arc<Store>,
    secrets: LocalSecrets,
    forge: GitHub,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let gh = support::executable(
        dir.path(),
        "gh",
        &format!(
            r#"#!/usr/bin/env bash
if [ "$1 $2 $3 $4" = "api user --hostname github.com" ] && [ "$GH_TOKEN" = "{TOKEN}" ]; then
  echo $$ >> "$(dirname "$0")/calls"
  if [ -e "$(dirname "$0")/stall" ]; then
    exec sleep 30
  fi
  if [ -e "$(dirname "$0")/down" ]; then
    echo "gh: boom, token $GH_TOKEN (HTTP 500)" >&2
    exit 1
  fi
  echo '{{"login":"moonthebot","id":7}}'
  exit 0
fi
echo "fake gh: unexpected: $*" >&2
exit 1
"#
        ),
    );
    let store = Arc::new(Store::open_in_memory().unwrap());
    let secrets = LocalSecrets::new(&[7; MASTER_KEY_LEN], Arc::clone(&store));
    Fixture {
        dir,
        store,
        secrets,
        forge: GitHub {
            gh,
            ..GitHub::default()
        },
    }
}

impl Fixture {
    fn project(&self, name: &str, by: Option<&str>) {
        self.store
            .create_project(
                name,
                ForgeKind::GitHub,
                &format!("Badger-Embedded/{name}"),
                "main",
                None,
                by,
            )
            .unwrap();
    }

    async fn copy(&self) -> factory_server::forge_tokens::CopyReport {
        copy_gh_token(
            &self.store,
            &self.secrets,
            &self.forge,
            TOKEN,
            LOOKUP_TIMEOUT,
        )
        .await
    }

    fn token(&self, name: &str) -> Option<String> {
        self.secrets
            .forge_token(owner(name))
            .expect("read project token")
    }
}

fn owner(name: &str) -> ForgeTokenOwner<'_> {
    ForgeTokenOwner::Project {
        project: name,
        forge: ForgeKind::GitHub,
    }
}

#[tokio::test]
async fn copies_into_each_project_once_and_keeps_a_clear() {
    let fx = fixture();
    fx.project("alpha", None);
    fx.project("beta", None);
    let logs = Logs::default();
    let _guard = tracing::subscriber::set_default(
        tracing_subscriber::fmt().with_writer(logs.clone()).finish(),
    );

    let report = fx.copy().await;

    let account = ForgeAccount {
        login: "moonthebot".into(),
        id: 7,
    };
    assert_eq!(
        report.copied,
        vec![
            ("alpha".into(), account.clone()),
            ("beta".into(), account.clone())
        ]
    );
    for name in ["alpha", "beta"] {
        assert_eq!(fx.token(name).as_deref(), Some(TOKEN));
        assert_eq!(
            fx.store.forge_account(owner(name)).unwrap(),
            Some(account.clone())
        );
    }
    let text = logs.text();
    assert!(text.contains("alpha") && text.contains("moonthebot"));
    assert!(!text.contains(TOKEN));

    fx.secrets.delete_forge_token(owner("alpha")).unwrap();
    let again = fx.copy().await;
    assert!(again.copied.is_empty() && again.failed.is_empty());
    assert_eq!(fx.token("alpha"), None);
    assert_eq!(fx.token("beta").as_deref(), Some(TOKEN));
}

#[tokio::test]
async fn failed_lookup_stores_nothing_warns_without_the_token_and_is_retried() {
    let fx = fixture();
    fx.project("alpha", None);
    std::fs::write(fx.dir.path().join("down"), "").unwrap();
    let logs = Logs::default();
    let _guard = tracing::subscriber::set_default(
        tracing_subscriber::fmt().with_writer(logs.clone()).finish(),
    );

    let report = fx.copy().await;

    assert_eq!(report.failed, vec!["alpha".to_owned()]);
    assert_eq!(report.copied.len(), 0);
    assert_eq!(fx.token("alpha"), None);
    let text = logs.text();
    assert!(text.contains("alpha") && text.contains("WARN"));
    assert!(!text.contains(TOKEN));

    std::fs::remove_file(fx.dir.path().join("down")).unwrap();
    let retry = fx.copy().await;
    assert_eq!(retry.copied.len(), 1);
    assert_eq!(fx.token("alpha").as_deref(), Some(TOKEN));
}

#[tokio::test]
async fn leaves_onboarded_projects_and_existing_tokens_alone() {
    let fx = fixture();
    fx.project("imported", None);
    fx.project("onboarded", Some("alice"));
    fx.project("held", None);
    let theirs = ForgeAccount {
        login: "theirs".into(),
        id: 9,
    };
    fx.secrets
        .put_forge_token(owner("held"), "their-token", &theirs)
        .unwrap();

    let report = fx.copy().await;

    assert_eq!(report.copied.len(), 1);
    assert_eq!(fx.token("imported").as_deref(), Some(TOKEN));
    assert_eq!(fx.token("onboarded"), None);
    assert_eq!(fx.token("held").as_deref(), Some("their-token"));

    fx.secrets.delete_forge_token(owner("held")).unwrap();
    fx.copy().await;
    assert_eq!(fx.token("held"), None);

    fx.store.remove_project("imported").unwrap();
    assert!(!fx.store.gh_token_copy_done("imported").unwrap());
    fx.project("imported", Some("alice"));
    fx.copy().await;
    assert_eq!(fx.token("imported"), None);
}

#[tokio::test]
async fn copies_nothing_without_the_secret_store() {
    let fx = fixture();
    fx.project("alpha", None);
    let unavailable = factory_server::UnavailableSecrets::new(Arc::clone(&fx.store));
    let logs = Logs::default();
    let _guard = tracing::subscriber::set_default(
        tracing_subscriber::fmt().with_writer(logs.clone()).finish(),
    );

    let report = copy_gh_token(&fx.store, &unavailable, &fx.forge, TOKEN, LOOKUP_TIMEOUT).await;

    assert!(report.copied.is_empty() && report.failed.is_empty());
    assert!(!fx.store.gh_token_copy_done("alpha").unwrap());
    assert_eq!(logs.text().matches("WARN").count(), 1);
}

#[tokio::test]
async fn a_stalled_lookup_times_out_once_and_leaves_projects_for_retry() {
    let fx = fixture();
    fx.project("alpha", None);
    fx.project("beta", None);
    std::fs::write(fx.dir.path().join("stall"), "").unwrap();
    let started = std::time::Instant::now();

    let report = copy_gh_token(
        &fx.store,
        &fx.secrets,
        &fx.forge,
        TOKEN,
        std::time::Duration::from_millis(300),
    )
    .await;

    assert!(started.elapsed() < std::time::Duration::from_secs(5));
    let calls = std::fs::read_to_string(fx.dir.path().join("calls")).unwrap();
    let pids: Vec<&str> = calls.lines().collect();
    assert_eq!(pids.len(), 1, "one lookup for both projects");
    let stat = format!("/proc/{}/stat", pids[0]);
    let mut gone = false;
    for _ in 0..100 {
        gone = std::fs::read_to_string(&stat).map_or(true, |text| text.contains(") Z "));
        if gone {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(gone, "the stalled gh must be killed after the timeout");
    assert_eq!(report.failed, vec!["alpha".to_owned(), "beta".to_owned()]);
    assert_eq!(fx.token("alpha"), None);
    assert!(!fx.store.gh_token_copy_done("alpha").unwrap());

    std::fs::remove_file(fx.dir.path().join("stall")).unwrap();
    assert_eq!(fx.copy().await.copied.len(), 2);
}

#[tokio::test]
async fn a_token_cleared_before_the_first_pass_is_not_refilled() {
    let fx = fixture();
    fx.project("alpha", None);
    // A token saved before this change has no marker.
    let scratch = Arc::new(Store::open_in_memory().unwrap());
    let other = LocalSecrets::new(&[7; MASTER_KEY_LEN], Arc::clone(&scratch));
    let theirs = ForgeAccount {
        login: "theirs".into(),
        id: 9,
    };
    other
        .put_forge_token(owner("alpha"), "their-token", &theirs)
        .unwrap();
    let (backend, sealed) = scratch
        .get_secret(factory_server::SecretRef {
            project: "alpha",
            server: "",
            header: "forge-project",
        })
        .unwrap()
        .unwrap();
    fx.store
        .put_secret(
            factory_server::SecretRef {
                project: "alpha",
                server: "",
                header: "forge-project",
            },
            &backend,
            &sealed,
        )
        .unwrap();
    assert_eq!(fx.token("alpha").as_deref(), Some("their-token"));
    assert!(!fx.store.gh_token_copy_done("alpha").unwrap());

    factory_server::UnavailableSecrets::new(Arc::clone(&fx.store))
        .delete_forge_token(owner("alpha"))
        .unwrap();
    let report = fx.copy().await;

    assert_eq!(report.copied.len(), 0);
    assert_eq!(fx.token("alpha"), None);
}
