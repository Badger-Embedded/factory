//! The registry of Forgejo servers in the store: what a name and a URL must
//! look like, which instances can be deleted, and what a delete removes.

use factory_server::{ForgejoInstance, SecretRef, Store, StoreError};
use rusqlite::Connection;

fn refusal(result: Result<impl Sized, StoreError>) -> String {
    match result {
        Err(StoreError::Forgejo(message)) => message,
        Err(err) => panic!("expected a Forgejo refusal, got {err}"),
        Ok(_) => panic!("expected a Forgejo refusal"),
    }
}

fn store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("factory.db")).unwrap();
    (dir, store)
}

fn raw(dir: &tempfile::TempDir) -> Connection {
    Connection::open(dir.path().join("factory.db")).unwrap()
}

fn secret<'a>(project: &'a str, header: &'a str) -> SecretRef<'a> {
    SecretRef {
        project,
        server: "",
        header,
    }
}

#[test]
fn a_valid_name_and_url_are_stored_in_normal_form() {
    for (url, stored) in [
        ("https://git.example.org/", "https://git.example.org"),
        (
            "https://Git.Example.org:8443",
            "https://git.example.org:8443",
        ),
        (
            "https://example.org/forgejo/",
            "https://example.org/forgejo",
        ),
        ("https://git.example.org:443", "https://git.example.org"),
    ] {
        assert_eq!(
            ForgejoInstance::validate("mine", url),
            Ok(("mine".to_owned(), stored.to_owned())),
            "{url}"
        );
    }
}

#[test]
fn a_bad_name_is_refused() {
    for name in [
        "",
        "Mine",
        "my_forge",
        "my forge",
        "a/b",
        "github",
        "salsa",
        &"x".repeat(33),
    ] {
        assert!(
            ForgejoInstance::validate(name, "https://git.example.org").is_err(),
            "{name:?}"
        );
    }
    assert!(ForgejoInstance::validate(&"x".repeat(32), "https://git.example.org").is_ok());
}

#[test]
fn a_bad_url_is_refused() {
    for url in [
        "",
        "not a url",
        "http://git.example.org",
        "ssh://git.example.org",
        "https://",
        "https://user@git.example.org",
        "https://@git.example.org",
        "https://\t/@git.example.org",
        "https:git.example.org",
        "https:///@git.example.org",
        "https://:@git.example.org",
        "https://user:pass@git.example.org",
        "https://git.example.org?a=b",
        "https://git.example.org/?",
        "https://git.example.org#top",
    ] {
        assert!(ForgejoInstance::validate("mine", url).is_err(), "{url:?}");
    }
}

#[test]
fn a_name_or_url_that_is_taken_is_refused_and_nothing_is_stored() {
    let (_dir, store) = store();
    store
        .add_forgejo_instance("mine", "https://git.example.org", "17.0.2")
        .unwrap();
    assert!(
        refusal(store.add_forgejo_instance("mine", "https://other.example.org", "17.0.2"))
            .contains("already exists")
    );
    assert!(
        refusal(store.add_forgejo_instance("other", "https://git.example.org/", "17.0.2"))
            .contains("already registered")
    );
    assert!(
        store
            .add_forgejo_instance("github", "https://x.example.org", "17.0.2")
            .is_err()
    );
    let names: Vec<String> = store
        .forgejo_instances()
        .unwrap()
        .into_iter()
        .map(|instance| instance.name)
        .collect();
    assert_eq!(names, ["mine"]);
}

#[test]
fn instances_are_listed_by_name_with_the_version_read() {
    let (_dir, store) = store();
    store
        .add_forgejo_instance("zeta", "https://z.example.org", "16.1.0")
        .unwrap();
    store
        .add_forgejo_instance("alpha", "https://a.example.org/forgejo", "17.0.2")
        .unwrap();
    let listed = store.forgejo_instances().unwrap();
    assert_eq!(listed[0].name, "alpha");
    assert_eq!(listed[0].base_url, "https://a.example.org/forgejo");
    assert_eq!(listed[0].version, "17.0.2");
    assert_eq!(listed[1].version, "16.1.0");
    assert!(store.forgejo_instance("zeta").unwrap().is_some());
    assert!(store.forgejo_instance("missing").unwrap().is_none());
}

