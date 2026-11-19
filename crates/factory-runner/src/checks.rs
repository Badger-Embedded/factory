use std::ffi::OsString;
use std::path::Path;
use std::process::Stdio;

use factory_core::Event;
use tokio::process::Command;

use crate::worker::{WORK_DIR, WorkerConfig};

const MAX_REPORT: usize = 30_000;
/// Exit codes podman uses for its own errors, as opposed to the command's.
const PODMAN_ERRORS: [i32; 3] = [125, 126, 127];

/// The arguments after `podman` to run `command` with bash in the job's
/// checkout. The container gets no Claude token and no GitHub credentials.
#[must_use]
pub fn checks_args(
    config: &WorkerConfig,
    name: &str,
    workspace: &Path,
    command: &str,
) -> Vec<OsString> {
    let mut volume = OsString::from(workspace.as_os_str());
    volume.push(format!(":{WORK_DIR}"));
    let mut args: Vec<OsString> = [
        "run",
        "--rm",
        "--init",
        "--userns=keep-id",
        "--name",
        name,
        "--memory",
        &config.memory,
        "--cpus",
        &config.cpus,
        "--workdir",
        WORK_DIR,
        "--volume",
    ]
    .into_iter()
    .map(OsString::from)
    .collect();
    args.push(volume);
    for volume in &config.volumes {
        args.push("--volume".into());
        args.push(volume.into());
    }
    for arg in [config.image.as_str(), "bash", "-lc", command] {
        args.push(arg.into());
    }
    args
}

/// Runs the project's checks in a worker. A failing command becomes
/// [`Event::ChecksFailed`] with the end of its output.
///
/// # Errors
///
/// Fails when podman cannot run the container, which is not the agent's to
/// fix, so the factory tries again later.
pub async fn run_checks(
    config: &WorkerConfig,
    name: &str,
    workspace: &Path,
    command: &str,
) -> Result<Event, String> {
    let output = Command::new(&config.podman)
        .args(checks_args(config, name, workspace, command))
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|err| format!("could not start {}: {err}", config.podman.display()))?;
    if output.status.success() {
        return Ok(Event::ChecksPassed);
    }
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    if output
        .status
        .code()
        .is_some_and(|code| PODMAN_ERRORS.contains(&code))
    {
        return Err(format!(
            "podman could not run the checks ({}): {}",
            output.status,
            text.trim()
        ));
    }
    let mut start = text.len().saturating_sub(MAX_REPORT);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    Ok(Event::ChecksFailed {
        report: format!(
            "`{command}` failed ({}):\n\n{}",
            output.status,
            &text[start..]
        ),
    })
}
