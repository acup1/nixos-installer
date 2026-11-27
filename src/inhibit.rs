use crate::SYSTEM_PATH;
use std::{
    collections::BTreeSet,
    io::Read,
    os::fd::AsRawFd,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
};

const NAME: &str = "nixos-installer";
const REASON: &str = "Keep the system awake during NixOS installation";

pub struct SleepLock(Child);

impl SleepLock {
    fn acquire(command: &mut Command) -> Option<Self> {
        // The helper holds the lock until this process closes its private pipe.
        let mut child = command
            .env("PATH", SYSTEM_PATH)
            .args(["/bin/sh", "-c", "printf R; exec cat >/dev/null"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let mut stdout = child.stdout.take().expect("piped stdout");
        let mut fd = libc::pollfd {
            fd: stdout.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let mut marker = [0];
        let ready = unsafe { libc::poll(&mut fd, 1, 2000) } > 0
            && fd.revents & libc::POLLIN != 0
            && stdout.read_exact(&mut marker).is_ok()
            && marker == [b'R'];
        if ready {
            Some(Self(child))
        } else {
            drop(child.stdin.take());
            let _ = child.kill();
            let _ = child.wait();
            None
        }
    }
}

impl Drop for SleepLock {
    fn drop(&mut self) {
        // Child::wait closes stdin first, letting the helper release the lock.
        let _ = self.0.wait();
    }
}

fn program(name: &str) -> Option<PathBuf> {
    SYSTEM_PATH
        .split(':')
        .map(|dir| Path::new(dir).join(name))
        .find(|path| path.is_file())
}

fn loginctl(path: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new(path)
        .arg("--no-pager")
        .args(args)
        .output()
        .ok()?;
    output.status.success().then_some(())?;
    String::from_utf8(output.stdout).ok()
}

fn gnome_locks() -> Option<Vec<SleepLock>> {
    let gnome = program("gnome-session-inhibit")?;
    let runuser = program("runuser")?;
    let env = program("env")?;
    let login = program("loginctl")?;
    let sessions = loginctl(&login, &["list-sessions", "--no-legend"])?;
    let mut users = BTreeSet::new();
    let mut locks = Vec::new();
    // The graphical session may be inactive while the terminal runs on another VT.
    for session in sessions
        .lines()
        .filter_map(|line| line.split_whitespace().next())
    {
        let Some(info) = loginctl(
            &login,
            &[
                "show-session",
                "--property=Remote",
                "--property=Type",
                "--property=User",
                "--property=Name",
                session,
            ],
        ) else {
            continue;
        };
        let property = |key| {
            info.lines().find_map(|line| {
                let (name, value) = line.split_once('=')?;
                (name == key).then_some(value)
            })
        };
        if property("Remote") != Some("no") || !matches!(property("Type"), Some("wayland" | "x11"))
        {
            continue;
        }
        let (Some(uid), Some(user)) = (
            property("User").and_then(|value| value.parse::<u32>().ok()),
            property("Name"),
        ) else {
            continue;
        };
        let runtime = format!("/run/user/{uid}");
        let bus = format!("{runtime}/bus");
        if !Path::new(&bus).exists() || !users.insert(uid) {
            continue;
        }
        locks.extend(SleepLock::acquire(
            Command::new(&runuser)
                .args(["-u", user, "--"])
                .arg(&env)
                .arg(format!("XDG_RUNTIME_DIR={runtime}"))
                .arg(format!("DBUS_SESSION_BUS_ADDRESS=unix:path={bus}"))
                .arg(&gnome)
                // gnome-session-inhibit treats the first non-option as the command.
                .args([
                    "--inhibit=suspend:idle",
                    "--app-id",
                    NAME,
                    "--reason",
                    REASON,
                ]),
        ));
    }
    Some(locks)
}

pub fn keep_awake() -> Vec<SleepLock> {
    let mut locks = Vec::new();
    if let Some(inhibit) = program("systemd-inhibit") {
        locks.extend(SleepLock::acquire(Command::new(inhibit).args([
            "--what=sleep:idle",
            "--mode=block",
            "--who",
            NAME,
            "--why",
            REASON,
            "--",
        ])));
    }
    locks.extend(gnome_locks().into_iter().flatten());
    locks
}
