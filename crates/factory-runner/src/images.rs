use std::fmt::Write;
use std::fs;
use std::path::{Path, PathBuf};

use crate::worker::WORK_DIR;

/// Where the images sit under the checkout. Git does not track `.git`, so
/// the checkout's `git status` stays clean.
const IMAGES_DIR: &str = ".git/factory-images";

/// Copies `sources` into the checkout at `workspace` and returns each
/// image's path as the worker sees it, in the order of `sources`.
///
/// # Errors
///
/// Fails when a file cannot be copied or the checkout has no `.git`
/// directory.
pub fn place_images(workspace: &Path, sources: &[PathBuf]) -> Result<Vec<String>, String> {
    if sources.is_empty() {
        return Ok(Vec::new());
    }
    let dir = workspace.join(IMAGES_DIR);
    fs::create_dir_all(&dir).map_err(|err| format!("cannot create {}: {err}", dir.display()))?;
    let mut paths = Vec::new();
    for source in sources {
        let Some(name) = source.file_name() else {
            return Err(format!("{} has no file name", source.display()));
        };
        let target = dir.join(name);
        fs::copy(source, &target)
            .map_err(|err| format!("cannot copy {}: {err}", source.display()))?;
        paths.push(format!(
            "{WORK_DIR}/{IMAGES_DIR}/{}",
            name.to_string_lossy()
        ));
    }
    Ok(paths)
}

/// Adds the image paths to the end of `prompt` and tells the agent to look
/// at them. It returns `prompt` unchanged when there are no images.
#[must_use]
pub fn show_images(prompt: &str, paths: &[String]) -> String {
    if paths.is_empty() {
        return prompt.to_owned();
    }
    let mut text = format!(
        "{prompt}\n\nThe requester attached images to the request. Look at each one before \
         you answer. They are files in the worker:\n"
    );
    for path in paths {
        let _ = writeln!(text, "- {path}");
    }
    text
}
