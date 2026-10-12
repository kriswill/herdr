//! macOS process-table reads for `[experimental] follow_nested_shells` (see
//! `crate::detect::nested_shells`).
//!
//! A foreground job such as `devenv shell` or `script` can run its own PTY
//! session, and an agent started there is in that session's foreground group,
//! not the pane terminal's.

use super::{
    collect_positive_pids, comm_from_bsdinfo, process_argv, process_argv0_name, process_bsdinfo,
    process_group_pids, ForegroundJob, ForegroundProcess,
};
use crate::detect::nested_shells::{self, TerminalForeground};

const PROC_PPID_ONLY: u32 = 6;
/// Job members whose children are scanned for a nested PTY session. Each scan
/// walks the process table, and this runs on every detection tick.
const MAX_PARENTS: usize = 8;

/// The foreground group to report for a terminal whose own foreground group is
/// `process_group_id`: that group, or with `follow_nested_shells` on, the
/// innermost foreground group of the PTY sessions it hosts.
pub(super) fn foreground_process_group(process_group_id: u32, tty_device: u32) -> u32 {
    if !nested_shells::enabled() {
        return process_group_id;
    }
    nested_shells::innermost_foreground_process_group(
        TerminalForeground {
            process_group_id,
            tty_device,
        },
        nested_pty_session,
        process_group_job,
    )
}

/// Whether `pid`, alive on another terminal than the pane's, is in a nested
/// PTY session that the pane shell `shell_pid` hosts. Only with
/// `follow_nested_shells` on.
pub(super) fn in_nested_session(shell_pid: u32, pid: u32) -> bool {
    nested_shells::hosted_by_pane_shell(shell_pid, pid, parent_pid)
}

fn parent_pid(pid: u32) -> Option<u32> {
    process_bsdinfo(pid).map(|info| info.pbi_ppid)
}

fn terminal_foreground(info: &libc::proc_bsdinfo) -> Option<TerminalForeground> {
    let fg = info.e_tpgid;
    #[allow(clippy::unnecessary_cast)] // info.e_tpgid (pid_t) type is platform-dependent
    (fg > 0).then_some(TerminalForeground {
        process_group_id: fg as u32,
        tty_device: info.e_tdev,
    })
}

/// Find a PTY session started by a member of the foreground job: a child that
/// left the job and has a different controlling terminal. The session leader's
/// `e_tpgid` is that terminal's foreground group.
fn nested_pty_session(foreground: TerminalForeground) -> Option<TerminalForeground> {
    process_group_pids(foreground.process_group_id)
        .into_iter()
        .take(MAX_PARENTS)
        .flat_map(child_pids)
        .filter_map(process_bsdinfo)
        .filter(|info| {
            info.pbi_pgid != foreground.process_group_id && info.e_tdev != foreground.tty_device
        })
        .find_map(|info| terminal_foreground(&info))
}

fn process_group_job(process_group_id: u32) -> Option<ForegroundJob> {
    let processes: Vec<_> = process_group_pids(process_group_id)
        .into_iter()
        .filter_map(|pid| {
            let info = process_bsdinfo(pid)?;
            if info.pbi_pgid != process_group_id {
                return None;
            }
            let argv = process_argv(pid);
            Some(ForegroundProcess {
                pid,
                name: comm_from_bsdinfo(&info)?,
                argv0: process_argv0_name(pid),
                cmdline: argv.as_ref().map(|parts| parts.join(" ")),
                argv,
            })
        })
        .collect();
    (!processes.is_empty()).then_some(ForegroundJob {
        process_group_id,
        processes,
    })
}

fn child_pids(parent_pid: u32) -> Vec<u32> {
    let mut capacity = 16usize;
    for _ in 0..8 {
        let mut pids = vec![0 as libc::pid_t; capacity];
        let buffer_bytes = pids.len() * std::mem::size_of::<libc::pid_t>();
        let returned_bytes = unsafe {
            libc::proc_listpids(
                PROC_PPID_ONLY,
                parent_pid,
                pids.as_mut_ptr() as *mut libc::c_void,
                buffer_bytes as libc::c_int,
            )
        };
        if returned_bytes <= 0 {
            return Vec::new();
        }
        let returned_bytes = returned_bytes as usize;
        if returned_bytes < buffer_bytes {
            return collect_positive_pids(
                pids,
                returned_bytes / std::mem::size_of::<libc::pid_t>(),
            );
        }
        capacity = capacity.saturating_mul(2);
    }
    Vec::new()
}

#[cfg(test)]
#[path = "nested_pty_tests.rs"]
mod tests;
