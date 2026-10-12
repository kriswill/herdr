use super::*;

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
    child_pids(host_pid).into_iter().find(|pid| {
        process_bsdinfo(*pid).is_some_and(|info| {
            info.e_tpgid > 0 && comm_from_bsdinfo(&info).as_deref() == Some("sleep")
        })
    })
}

/// The pane-level lookup with `follow_nested_shells` on, without touching the
/// process-wide flag other tests may observe.
fn followed_foreground_process_group(pid: u32) -> Option<u32> {
    Some(nested_shells::innermost_foreground_process_group(
        terminal_foreground(&process_bsdinfo(pid)?)?,
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
        spawn_in_test_pty(&["/usr/bin/script", "-q", "/dev/null", "/bin/sleep", "30"]);
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
        "/bin/bash",
        "-c",
        "exec -a codex /usr/bin/script -q /dev/null /bin/sleep 30",
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
