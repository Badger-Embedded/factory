use std::ffi::OsString;

/// The exit code the wrapper uses when the packages cannot be built, so the
/// factory tells that apart from the wrapped command's own failure.
pub const FAILED_CODE: i32 = 86;

/// Collects the package names before `--`, builds them so a failure is
/// reported before the command starts, then runs the command in a nix shell.
/// The package names arrive as arguments, never inside this text.
const SCRIPT: &str = r#"pkgs=()
while [ "$1" != -- ]; do pkgs+=("nixpkgs#$1"); shift; done
shift
nix build --no-link "${pkgs[@]}" >&2 || exit 86
exec nix shell "${pkgs[@]}" --command "$@""#;

/// The container command that runs `command` with `packages` on `PATH`,
/// which is `command` itself when the list is empty.
#[must_use]
pub fn wrap(packages: &[String], command: Vec<OsString>) -> Vec<OsString> {
    if packages.is_empty() {
        return command;
    }
    let mut args: Vec<OsString> = ["bash", "-c", SCRIPT, "factory-worker-packages"]
        .into_iter()
        .map(OsString::from)
        .collect();
    args.extend(packages.iter().map(OsString::from));
    args.push("--".into());
    args.extend(command);
    args
}

/// Why a run failed when its packages could not be installed.
#[must_use]
pub fn failure(packages: &[String], detail: &str) -> String {
    format!(
        "the worker packages ({}) could not be installed{detail}",
        packages.join(", ")
    )
}
