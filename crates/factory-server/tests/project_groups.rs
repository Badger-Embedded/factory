//! Tests for the project group writes: the checks that authorize a write run
//! in the transaction that makes it. No network and no files: the store runs
//! in memory.

use factory_runner::ForgeKind;
use factory_server::{GroupsSave, Store};

const LOGIN: &str = "alice@example.com";

fn store() -> Store {
    let store = Store::open_in_memory().expect("open in-memory store");
    for name in ["a", "b"] {
        store
            .create_project(
                name,
                &ForgeKind::GitHub,
                &format!("o/{name}"),
                "main",
                None,
                Some("alice"),
            )
            .unwrap();
        store.add_collaborator(name, LOGIN, "alice").unwrap();
    }
    store
}

fn group_id(store: &Store, name: &str) -> i64 {
    store
        .project_groups(LOGIN)
        .unwrap()
        .iter()
        .find(|group| group.name == name)
        .unwrap()
        .id
}

#[test]
fn a_name_that_repeats_another_ignoring_case_is_refused_when_it_is_saved() {
    let store = store();
    assert_eq!(
        store
            .save_project_groups(LOGIN, &[], Some("Équipe"))
            .unwrap(),
        GroupsSave::Saved
    );
    assert_eq!(
        store
            .save_project_groups(LOGIN, &[], Some("équipe"))
            .unwrap(),
        GroupsSave::Repeated("équipe".into())
    );
    assert_eq!(store.project_groups(LOGIN).unwrap().len(), 1);
}

#[test]
fn a_long_name_and_a_repeated_field_save_nothing_beyond_the_first_name() {
    let store = store();
    store.save_project_groups(LOGIN, &[], Some("Home")).unwrap();
    let home = group_id(&store, "Home");
    let long = "x".repeat(41);
    assert_eq!(
        store
            .save_project_groups(LOGIN, &[(home, "Hobby".into()), (home, long)], None)
            .unwrap(),
        GroupsSave::Saved
    );
    assert_eq!(store.project_groups(LOGIN).unwrap()[0].name, "Hobby");
    let long = "y".repeat(41);
    assert_eq!(
        store
            .save_project_groups(LOGIN, &[(home, long.clone())], None)
            .unwrap(),
        GroupsSave::TooLong(long)
    );
}

#[test]
fn groups_sort_by_their_lowercase_name_including_accents() {
    let store = store();
    for name in ["àland", "Équipe", "Zed", "alpha"] {
        store.save_project_groups(LOGIN, &[], Some(name)).unwrap();
    }
    let names: Vec<_> = store
        .project_groups(LOGIN)
        .unwrap()
        .into_iter()
        .map(|group| group.name)
        .collect();
    assert_eq!(names, ["alpha", "Zed", "àland", "Équipe"]);
}

#[test]
fn a_membership_for_a_missing_group_or_a_departed_project_changes_nothing() {
    let store = store();
    store.save_project_groups(LOGIN, &[], Some("Home")).unwrap();
    store.save_project_groups(LOGIN, &[], Some("Work")).unwrap();
    let (home, work) = (group_id(&store, "Home"), group_id(&store, "Work"));
    store
        .set_project_groups(LOGIN, &[("a".into(), Some(home))])
        .unwrap();

    store
        .save_project_groups(LOGIN, &[(work, String::new())], None)
        .unwrap();
    store
        .set_project_groups(LOGIN, &[("a".into(), Some(work))])
        .unwrap();
    assert_eq!(store.project_groups(LOGIN).unwrap()[0].projects, ["a"]);

    store.remove_project("b").unwrap();
    store
        .set_project_groups(LOGIN, &[("b".into(), Some(home))])
        .unwrap();
    store
        .set_project_groups(LOGIN, &[("not-mine".into(), Some(home))])
        .unwrap();
    assert_eq!(store.project_groups(LOGIN).unwrap()[0].projects, ["a"]);

    store
        .set_project_groups(LOGIN, &[("a".into(), None)])
        .unwrap();
    assert_eq!(store.project_groups(LOGIN).unwrap()[0].projects.len(), 0);
}
