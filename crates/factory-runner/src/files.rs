use std::fmt::Write;
use std::fs;
use std::path::{Path, PathBuf};

use crate::worker::WORK_DIR;

/// Where the files sit under the checkout, next to the images. Git does not
/// track `.git`, so the checkout's `git status` stays clean.
const FILES_DIR: &str = ".git/factory-files";

/// A file placed in the checkout: its path as the worker sees it, the name the
/// requester uploaded it under, and whether it is a ZIP archive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlacedFile {
    pub path: String,
    pub original_name: String,
    pub is_zip: bool,
}

/// Copies each `(source, original_name)` into the checkout at `workspace` and
/// returns the placed files in the order of `sources`. A source whose stored
/// name ends in `.zip` is a ZIP archive.
///
/// # Errors
///
/// Fails when a file cannot be copied or the checkout has no `.git` directory.
pub fn place_files(
    workspace: &Path,
    sources: &[(PathBuf, String)],
) -> Result<Vec<PlacedFile>, String> {
    if sources.is_empty() {
        return Ok(Vec::new());
    }
    let dir = workspace.join(FILES_DIR);
    fs::create_dir_all(&dir).map_err(|err| format!("cannot create {}: {err}", dir.display()))?;
    let mut placed = Vec::new();
    for (source, original_name) in sources {
        let Some(name) = source.file_name() else {
            return Err(format!("{} has no file name", source.display()));
        };
        let target = dir.join(name);
        fs::copy(source, &target)
            .map_err(|err| format!("cannot copy {}: {err}", source.display()))?;
        let name = name.to_string_lossy();
        placed.push(PlacedFile {
            path: format!("{WORK_DIR}/{FILES_DIR}/{name}"),
            original_name: original_name.clone(),
            is_zip: name.ends_with(".zip"),
        });
    }
    Ok(placed)
}

/// Adds the files to the end of `prompt` and tells the agent how to use them.
/// It returns `prompt` unchanged when there are no files.
#[must_use]
pub fn show_files(prompt: &str, files: &[PlacedFile]) -> String {
    if files.is_empty() {
        return prompt.to_owned();
    }
    let mut text = format!(
        "{prompt}\n\nThe requester attached files to the request. They are files in the \
         worker. Read each text file. For a ZIP archive, run `unzip -l` on it to list its \
         entries before you unpack it, unpack it with `unzip -d` under \
         `.git/factory-files/`, and do not copy its contents into the repository unless the \
         request asks for that.\n"
    );
    for file in files {
        let _ = writeln!(text, "- {} ({})", file.path, file.original_name);
    }
    text
}