#[test]
fn old_versions_warn_about_what_they_cannot_do() {
    let instance = |version: &str| ForgejoInstance {
        name: "mine".into(),
        base_url: "https://git.example.org".into(),
        version: version.into(),
        added_at: String::new(),
        registration: 1,
    };
    assert_eq!(
        instance("17.0.0-121-39320c0+gitea-1.22.0").warnings(),
        Vec::<&str>::new()
    );
    let below_17 = instance("16.2.0+gitea-1.22.0").warnings();
    assert_eq!(below_17.len(), 1);
    assert!(below_17[0].contains("rerun"));
    let below_16 = instance("15.0.0+gitea-1.22.0").warnings();
    assert_eq!(below_16.len(), 2);
    assert!(below_16[1].contains("logs"));
    assert_eq!(instance("unknown").warnings(), Vec::<&str>::new());
}

#[test]
fn an_instance_a_project_uses_cannot_be_deleted_and_the_refusal_names_the_projects() {
    let (dir, store) = store();
    store
        .add_forgejo_instance("mine", "https://git.example.org", "17.0.2")
        .unwrap();
    let conn = raw(&dir);
    conn.execute(
        "INSERT INTO projects (name, forge, repo, base) VALUES ('web', 'mine', 'o/web', 'main')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO projects (name, forge, repo, base) VALUES ('pkg', 'github', 'o/pkg', 'main')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO project_maintenance (project, upstream_forge) VALUES ('pkg', 'mine')",
        [],
    )
    .unwrap();
    let message = refusal(store.delete_forgejo_instance("mine"));
    assert!(message.contains("pkg, web"), "{message}");
    assert!(store.forgejo_instance("mine").unwrap().is_some());

    conn.execute("DELETE FROM projects WHERE name = 'web'", [])
        .unwrap();
    assert!(refusal(store.delete_forgejo_instance("mine")).contains("pkg"));
    conn.execute("DELETE FROM project_maintenance", []).unwrap();
    store.delete_forgejo_instance("mine").unwrap();
    assert!(store.forgejo_instance("mine").unwrap().is_none());
}

