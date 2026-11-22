//! Helpers shared by this crate's test binaries.

#![allow(dead_code, reason = "each test binary uses a part of the helpers")]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Writes `body` as the executable script `dir/name` and returns its path.
///
/// Tests run in parallel threads, and a child that another thread forks
/// inherits every open file until it calls exec. If this process had the
/// script open for writing at that moment, running the script fails with
/// "Text file busy" until the child execs. So the text goes to a sibling file
/// that nothing runs, and a `cp` child creates the script, which this process
/// never opens.
pub fn executable(dir: &Path, name: &str, body: &str) -> PathBuf {
    let source = dir.join(format!("{name}.src"));
    fs::write(&source, body).unwrap();
    let path = dir.join(name);
    let status = Command::new("cp").arg(&source).arg(&path).status().unwrap();
    assert!(status.success(), "cp {} failed", source.display());
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

pub fn check_link_targets(body: &str) -> usize {
    let html = scraper::Html::parse_document(body);
    let anchors = scraper::Selector::parse("a[href]").unwrap();
    let mut external = 0;
    for a in html.select(&anchors) {
        let href = a.value().attr("href").unwrap();
        let (target, rel) = (a.value().attr("target"), a.value().attr("rel"));
        if href.starts_with("http://") || href.starts_with("https://") {
            external += 1;
            assert_eq!(target, Some("_blank"), "{href} must open in a new tab");
            assert_eq!(rel, Some("noopener noreferrer"), "{href} needs rel");
        } else {
            assert_ne!(target, Some("_blank"), "{href} must stay in the same tab");
        }
    }
    external
}
