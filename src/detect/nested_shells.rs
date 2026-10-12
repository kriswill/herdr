//! `[experimental] follow_nested_shells`: detect agents that run inside a nested
//! shell, either a PTY session such as `devenv shell` or `script`, or a
//! subshell.
//!
//! An agent stopped with ctrl-z inside a nested PTY session keeps its session,
//! as upstream does for one stopped in the pane terminal itself.
//!
//! This is a kriswill-custom feature. It lives in its own modules (this one,
//! plus the `nested_pty` process-table readers under `platform::macos` and
//! `platform::linux`) so upstream files only carry the hooks that call into
//! them.

use std::sync::atomic::{AtomicBool, Ordering};

use crate::platform::ForegroundJob;

/// Nested PTY sessions deeper than this are not followed.
const MAX_PTY_DEPTH: usize = 4;
/// Parents walked when checking that a process descends from the pane shell.
const MAX_ANCESTRY_DEPTH: usize = 64;

static ENABLED: AtomicBool = AtomicBool::new(false);

/// Applied by the server at startup and on config reload. Pane detection tasks
/// read it on every probe, so a reload takes effect without restarting panes.
pub fn set_enabled(enabled: bool) {
    ENABLED.store(enabled, Ordering::Relaxed);
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// True when following nested shells and the foreground job is a subshell or
/// a nested PTY session's shell at its prompt. An agent that exits back to it
/// has left the foreground, so its identity clears at once, as it does when
/// the pane shell returns.
pub fn is_shell_prompt(job: &ForegroundJob) -> bool {
    enabled() && job_is_lone_shell(job)
}

fn job_is_lone_shell(job: &ForegroundJob) -> bool {
    let [process] = job.processes.as_slice() else {
        return false;
    };
    // Windows selects single-process jobs from the process tree, where an
    // agent's `cmd /c` tool command also looks like a lone shell.
    cfg!(unix)
        && process.pid == job.process_group_id
        && crate::platform::is_pane_shell_process_name(&process.name)
}

/// A terminal's foreground process group, and the terminal's device number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TerminalForeground {
    pub(crate) process_group_id: u32,
    pub(crate) tty_device: u32,
}

/// Descend from `foreground` through the PTY sessions its jobs host and return
/// the innermost foreground group. `nested_session` finds a session started by
/// a member of a foreground job; `group_job` reads a process group's job.
///
/// A job that is itself an agent is not descended into: Codex runs tool
/// commands on PTYs of its own, and those commands must not displace it.
pub(crate) fn innermost_foreground_process_group(
    foreground: TerminalForeground,
    nested_session: impl Fn(TerminalForeground) -> Option<TerminalForeground>,
    group_job: impl Fn(u32) -> Option<ForegroundJob>,
) -> u32 {
    follow_nested_sessions(foreground, nested_session, |process_group_id| {
        group_job(process_group_id).is_some_and(|job| job_hosts_agent(&job))
    })
}

fn follow_nested_sessions(
    mut foreground: TerminalForeground,
    nested_session: impl Fn(TerminalForeground) -> Option<TerminalForeground>,
    hosts_agent: impl Fn(u32) -> bool,
) -> u32 {
    let mut seen_ttys = vec![foreground.tty_device];
    for _ in 0..MAX_PTY_DEPTH {
        let Some(nested) = nested_session(foreground) else {
            break;
        };
        if seen_ttys.contains(&nested.tty_device) || hosts_agent(foreground.process_group_id) {
            break;
        }
        seen_ttys.push(nested.tty_device);
        foreground = nested;
    }
    foreground.process_group_id
}

/// True when following nested shells and `pid` descends from the pane shell
/// `shell_pid`. `parent_of` reads a process's parent.
///
/// Holding an agent stopped with ctrl-z requires it to be alive in the pane
/// shell's terminal session. An agent inside a nested PTY session is in a
/// session of its own, so descent from the pane shell stands in for that.
pub(crate) fn hosted_by_pane_shell(
    shell_pid: u32,
    pid: u32,
    parent_of: impl Fn(u32) -> Option<u32>,
) -> bool {
    enabled() && descends_from(pid, shell_pid, parent_of)
}

pub(crate) fn descends_from(
    pid: u32,
    ancestor: u32,
    parent_of: impl Fn(u32) -> Option<u32>,
) -> bool {
    let mut current = pid;
    for _ in 0..MAX_ANCESTRY_DEPTH {
        let Some(parent) = parent_of(current) else {
            return false;
        };
        if parent == ancestor {
            return true;
        }
        if parent <= 1 || parent == current {
            return false;
        }
        current = parent;
    }
    false
}

fn job_hosts_agent(job: &ForegroundJob) -> bool {
    crate::detect::identify_agent_in_job(job).is_some()
        || job
            .processes
            .iter()
            .any(|process| crate::platform::process_agent_hint(process.pid).is_some())
}

#[cfg(test)]
#[path = "nested_shells_tests.rs"]
mod tests;
