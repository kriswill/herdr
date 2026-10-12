//! Linux process-table reads for `[experimental] follow_nested_shells` (see
//! `crate::detect::nested_shells`).
//!
//! A foreground job such as `devenv shell` or `script` can run its own PTY
//! session, and an agent started there is in that session's foreground group,
//! not the pane terminal's.

use std::collections::VecDeque;

use super::{
    foreground_job_for_group, process_task_children, process_task_ids, ForegroundJob,
    ForegroundScanBudget,
};
use crate::detect::nested_shells::{self, TerminalForeground};

/// Job members whose children are scanned for a nested PTY session. Each scan
/// reads `/proc` once per thread, and this runs on every detection tick.
const MAX_PARENTS: usize = 8;

/// The foreground group to report for `pid`'s terminal, whose own foreground
/// group is `process_group_id`: that group, or with `follow_nested_shells` on,
/// the innermost foreground group of the PTY sessions it hosts.
pub(super) fn foreground_process_group(pid: u32, process_group_id: u32) -> u32 {
    if !nested_shells::enabled() {
        return process_group_id;
    }
    let Some(stat) = ProcStat::read(pid) else {
        return process_group_id;
    };
    nested_shells::innermost_foreground_process_group(
        TerminalForeground {
            process_group_id,
            tty_device: stat.tty_nr,
        },
        nested_pty_session,
        process_group_job,
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ProcStat {
    pgrp: u32,
    tty_nr: u32,
    tpgid: i32,
}

impl ProcStat {
    fn read(pid: u32) -> Option<Self> {
        Self::parse(&std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)
    }

    /// `/proc/<pid>/stat` is "pid (comm) state ppid pgrp session tty_nr tpgid ...".
    /// `comm` can contain spaces and parens, so fields are read after the last ')'.
    fn parse(stat: &str) -> Option<Self> {
        let rest = stat.get(stat.rfind(')')? + 2..)?;
        let fields: Vec<&str> = rest.split_whitespace().collect();
        Some(Self {
            pgrp: fields.get(2)?.parse().ok()?,
            tty_nr: fields.get(4)?.parse().ok()?,
            tpgid: fields.get(5)?.parse().ok()?,
        })
    }

    fn terminal_foreground(&self) -> Option<TerminalForeground> {
        (self.tpgid > 0).then_some(TerminalForeground {
            process_group_id: self.tpgid as u32,
            tty_device: self.tty_nr,
        })
    }
}

/// Find a PTY session started by a member of the foreground job: a child that
/// left the job and has a different controlling terminal. The session leader's
/// `tpgid` is that terminal's foreground group. Members are reached from the
/// group leader, so a group whose leader exited is not scanned.
fn nested_pty_session(foreground: TerminalForeground) -> Option<TerminalForeground> {
    let mut budget = ForegroundScanBudget::for_probe();
    let mut members = VecDeque::from([foreground.process_group_id]);
    for _ in 0..MAX_PARENTS {
        let Some(member) = members.pop_front() else {
            break;
        };
        for tid in process_task_ids(member, &mut budget) {
            for child in process_task_children(member, tid, &mut budget) {
                let Some(stat) = ProcStat::read(child) else {
                    continue;
                };
                if stat.pgrp == foreground.process_group_id {
                    members.push_back(child);
                } else if stat.tty_nr != foreground.tty_device {
                    if let Some(nested) = stat.terminal_foreground() {
                        return Some(nested);
                    }
                }
            }
        }
    }
    None
}

fn process_group_job(process_group_id: u32) -> Option<ForegroundJob> {
    foreground_job_for_group(process_group_id, process_group_id)
}

#[cfg(test)]
#[path = "nested_pty_tests.rs"]
mod tests;
