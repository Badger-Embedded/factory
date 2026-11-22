use std::path::PathBuf;
use std::process::Command;

use serde_json::Value;

/// Runs `static/notifications.js` against a fake browser: one poll per entry
/// of the response list, with `localStorage` carried across them.
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

fn on(items: &[Value]) -> Value {
    serde_json::json!({ "enabled": true, "items": items })
}

#[test]
fn the_first_poll_stores_the_keys_without_notifying() {
    let out = run("granted", &[on(&[item(1, 5, "needs-you")])]);
    assert_eq!(out["shown"].as_array().unwrap().len(), 0);
    assert_eq!(out["stored"], "[\"1:5\"]");
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
    assert_eq!(out["stored"], "[]");
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
