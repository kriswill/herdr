use super::*;
use crate::platform::{ForegroundJob, ForegroundProcess};

fn job(process_group_id: u32, processes: &[(u32, &str)]) -> ForegroundJob {
    ForegroundJob {
        process_group_id,
        processes: processes
            .iter()
            .map(|(pid, name)| ForegroundProcess {
                pid: *pid,
                name: name.to_string(),
                argv0: None,
                argv: None,
                cmdline: None,
            })
            .collect(),
    }
}

#[test]
fn follow_nested_shells_default_off_and_parse() {
    let default_config = crate::config::Config::default();
    assert!(!default_config.experimental.follow_nested_shells);

    let toml = r#"
[experimental]
follow_nested_shells = true
"#;
    let config: crate::config::Config = toml::from_str(toml).unwrap();
    assert!(config.experimental.follow_nested_shells);
}

#[cfg(unix)]
#[test]
fn lone_shell_is_a_shell_prompt() {
    for name in ["zsh", "-zsh", "bash"] {
        assert!(job_is_lone_shell(&job(99, &[(99, name)])), "{name}");
    }
}

#[test]
fn shell_running_a_command_is_not_a_shell_prompt() {
    assert!(!job_is_lone_shell(&job(99, &[(99, "sh"), (100, "node")])));
}

#[test]
fn lone_non_shell_is_not_a_shell_prompt() {
    assert!(!job_is_lone_shell(&job(99, &[(99, "vim")])));
}

#[test]
fn shell_that_does_not_lead_its_group_is_not_a_shell_prompt() {
    assert!(!job_is_lone_shell(&job(99, &[(100, "zsh")])));
}

fn terminal(process_group_id: u32, tty_device: u32) -> TerminalForeground {
    TerminalForeground {
        process_group_id,
        tty_device,
    }
}

#[test]
fn innermost_foreground_group_without_nested_session_is_the_terminal_group() {
    let innermost = follow_nested_sessions(terminal(100, 1), |_| None, |_| false);

    assert_eq!(innermost, 100);
}

#[test]
fn innermost_foreground_group_follows_nested_sessions() {
    // pane shell -> devenv shell (tty 1) -> zsh (tty 2) -> script (tty 3) -> claude
    let innermost = follow_nested_sessions(
        terminal(100, 1),
        |foreground| match foreground.process_group_id {
            100 => Some(terminal(200, 2)),
            200 => Some(terminal(300, 3)),
            _ => None,
        },
        |_| false,
    );

    assert_eq!(innermost, 300);
}

#[test]
fn innermost_foreground_group_stays_on_an_agent_hosting_a_pty() {
    let innermost = follow_nested_sessions(
        terminal(100, 1),
        |foreground| match foreground.process_group_id {
            100 => Some(terminal(200, 2)),
            200 => Some(terminal(300, 3)),
            _ => None,
        },
        |process_group_id| process_group_id == 200,
    );

    assert_eq!(innermost, 200);
}

#[test]
fn innermost_foreground_group_stops_at_a_repeated_tty() {
    let innermost = follow_nested_sessions(
        terminal(100, 1),
        |foreground| Some(terminal(foreground.process_group_id + 1, 1)),
        |_| false,
    );

    assert_eq!(innermost, 100);
}

#[test]
fn innermost_foreground_group_stops_at_the_depth_limit() {
    let innermost = follow_nested_sessions(
        terminal(0, 0),
        |foreground| {
            let next = foreground.process_group_id + 1;
            Some(terminal(next, next))
        },
        |_| false,
    );

    assert_eq!(innermost, MAX_PTY_DEPTH as u32);
}

#[test]
fn agent_job_hosts_an_agent() {
    // No live process has this pid, so no environment hint is read.
    let pid = u32::MAX;
    assert!(job_hosts_agent(&job(pid, &[(pid, "codex")])));
    assert!(!job_hosts_agent(&job(pid, &[(pid, "devenv")])));
}

fn parents(pairs: &'static [(u32, u32)]) -> impl Fn(u32) -> Option<u32> {
    move |pid| {
        pairs
            .iter()
            .find_map(|(child, parent)| (*child == pid).then_some(*parent))
    }
}

#[test]
fn process_descends_from_the_shell_through_a_nested_session() {
    // pane shell 10 -> script 20 -> nested zsh 30 -> claude 40
    let parent_of = parents(&[(40, 30), (30, 20), (20, 10), (10, 1)]);

    assert!(descends_from(40, 10, &parent_of));
    assert!(descends_from(40, 30, &parent_of));
    assert!(!descends_from(10, 40, &parent_of));
}

#[test]
fn process_reparented_to_init_does_not_descend_from_the_shell() {
    assert!(!descends_from(40, 10, parents(&[(40, 1)])));
}

#[test]
fn process_with_an_unreadable_parent_does_not_descend_from_the_shell() {
    assert!(!descends_from(40, 10, parents(&[(40, 30)])));
}

#[test]
fn ancestry_walk_stops_at_a_self_parent_and_the_depth_limit() {
    assert!(!descends_from(40, 10, Some));
    assert!(!descends_from(1_000, 1, |pid| Some(pid + 1)));
}
