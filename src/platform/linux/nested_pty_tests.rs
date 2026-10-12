use super::*;

#[test]
fn proc_stat_reads_fields_after_a_comm_with_spaces_and_parens() {
    let stat = "4242 (my (odd) cmd) S 1 4242 4242 34817 4300 4194304 0 0";

    assert_eq!(
        ProcStat::parse(stat),
        Some(ProcStat {
            ppid: 1,
            pgrp: 4242,
            tty_nr: 34817,
            tpgid: 4300,
        })
    );
}

#[test]
fn proc_stat_without_a_terminal_has_no_terminal_foreground() {
    let stat = ProcStat::parse("77 (daemon) S 1 77 77 0 -1 4194560 0 0").unwrap();

    assert_eq!(stat.tpgid, -1);
    assert_eq!(stat.terminal_foreground(), None);
}

fn spawn_in_test_pty(
    argv: &[&str],
) -> (
    portable_pty::PtyPair,
    Box<dyn portable_pty::Child + Send + Sync>,
) {
    let pair = portable_pty::native_pty_system()
        .openpty(portable_pty::PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("failed to open pty");
    let child = pair
        .slave
        .spawn_command(portable_pty::CommandBuilder::from_argv(
            argv.iter().map(Into::into).collect(),
        ))
        .expect("failed to spawn");
    (pair, child)
}

fn poll<T>(mut probe: impl FnMut() -> Option<T>) -> Option<T> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if let Some(value) = probe() {
            return Some(value);
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    None
}

/// The `sleep` that `script` (pid `host_pid`) runs as leader of its own PTY session.
fn nested_sleep_session_leader(host_pid: u32) -> Option<u32> {
    let mut budget = ForegroundScanBudget::for_probe();
    let tids = process_task_ids(host_pid, &mut budget);
    tids.into_iter()
        .flat_map(|tid| process_task_children(host_pid, tid, &mut budget))
        .find(|pid| {
            ProcStat::read(*pid).is_some_and(|stat| stat.tpgid > 0)
                && super::super::process_pgrp_comm_and_state(*pid)
                    .is_some_and(|(_, comm, _)| comm == "sleep")
        })
}

/// The pane-level lookup with `follow_nested_shells` on, without touching the
/// process-wide flag other tests may observe.
fn followed_foreground_process_group(pid: u32) -> Option<u32> {
    let stat = ProcStat::read(pid)?;
    Some(nested_shells::innermost_foreground_process_group(
        TerminalForeground {
            process_group_id: u32::try_from(stat.tpgid).ok()?,
            tty_device: stat.tty_nr,
        },
        nested_pty_session,
        process_group_job,
    ))
}

fn kill_processes(pids: impl IntoIterator<Item = Option<u32>>) {
    for pid in pids.into_iter().flatten() {
        unsafe {
            libc::kill(pid as libc::c_int, libc::SIGKILL);
        }
    }
}

#[test]
fn foreground_group_follows_a_nested_pty_session() {
    let (_pair, mut child) =
        spawn_in_test_pty(&["script", "-q", "-c", "exec sleep 30", "/dev/null"]);
    let pid = child.process_id().expect("no pid");

    let sleep_pid = poll(|| nested_sleep_session_leader(pid));
    let foreground = poll(|| followed_foreground_process_group(pid).filter(|pgid| *pgid != pid));
    let job = foreground.and_then(process_group_job);
    kill_processes([sleep_pid, Some(pid)]);
    child.wait().ok();

    let sleep_pid = sleep_pid.expect("script should run sleep on a nested pty");
    assert_eq!(foreground, Some(sleep_pid));
    let job = job.expect("expected foreground job");
    assert!(
        job.processes.iter().any(|process| process.name == "sleep"),
        "expected the nested sleep in {job:?}"
    );
}

#[test]
fn foreground_group_stays_on_an_agent_that_hosts_a_pty() {
    let (_pair, mut child) = spawn_in_test_pty(&[
        "bash",
        "-c",
        "exec -a codex script -q -c 'exec sleep 30' /dev/null",
    ]);
    let pid = child.process_id().expect("no pid");

    let sleep_pid = poll(|| nested_sleep_session_leader(pid));
    let foreground = followed_foreground_process_group(pid);
    let job = process_group_job(pid);
    kill_processes([sleep_pid, Some(pid)]);
    child.wait().ok();

    assert!(
        sleep_pid.is_some(),
        "script should run sleep on a nested pty"
    );
    assert_eq!(foreground, Some(pid));
    let job = job.expect("expected foreground job");
    assert_eq!(
        crate::detect::identify_agent_in_job(&job),
        Some((crate::detect::Agent::Codex, "codex".to_string()))
    );
}

#[test]
fn nested_pty_session_is_outside_the_pane_session_but_descends_from_its_host() {
    let (_pair, mut child) =
        spawn_in_test_pty(&["script", "-q", "-c", "exec sleep 30", "/dev/null"]);
    let pid = child.process_id().expect("no pid");

    let sleep_pid = poll(|| nested_sleep_session_leader(pid));
    // With `follow_nested_shells` off, as other tests leave the process-wide flag.
    let in_pane_session = sleep_pid.and_then(|sleep_pid| {
        let start_token = crate::platform::process_start_token(sleep_pid)?;
        crate::platform::live_pane_process_group(pid, sleep_pid, start_token)
    });
    let descends =
        sleep_pid.map(|sleep_pid| nested_shells::descends_from(sleep_pid, pid, parent_pid));
    let unrelated_descends = nested_shells::descends_from(std::process::id(), pid, parent_pid);
    kill_processes([sleep_pid, Some(pid)]);
    child.wait().ok();

    assert!(
        sleep_pid.is_some(),
        "script should run sleep on a nested pty"
    );
    assert_eq!(in_pane_session, None);
    assert_eq!(descends, Some(true));
    assert!(!unrelated_descends);
}
