use std::path::PathBuf;
use std::process::Command;

use serde_json::Value;

const HARNESS: &str = r#"
const fs = require("fs");
const [path, permission, responses] = process.argv.slice(1);
const shown = [];
const storage = {};
global.localStorage = {
  getItem: (k) => (k in storage ? storage[k] : null),
  setItem: (k, v) => { storage[k] = v; },
};
function Notification(title, options) { shown.push({ title, ...options }); }
Notification.permission = permission;
global.Notification = Notification;
global.window = { Notification, focus() {}, open() {} };
const queue = JSON.parse(responses);
global.fetch = () => Promise.resolve({ ok: true, json: () => Promise.resolve(queue.shift()) });
global.setInterval = () => 0;
const code = fs.readFileSync(path, "utf8");
(async () => {
  const total = queue.length;
  for (let i = 0; i < total; i++) {
    new Function(code)();
    await new Promise((resolve) => setImmediate(resolve));
  }
  console.log(JSON.stringify({ shown, stored: storage["factory.notified"] ?? null }));
})();
"#;

fn item(job: u64, event: u64, kind: &str) -> Value {
    serde_json::json!({
        "key": format!("{job}:{event}"), "job": job, "title": "Add a button",
        "kind": kind, "url": format!("/jobs/{job}"),
    })
}

fn run(permission: &str, responses: &[Value]) -> Value {
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("static/notifications.js");
    let output = Command::new("node")
        .args(["-e", HARNESS])
        .arg(script)
        .arg(permission)
        .arg(serde_json::to_string(responses).unwrap())
        .output()
        .expect("node runs; the dev shell provides it");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn keys(out: &Value) -> Vec<String> {
    let state: Value = serde_json::from_str(out["stored"].as_str().unwrap()).unwrap();
    state["keys"]
        .as_array()
        .unwrap()
        .iter()
        .map(|key| key.as_str().unwrap().to_owned())
        .collect()
}

fn on(items: &[Value]) -> Value {
    serde_json::json!({ "enabled": true, "items": items })
}

#[test]
fn the_first_poll_stores_the_keys_without_notifying() {
    let out = run("granted", &[on(&[item(1, 5, "needs-you")])]);
    assert_eq!(out["shown"].as_array().unwrap().len(), 0);
    assert_eq!(keys(&out), ["1:5"]);
}

#[test]
fn a_new_key_notifies_once_and_a_repeat_does_not() {
    let out = run(
        "granted",
        &[
            on(&[]),
            on(&[item(1, 5, "needs-you")]),
            on(&[item(1, 5, "needs-you")]),
            on(&[item(1, 5, "needs-you"), item(2, 9, "failed")]),
        ],
    );
    let shown = out["shown"].as_array().unwrap();
    assert_eq!(shown.len(), 2);
    assert_eq!(shown[0]["title"], "Job #1 needs you");
    assert_eq!(shown[0]["tag"], "1:5");
    assert_eq!(shown[0]["body"], "Add a button");
    assert_eq!(shown[1]["title"], "Job #2 failed");
}

#[test]
fn keys_of_jobs_no_longer_listed_are_dropped() {
    let out = run("granted", &[on(&[item(1, 5, "failed")]), on(&[])]);
    assert_eq!(keys(&out), Vec::<&str>::new());
}

#[test]
fn nothing_shows_without_permission_or_when_disabled() {
    let none = run("default", &[on(&[]), on(&[item(1, 5, "failed")])]);
    assert_eq!(none["shown"].as_array().unwrap().len(), 0);
    let off = run(
        "granted",
        &[on(&[]), serde_json::json!({"enabled": false, "items": []})],
    );
    assert_eq!(off["shown"].as_array().unwrap().len(), 0);
}

const TABS: &str = r#"
const fs = require("fs");
const [path] = process.argv.slice(1);
const shown = [];
const storage = { "factory.notified": JSON.stringify({ at: 0, keys: [] }) };
global.localStorage = {
  getItem: (k) => (k in storage ? storage[k] : null),
  setItem: (k, v) => { storage[k] = v; },
};
function Notification(title, options) { shown.push({ title, ...options }); }
Notification.permission = "granted";
global.window = { Notification, focus() {}, open() {} };
global.Notification = Notification;
let clock = 100;
Date.now = () => clock++;
const requests = [];
global.fetch = () => new Promise((resolve) => requests.push(resolve));
const ticks = [];
global.setInterval = (fn) => { ticks.push(fn); return 0; };
const code = fs.readFileSync(path, "utf8");
const tick = () => new Promise((resolve) => setImmediate(resolve));
const reply = (items) => ({ ok: true, json: () => Promise.resolve({ enabled: true, items }) });
const item = (n) => ({ key: "1:" + n, job: 1, title: "t", kind: "needs-you", url: "/jobs/1" });
(async () => {
  new Function(code)();
  new Function(code)();
  await tick();
  const started = requests.length;
  ticks[0]();
  await tick();
  const overlapping = requests.length;
  requests[1](reply([item(6)]));
  await tick();
  await tick();
  requests[0](reply([item(5)]));
  await tick();
  await tick();
  console.log(JSON.stringify({ started, overlapping, shown, stored: storage["factory.notified"] }));
})();
"#;

#[test]
fn an_older_response_that_arrives_last_is_discarded() {
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("static/notifications.js");
    let output = Command::new("node")
        .args(["-e", TABS])
        .arg(script)
        .output()
        .expect("node runs; the dev shell provides it");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let out: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(out["started"], 2);
    assert_eq!(out["overlapping"], 2);
    let tags: Vec<_> = out["shown"]
        .as_array()
        .unwrap()
        .iter()
        .map(|note| note["tag"].as_str().unwrap())
        .collect();
    assert_eq!(tags, ["1:6"]);
    assert_eq!(keys(&out), ["1:6"]);
}
