use std::sync::Arc;

use factory_core::Event;
use factory_server::{McpWarnings, Store};

/// The `mcp-unavailable` rows of job `id`, in order.
fn unavailable(store: &Store, id: i64) -> Vec<(String, String)> {
    store
        .events(id)
        .unwrap()
        .into_iter()
        .filter(|event| event.event == "mcp-unavailable")
        .map(|event| (event.stage, event.detail.unwrap_or_default()))
        .collect()
}

#[test]
fn recording_an_unavailable_server_leaves_the_stage_and_adds_a_log_line() {
    let store = Store::open_in_memory().unwrap();
    let id = store.create("maestro", "alice", "x").unwrap();
    store.apply(id, Event::Start).unwrap();
    let before = store.get(id).unwrap().job.stage().clone();

    store
        .record_mcp_unavailable(id, "context7", "the server could not be reached")
        .unwrap();

    // The row keeps the stage the job was in, so it changes neither the stage
    // nor the turn.
    let rows = unavailable(&store, id);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, before.name());
    assert_eq!(rows[0].1, "context7: the server could not be reached");
    assert_eq!(&store.get(id).unwrap().job.stage().clone(), &before);

    // The warning also writes one factory line that names the server and the
    // reason, with no credential.
    let line = store
        .logs(id, 10)
        .unwrap()
        .into_iter()
        .find(|entry| entry.text.contains("MCP server context7"))
        .expect("a factory line names the server");
    assert_eq!(line.kind, "factory");
    assert!(
        line.text.contains("the server could not be reached"),
        "{}",
        line.text
    );
}

#[test]
fn a_server_is_warned_about_at_most_once_per_run() {
    let store = Arc::new(Store::open_in_memory().unwrap());
    let id = store.create("maestro", "alice", "x").unwrap();
    store.apply(id, Event::Start).unwrap();
    let warnings = McpWarnings::new(Arc::clone(&store));

    warnings.begin(id);
    warnings.record(id, "context7", "the server could not be reached");
    // A second report for the same server in the same run writes nothing.
    warnings.record(id, "context7", "the server answered 503");
    assert_eq!(unavailable(&store, id).len(), 1);

    // A different server in the same run writes its own row.
    warnings.record(id, "acme", "the connection to the server timed out");
    assert_eq!(unavailable(&store, id).len(), 2);

    // A new run warns about the server afresh.
    warnings.begin(id);
    warnings.record(id, "context7", "the server could not be reached");
    assert_eq!(unavailable(&store, id).len(), 3);
}

/// The `mcp-tool-call` rows of job `id`, in order.
fn called(store: &Store, id: i64) -> Vec<(String, String)> {
    store
        .events(id)
        .unwrap()
        .into_iter()
        .filter(|event| event.event == "mcp-tool-call")
        .map(|event| (event.stage, event.detail.unwrap_or_default()))
        .collect()
}

#[test]
fn recording_a_forwarded_call_adds_a_row_without_touching_the_job() {
    let store = Store::open_in_memory().unwrap();
    let id = store.create("maestro", "alice", "x").unwrap();
    store.apply(id, Event::Start).unwrap();
    let before = store.get(id).unwrap().job.stage().clone();
    let turn_before = store.get(id).unwrap().job.turn().clone();
    let logs_before = store.logs(id, 100).unwrap().len();

    store
        .record_mcp_tool_call(id, "context7", "resolve-library-id")
        .unwrap();

    // The row carries the job's current stage and detail "<server>: <tool>".
    let rows = called(&store, id);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, before.name());
    assert_eq!(rows[0].1, "context7: resolve-library-id");

    // The call leaves the job's stage and turn alone and writes no log line.
    assert_eq!(&store.get(id).unwrap().job.stage().clone(), &before);
    assert_eq!(&store.get(id).unwrap().job.turn().clone(), &turn_before);
    assert_eq!(store.logs(id, 100).unwrap().len(), logs_before);

    // Every call is its own row: a second call of the same tool adds a second.
    store
        .record_mcp_tool_call(id, "context7", "resolve-library-id")
        .unwrap();
    assert_eq!(called(&store, id).len(), 2);
}

#[test]
fn record_tool_called_is_not_deduped() {
    let store = Arc::new(Store::open_in_memory().unwrap());
    let id = store.create("maestro", "alice", "x").unwrap();
    store.apply(id, Event::Start).unwrap();
    let warnings = McpWarnings::new(Arc::clone(&store));

    warnings.begin(id);
    warnings.record_tool_called(id, "context7", "resolve-library-id");
    warnings.record_tool_called(id, "context7", "resolve-library-id");
    // Unlike a blocked call, a forwarded call is recorded every time.
    assert_eq!(called(&store, id).len(), 2);
}

/// The `mcp-tool-blocked` rows of job `id`, in order.
fn blocked(store: &Store, id: i64) -> Vec<String> {
    store
        .events(id)
        .unwrap()
        .into_iter()
        .filter(|event| event.event == "mcp-tool-blocked")
        .map(|event| event.detail.unwrap_or_default())
        .collect()
}

#[test]
fn a_refused_tool_call_is_recorded_once_per_tool_per_run() {
    let store = Arc::new(Store::open_in_memory().unwrap());
    let id = store.create("maestro", "alice", "x").unwrap();
    store.apply(id, Event::Start).unwrap();
    let before = store.get(id).unwrap().job.stage().clone();
    let warnings = McpWarnings::new(Arc::clone(&store));

    warnings.begin(id);
    warnings.record_tool_blocked(id, "context7", "delete");
    warnings.record_tool_blocked(id, "context7", "delete");
    assert_eq!(blocked(&store, id), vec!["context7: delete"]);
    assert_eq!(&store.get(id).unwrap().job.stage().clone(), &before);

    // Another tool, and the server's own unavailable warning, are separate.
    warnings.record_tool_blocked(id, "context7", "write");
    warnings.record(id, "context7", "the server answered 503");
    assert_eq!(blocked(&store, id).len(), 2);
    assert_eq!(unavailable(&store, id).len(), 1);

    let line = store
        .logs(id, 10)
        .unwrap()
        .into_iter()
        .find(|entry| entry.text.contains("MCP tool delete"))
        .expect("a factory line names the tool");
    assert_eq!(line.kind, "factory");

    // A new run records the tool afresh.
    warnings.begin(id);
    warnings.record_tool_blocked(id, "context7", "delete");
    assert_eq!(blocked(&store, id).len(), 3);
}
