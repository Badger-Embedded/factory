//! Tests for `static/notification-permission.js`, run under node with a fake
//! page and a fake `Notification`: the permission line and Allow button for
//! each permission state, and the display after the Allow click resolves.

use std::path::PathBuf;
use std::process::Command;

use serde_json::Value;

const HARNESS: &str = r#"
const fs = require("fs");
const [path, permission, clickResult] = process.argv.slice(1);
const line = { textContent: "" };
const button = {
  hidden: true,
  click: null,
  addEventListener(type, handler) {
    if (type === "click") this.click = handler;
  },
};
const box = {
  hidden: true,
  querySelector: (selector) => (selector === ".permission-state" ? line : button),
};
global.document = {
  getElementById: (id) => (id === "notification-permission" ? box : null),
};
global.window = {};
if (permission !== "missing") {
  const Notification = function () {};
  Notification.permission = permission;
  Notification.requestPermission = () => {
    Notification.permission = clickResult;
    return Promise.resolve(clickResult);
  };
  global.Notification = Notification;
  global.window.Notification = Notification;
}
const state = () => ({ line: line.textContent, boxHidden: box.hidden, buttonHidden: button.hidden });
new Function(fs.readFileSync(path, "utf8"))();
const before = state();
(async () => {
  let after = null;
  if (clickResult && button.click) {
    button.click();
    await new Promise((resolve) => setImmediate(resolve));
    after = state();
  }
  console.log(JSON.stringify({ before, after }));
})();
"#;

fn run(permission: &str, click_result: &str) -> Value {
    let script =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("static/notification-permission.js");
    let output = Command::new("node")
        .args(["-e", HARNESS])
        .arg(script)
        .arg(permission)
        .arg(click_result)
        .output()
        .expect("node runs; the dev shell provides it");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn a_browser_not_asked_yet_shows_the_allow_button() {
    let state = run("default", "")["before"].clone();
    assert_eq!(state["boxHidden"], false);
    assert_eq!(state["buttonHidden"], false);
    assert!(state["line"].as_str().unwrap().contains("not asked yet"));
}

#[test]
fn a_granted_permission_shows_its_state_and_no_button() {
    let state = run("granted", "")["before"].clone();
    assert_eq!(state["boxHidden"], false);
    assert_eq!(state["buttonHidden"], true);
    assert!(state["line"].as_str().unwrap().contains("granted"));
}

#[test]
fn a_denied_permission_says_to_unblock_the_site_and_shows_no_button() {
    let state = run("denied", "")["before"].clone();
    assert_eq!(state["boxHidden"], false);
    assert_eq!(state["buttonHidden"], true);
    let line = state["line"].as_str().unwrap();
    assert!(line.contains("blocked"));
    assert!(line.contains("site settings"));
}

#[test]
fn the_allow_button_updates_the_display_once_the_request_resolves() {
    let granted = run("default", "granted");
    assert!(
        granted["before"]["line"]
            .as_str()
            .unwrap()
            .contains("not asked yet")
    );
    assert_eq!(granted["after"]["buttonHidden"], true);
    assert!(
        granted["after"]["line"]
            .as_str()
            .unwrap()
            .contains("granted")
    );

    let denied = run("default", "denied");
    assert_eq!(denied["after"]["buttonHidden"], true);
    assert!(
        denied["after"]["line"]
            .as_str()
            .unwrap()
            .contains("blocked")
    );
}

#[test]
fn a_browser_without_the_notification_api_keeps_the_block_hidden() {
    let state = run("missing", "")["before"].clone();
    assert_eq!(state["boxHidden"], true);
    assert_eq!(state["line"], "");
}
