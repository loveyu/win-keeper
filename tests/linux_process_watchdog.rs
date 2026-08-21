#![cfg(target_os = "linux")]

use nix::{
    errno::Errno,
    sys::signal::{Signal, kill, killpg},
    unistd::Pid,
};
use std::{
    io::{BufRead, BufReader, Read, Write},
    os::unix::process::CommandExt,
    path::PathBuf,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

struct ProcessGroup {
    child: Option<Child>,
    pgid: i32,
    member_pid: i32,
}

impl ProcessGroup {
    fn spawn() -> Self {
        let child = Command::new("sh")
            .arg("-c")
            .arg("trap 'exit 0' TERM; sleep 300 & echo $!; wait")
            .process_group(0)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let pgid = child.id() as i32;
        let mut child = child;
        let mut member_pid = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut member_pid)
            .unwrap();
        Self {
            child: Some(child),
            pgid,
            member_pid: member_pid.trim().parse().unwrap(),
        }
    }

    fn is_running(&mut self) -> bool {
        self.child
            .as_mut()
            .is_some_and(|child| child.try_wait().unwrap().is_none())
    }

    fn wait(&mut self) {
        if let Some(mut child) = self.child.take() {
            child.wait().unwrap();
        }
    }
}

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = killpg(Pid::from_raw(self.pgid), Signal::SIGKILL);
            let _ = child.wait();
        }
    }
}

fn spawn_watchdog(pgid: i32) -> Child {
    let mut watchdog = Command::new(env!("CARGO_BIN_EXE_win-keeper"))
        .arg("--process-watchdog")
        .arg(pgid.to_string())
        .arg("250")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut ready = [0_u8; 1];
    watchdog
        .stdout
        .as_mut()
        .unwrap()
        .read_exact(&mut ready)
        .unwrap();
    assert_eq!(ready[0], b'R');
    watchdog
}

#[test]
fn eof_stops_the_entire_process_group() {
    let mut target = ProcessGroup::spawn();
    let mut watchdog = spawn_watchdog(target.pgid);
    drop(watchdog.stdin.take());

    assert!(watchdog.wait().unwrap().success());
    target.wait();
    assert!(!target.is_running());
    assert_eq!(
        kill(Pid::from_raw(target.member_pid), None),
        Err(Errno::ESRCH)
    );
}

#[test]
fn disarm_keeps_the_process_group_running() {
    let mut target = ProcessGroup::spawn();
    let mut watchdog = spawn_watchdog(target.pgid);
    let mut control = watchdog.stdin.take().unwrap();
    control.write_all(b"D").unwrap();
    drop(control);

    assert!(watchdog.wait().unwrap().success());
    assert!(target.is_running());
}

#[test]
fn eof_stops_descendants_that_escape_the_process_group_inside_a_systemd_scope() {
    if !systemd_user_manager_available() {
        return;
    }

    let unit = format!("winkeeper-watchdog-test-{}.scope", std::process::id());
    let mut target = Command::new("systemd-run")
        .args(["--user", "--scope", "--quiet"])
        .arg(format!("--unit={unit}"))
        .arg("--property=KillMode=control-group")
        .args(["/bin/sh", "-c", "setsid /bin/sleep 300 & echo $!; wait"])
        .process_group(0)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let pgid = target.id() as i32;
    let mut escaped_pid = String::new();
    BufReader::new(target.stdout.take().unwrap())
        .read_line(&mut escaped_pid)
        .unwrap();
    let escaped_pid = escaped_pid.trim().parse::<i32>().unwrap();
    let cgroup = wait_for_scope_cgroup(&unit);

    let watchdog_unit = format!(
        "winkeeper-watchdog-runner-test-{}.scope",
        std::process::id()
    );
    let mut watchdog = Command::new("systemd-run")
        .args(["--user", "--scope", "--quiet"])
        .arg(format!("--unit={watchdog_unit}"))
        .arg("--property=KillMode=control-group")
        .arg("--")
        .arg(env!("CARGO_BIN_EXE_win-keeper"))
        .arg("--process-watchdog")
        .arg(pgid.to_string())
        .arg("250")
        .arg("--scope-unit")
        .arg(&unit)
        .arg("--scope-cgroup")
        .arg(&cgroup)
        .arg("--scope-systemctl")
        .arg("systemctl")
        .process_group(0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut ready = [0_u8; 1];
    watchdog
        .stdout
        .as_mut()
        .unwrap()
        .read_exact(&mut ready)
        .unwrap();
    assert_eq!(ready[0], b'R');
    drop(watchdog.stdin.take());

    assert!(watchdog.wait().unwrap().success());
    let _ = target.wait();
    assert_eq!(kill(Pid::from_raw(escaped_pid), None), Err(Errno::ESRCH));
}

fn systemd_user_manager_available() -> bool {
    std::env::var_os("XDG_RUNTIME_DIR").is_some_and(|directory| {
        PathBuf::from(directory).join("systemd/private").exists()
            && Command::new("systemctl")
                .args(["--user", "show-environment"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success())
    })
}

fn wait_for_scope_cgroup(unit: &str) -> PathBuf {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let output = Command::new("systemctl")
            .args(["--user", "show", "--property=ControlGroup", "--value"])
            .arg(unit)
            .output()
            .unwrap();
        let relative = String::from_utf8(output.stdout).unwrap();
        if output.status.success() && !relative.trim().is_empty() {
            return PathBuf::from("/sys/fs/cgroup").join(relative.trim().trim_start_matches('/'));
        }
        assert!(Instant::now() < deadline, "scope did not become ready");
        std::thread::sleep(Duration::from_millis(25));
    }
}