#[test]
fn deleting_an_unknown_instance_removes_nothing() {
    let (dir, store) = store();
    let conn = raw(&dir);
    conn.execute(
        "INSERT INTO user_forge_accounts (login, forge, account_login, account_id)
         VALUES ('alice', 'github', 'alice-gh', 1)",
        [],
    )
    .unwrap();
    store
        .put_secret(secret("", "forge-user:github:alice"), "local", b"sealed")
        .unwrap();
    assert!(refusal(store.delete_forgejo_instance("github")).contains("No instance"));
    let accounts: i64 = conn
        .query_row("SELECT COUNT(*) FROM user_forge_accounts", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(accounts, 1);
    assert!(
        store
            .get_secret(secret("", "forge-user:github:alice"))
            .unwrap()
            .is_some()
    );
}

#[test]
fn a_delete_removes_the_tokens_of_that_instance_and_no_others() {
    let (dir, store) = store();
    for (name, url) in [
        ("mine", "https://a.example.org"),
        ("other", "https://b.example.org"),
    ] {
        store.add_forgejo_instance(name, url, "17.0.2").unwrap();
    }
    let conn = raw(&dir);
    for forge in ["mine", "other", "github"] {
        conn.execute(
            "INSERT INTO user_forge_accounts (login, forge, account_login, account_id)
             VALUES ('alice', ?1, 'bot', 1)",
            [forge],
        )
        .unwrap();
        store
            .put_secret(
                secret("", &format!("forge-user:{forge}:alice")),
                "local",
                b"sealed",
            )
            .unwrap();
    }
    // A name that starts with the deleted name keeps its token.
    store
        .put_secret(secret("", "forge-user:mine-two:alice"), "local", b"sealed")
        .unwrap();
    conn.execute(
        "INSERT INTO project_forge_accounts (project, kind, forge, account_login, account_id)
         VALUES ('stale', 'project', 'mine', 'bot', 1), ('stale', 'upstream', 'other', 'bot', 2)",
        [],
    )
    .unwrap();
    store
        .put_secret(secret("stale", "forge-project"), "local", b"sealed")
        .unwrap();
    store
        .put_secret(secret("stale", "forge-upstream"), "local", b"sealed")
        .unwrap();

    store.delete_forgejo_instance("mine").unwrap();

    let held =
        |project: &str, header: &str| store.get_secret(secret(project, header)).unwrap().is_some();
    assert!(!held("", "forge-user:mine:alice"));
    assert!(!held("stale", "forge-project"));
    assert!(held("", "forge-user:other:alice"));
    assert!(held("", "forge-user:github:alice"));
    assert!(held("", "forge-user:mine-two:alice"));
    assert!(held("stale", "forge-upstream"));
    let accounts: Vec<String> = conn
        .prepare("SELECT forge || '/' || login FROM (SELECT forge, login FROM user_forge_accounts) ORDER BY 1")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(accounts, ["github/alice", "other/alice"]);
    let project_accounts: i64 = conn
        .query_row("SELECT COUNT(*) FROM project_forge_accounts", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(project_accounts, 1);
}

#[test]
fn a_project_and_its_upstream_resolve_against_the_registry() {
    let (dir, store) = store();
    store
        .add_forgejo_instance("mine", "https://git.example.org/sub", "17.0.2")
        .unwrap();
    let conn = raw(&dir);
    conn.execute_batch(
        "INSERT INTO projects (name, forge, repo, base) VALUES ('web', 'mine', 'o/web', 'main');
         INSERT INTO projects (name, forge, repo, base) VALUES ('gone', 'old', 'o/gone', 'main');
         INSERT INTO project_maintenance (project, renovate, packaging, manifest_files, build_config_files, upstream_repo)
         VALUES ('web', 1, 1, '[\"a\"]', '[]', 'o/up');
         INSERT INTO project_maintenance (project, renovate, packaging, manifest_files, build_config_files, upstream_repo)
         VALUES ('gone', 1, 1, '[\"a\"]', '[]', 'o/up');",
    )
    .unwrap();

    let web = store.project("web").unwrap().unwrap();
    let kind = web.forge.kind().expect("registered");
    assert_eq!(kind.as_str(), "mine");
    assert_eq!(kind.web_url("o/web"), "https://git.example.org/sub/o/web");
    assert_eq!(
        kind.pr_url("o/web", 4),
        "https://git.example.org/sub/o/web/pulls/4"
    );
    assert_eq!(kind.host(), "git.example.org");
    assert_eq!(
        store
            .project_maintenance("web")
            .unwrap()
            .packaging
            .unwrap()
            .upstream_forge,
        *kind
    );
    assert_eq!(store.list_projects().unwrap().len(), 2);
    assert!(
        store
            .project("gone")
            .unwrap()
            .unwrap()
            .forge
            .kind()
            .is_none()
    );
    assert!(
        store
            .project_maintenance("gone")
            .unwrap()
            .packaging
            .is_some()
    );

    conn.execute(
        "UPDATE project_maintenance SET upstream_forge = 'old' WHERE project = 'web'",
        [],
    )
    .unwrap();
    assert!(matches!(
        store.project_maintenance("web"),
        Err(StoreError::UnknownUpstreamForge(name)) if name == "old"
    ));
    assert_eq!(
        store.resolve_forge("github").unwrap().unwrap().as_str(),
        "github"
    );
    assert!(store.resolve_forge("old").unwrap().is_none());
    let names: Vec<String> = store
        .forge_kinds()
        .unwrap()
        .iter()
        .map(|k| k.as_str().to_owned())
        .collect();
    assert_eq!(names, ["github", "salsa", "mine"]);
}

#[test]
fn a_write_for_a_deleted_or_replaced_instance_stores_nothing() {
    replaced_instance_stores_nothing("https://two.example.org");
    replaced_instance_stores_nothing("https://one.example.org");
}

fn replaced_instance_stores_nothing(again: &str) {
    use factory_runner::ForgeAccount;
    use factory_server::{
        ForgeTokenOwner, Maintenance, MaintenanceSave, Packaging, PackagingType, ProjectGuard,
    };

    let (_dir, store) = store();
    store
        .add_forgejo_instance("mine", "https://one.example.org", "17.0.2")
        .unwrap();
    let old = store.resolve_forge("mine").unwrap().unwrap();
    store
        .create_project(
            "web",
            &factory_runner::ForgeKind::GitHub,
            "o/web",
            "main",
            None,
            Some("alice"),
        )
        .unwrap();
    let (_, instance) = store.project_with_instance("web").unwrap().unwrap();
    store.delete_forgejo_instance("mine").unwrap();
    store.add_forgejo_instance("mine", again, "17.0.2").unwrap();

    let account = ForgeAccount {
        login: "bot".to_owned(),
        id: 5,
    };
    let owner = ForgeTokenOwner::User {
        login: "dave",
        forge: &old,
    };
    let (row, header) = ("", "forge-user:mine:dave");
    let put = store.put_forge_token(secret(row, header), "local", b"sealed", owner, &account);
    assert!(matches!(put, Err(StoreError::InstanceChanged(name)) if name == "mine"));
    assert!(store.forge_account(owner).unwrap().is_none());
    assert!(store.get_secret(secret(row, header)).unwrap().is_none());

    let guard = ProjectGuard {
        project: "web",
        login: "alice",
        instance,
    };
    let upstream = ForgeTokenOwner::Upstream {
        project: "web",
        forge: &old,
    };
    let put = store.put_project_forge_token(
        secret("web", "forge-upstream"),
        "local",
        b"sealed",
        upstream,
        &account,
        &guard,
    );
    assert!(matches!(put, Err(StoreError::InstanceChanged(_))));
    assert!(store.forge_account(upstream).unwrap().is_none());

    let maintenance = Maintenance {
        renovate: false,
        renovate_author: None,
        packaging: Some(Packaging {
            kind: PackagingType::Other,
            manifest_files: vec!["a".to_owned()],
            build_config_files: Vec::new(),
            upstream_repo: "o/up".to_owned(),
            upstream_forge: old,
        }),
    };
    let saved = store
        .save_project_maintenance_guarded(&guard, &maintenance, &[])
        .unwrap();
    assert_eq!(saved, MaintenanceSave::InstanceChanged);
    assert!(
        store
            .project_maintenance("web")
            .unwrap()
            .packaging
            .is_none()
    );
}

#[test]
fn a_stored_upstream_that_is_no_longer_registered_reads_leniently_for_settings() {
    let (dir, store) = store();
    store
        .create_project(
            "web",
            &factory_runner::ForgeKind::GitHub,
            "o/web",
            "main",
            None,
            Some("alice"),
        )
        .unwrap();
    raw(&dir)
        .execute_batch(
            "INSERT INTO project_maintenance (project, renovate, packaging, manifest_files, build_config_files, upstream_repo, upstream_forge)
             VALUES ('web', 1, 1, '[\"a\"]', '[]', 'o/up', 'old')",
        )
        .unwrap();
    assert!(matches!(
        store.project_maintenance("web"),
        Err(StoreError::UnknownUpstreamForge(_))
    ));
    let (maintenance, unknown) = store.project_maintenance_lenient("web").unwrap();
    assert_eq!(unknown.as_deref(), Some("old"));
    assert!(maintenance.packaging.is_some());
}
