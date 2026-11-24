//! trio-start: the login launcher. Starts `trio open <room> --no-browser` for each room listed in
//! ~/.trio/autostart.txt, with no console window, logging to ~/.trio/trio.log, then exits.
//!
//! autostart.txt: one room dir per line; `# comments` and blank lines are skipped; an optional
//! trailing ` @<port>` picks the port (default 8964, then 8965, ...).
#![windows_subsystem = "windows"]

use std::fs::OpenOptions;
use std::path::PathBuf;
use std::process::{Command, Stdio};

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

fn main() {
    let Some(home) = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")).map(PathBuf::from) else { return };
    let base = home.join(".trio");
    let exe = base.join("bin").join(format!("trio{}", std::env::consts::EXE_SUFFIX));
    let Ok(list) = std::fs::read_to_string(base.join("autostart.txt")) else { return };
    let mut next_port = 8964u16;
    for line in list.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
        let (dir, port) = match line.rsplit_once(" @") {
            Some((d, p)) => (d.trim(), p.trim().parse().unwrap_or(next_port)),
            None => (line, next_port),
        };
        next_port = port.saturating_add(1);
        let Ok(log) = OpenOptions::new().create(true).append(true).open(base.join("trio.log")) else { return };
        let Ok(err) = log.try_clone() else { return };
        let mut cmd = Command::new(&exe);
        cmd.args(["open", dir, "--no-browser", "--port", &port.to_string()]).stdin(Stdio::null()).stdout(log).stderr(err);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        let _ = cmd.spawn();
    }
}
