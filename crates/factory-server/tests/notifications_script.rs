use std::path::PathBuf;
use std::process::Command;

use serde_json::Value;

const BROWSER: &str = r#"
const fs = require("fs");
const shown = [];
const storage = {};
global.localStorage = {
  get length() { return Object.keys(storage).length; },
  key: (i) => Object.keys(storage)[i] ?? null,
  getItem: (k) => {
    const value = k in storage ? storage[k] : null;
    if (global.afterRead) { const hook = global.afterRead; global.afterRead = null; hook(); }
    return value;
  },
  setItem: (k, v) => { storage[k] = v; },
  removeItem: (k) => { delete storage[k]; },
};
function Notification(title, options) { shown.push({ title, ...options }); }
global.Notification = Notification;
global.window = { Notification, focus() {}, open() {} };
const load = (path) => fs.readFileSync(path, "utf8");
const latest = () => {
  let best = null;
  for (const [name, value] of Object.entries(storage)) {
    if (!name.startsWith("factory.notified.")) continue;
    const state = JSON.parse(value);
    const [a, b] = state.snapshot;
    if (!best || a > best.snapshot[0] || (a === best.snapshot[0] && b > best.snapshot[1])) best = state;
  }
  return best;
};
const item = (n) => ({ key: "1:" + n, job: 1, title: "Add a button", kind: "needs-you", url: "/jobs/1" });
const body = (snapshot, items) => ({ enabled: true, snapshot, items });
"#;

const SEQUENTIAL: &str = r"
const [path, permission, responses] = process.argv.slice(1);
Notification.permission = permission;
const queue = JSON.parse(responses).map((r, i) => ({ snapshot: [1, i + 1], ...r }));
global.fetch = () => Promise.resolve({ ok: true, json: () => Promise.resolve(queue.shift()) });
global.setInterval = () => 0;
const code = load(path);
(async () => {
  const total = queue.length;
  for (let i = 0; i < total; i++) {
    new Function(code)();
    await new Promise((resolve) => setImmediate(resolve));
  }
  console.log(JSON.stringify({ shown, latest: latest() }));
})();
";

const INTERLEAVED: &str = r#"
const [path] = process.argv.slice(1);
Notification.permission = "granted";
Object.defineProperty(global, "navigator", { value: {}, configurable: true });
Date.now = () => 100;

function Sync() { this.state = null; this.value = undefined; this.handlers = []; }
Sync.prototype.then = function (ok, err) {
  const next = new Sync();
  this.handlers.push({ ok, err, next });
  if (this.state) flush(this);
  return next;
};
Sync.prototype.catch = function (err) { return this.then(undefined, err); };
function settle(p, state, value) { p.state = state; p.value = value; flush(p); }
function flush(p) {
  const handlers = p.handlers;
  p.handlers = [];
  for (const h of handlers) {
    const cb = p.state === "ok" ? h.ok : h.err;
    if (!cb) { settle(h.next, p.state, p.value); continue; }
    try {
      const result = cb(p.value);
      if (result && typeof result.then === "function") {
        result.then((v) => settle(h.next, "ok", v), (e) => settle(h.next, "err", e));
      } else settle(h.next, "ok", result);
    } catch (e) { settle(h.next, "err", e); }
  }
}
const done = (value) => { const p = new Sync(); settle(p, "ok", value); return p; };

const requests = [];
global.fetch = () => { const p = new Sync(); requests.push(p); return p; };
const ticks = [];
global.setInterval = (fn) => { ticks.push(fn); return 0; };
const code = load(path);
const respond = (request, snapshot, items) =>
  settle(request, "ok", { ok: true, json: () => done(body(snapshot, items)) });

storage["factory.notified.seed"] = JSON.stringify({ snapshot: [1, 0], keys: [] });
new Function(code)();
new Function(code)();
const [first, second] = requests.splice(0);
// The tab that started first gets the newer snapshot from the server. The
// other tab has read the stored state when the first one finishes its write.
global.afterRead = () => respond(first, [1, 6], [item(6)]);
respond(second, [1, 5], [item(5)]);
const afterRace = latest();
ticks[0]();
respond(requests.shift(), [1, 7], [item(6)]);
console.log(JSON.stringify({ shown, afterRace, latest: latest() }));
"#;

fn node(harness: &str, args: &[&str]) -> Value {
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("static/notifications.js");
    let output = Command::new("node")
        .args(["-e", &format!("{BROWSER}{harness}")])
        .arg(script)
        .args(args)
        .output()
        .expect("node runs; the dev shell provides it");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn item(job: u64, event: u64, kind: &str) -> Value {
    serde_json::json!({
        "key": format!("{job}:{event}"), "job": job, "title": "Add a button",
        "kind": kind, "url": format!("/jobs/{job}"),
    })
}

fn run(permission: &str, responses: &[Value]) -> Value {
    node(
        SEQUENTIAL,
        &[permission, &serde_json::to_string(responses).unwrap()],
    )
}

fn on(items: &[Value]) -> Value {
    serde_json::json!({ "enabled": true, "items": items })
}

fn keys(state: &Value) -> Vec<&str> {
    state["keys"]
        .as_array()
        .unwrap()
        .iter()
        .map(|key| key.as_str().unwrap())
        .collect()
}

fn tags(out: &Value) -> Vec<&str> {
    out["shown"]
        .as_array()
        .unwrap()
        .iter()
        .map(|note| note["tag"].as_str().unwrap())
        .collect()
}

#[test]
fn the_first_poll_stores_the_keys_without_notifying() {
    let out = run("granted", &[on(&[item(1, 5, "needs-you")])]);
    assert_eq!(out["shown"].as_array().unwrap().len(), 0);
    assert_eq!(keys(&out["latest"]), ["1:5"]);
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
    assert_eq!(keys(&out["latest"]), Vec::<&str>::new());
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

#[test]
fn a_tab_that_read_the_state_before_a_newer_write_cannot_overwrite_it() {
    let out = node(INTERLEAVED, &[]);
    assert_eq!(out["afterRace"]["snapshot"], serde_json::json!([1, 6]));
    assert_eq!(keys(&out["afterRace"]), ["1:6"]);
    assert_eq!(
        tags(&out).iter().filter(|tag| **tag == "1:6").count(),
        1,
        "a key shown once is not shown again: {:?}",
        tags(&out)
    );
    assert_eq!(out["latest"]["snapshot"], serde_json::json!([1, 7]));
    assert_eq!(keys(&out["latest"]), ["1:6"]);
}
