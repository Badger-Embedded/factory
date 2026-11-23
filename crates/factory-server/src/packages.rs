//! The validation of a project's worker packages: the nixpkgs attribute paths
//! the factory puts on `PATH` in each of the project's workers.

const MAX_PACKAGES: usize = 32;
const MAX_LEN: usize = 128;

/// Checks the packages posted for a project. Entries are trimmed, a blank one
/// is dropped and a duplicate collapses into its first place. Returns the
/// list to save, or the problem, which names the entry.
///
/// # Errors
///
/// Fails when an entry is not a dotted nixpkgs attribute path of letters,
/// digits, `_`, `+` and `-`, is longer than 128 bytes, or there are more than
/// 32 entries.
pub fn validate<'a>(entries: impl IntoIterator<Item = &'a str>) -> Result<Vec<String>, String> {
    let mut packages: Vec<String> = Vec::new();
    for entry in entries {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        if entry.len() > MAX_LEN {
            return Err(format!(
                "the package {:?} is longer than {MAX_LEN} bytes",
                &entry[..entry.floor_char_boundary(40)]
            ));
        }
        if !is_attribute_path(entry) {
            return Err(format!(
                "{entry:?} is not a nixpkgs package name; use an attribute path such as playwright-mcp or python3Packages.requests"
            ));
        }
        if !packages.iter().any(|seen| seen == entry) {
            packages.push(entry.to_owned());
        }
    }
    if packages.len() > MAX_PACKAGES {
        return Err(format!(
            "a project can list at most {MAX_PACKAGES} packages"
        ));
    }
    Ok(packages)
}

fn is_attribute_path(entry: &str) -> bool {
    entry.split('.').all(|part| {
        !part.is_empty()
            && part
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'+' | b'-'))
    })
}
