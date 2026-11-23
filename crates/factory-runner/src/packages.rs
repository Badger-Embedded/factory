use std::ffi::OsString;

/// The exit code the wrapper uses when the packages cannot be built, so the
/// factory tells that apart from the wrapped command's own failure.
pub const FAILED_CODE: i32 = 86;

/// Takes the package count, then that many package names, and runs the rest
/// as the command. It builds the packages first so a failure is reported
/// before the command starts. The command writes a line to a deleted file
/// held open on a descriptor when it starts, which command cleanup of
/// temporary files cannot erase, so an exit before that is a setup failure and exits 86. A command
/// that exits 86 itself is reported as 1. The package names arrive as
/// arguments, never inside this text.
const SCRIPT: &str = r#"n=$1; shift
pkgs=()
for ((i = 0; i < n; i++)); do pkgs+=("nixpkgs#$1"); shift; done
nix build --no-link "${pkgs[@]}" >&2 || exit 86
f=$(mktemp)
exec 9>"$f" 8<"$f"
rm -f "$f"
nix shell "${pkgs[@]}" --command bash -c 'echo >&9; exec "$@" 8<&- 9>&-' bash "$@"
code=$?
if ! IFS= read -r -u 8 _; then exit 86; fi
if [ "$code" -eq 86 ]; then exit 1; fi
exit "$code""#;

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
    args.push(packages.len().to_string().into());
    args.extend(packages.iter().map(OsString::from));
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
