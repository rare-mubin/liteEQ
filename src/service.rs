//! Managing the background service from the TUI: starting it detached, and the
//! optional systemd user unit that starts it at login.

use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};

const UNIT: &str = "liteeq.service";

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

fn unit_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).or_else(|| home().map(|h| h.join(".config")))?;
    Some(base.join("systemd").join("user").join(UNIT))
}

pub fn log_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME").map(PathBuf::from).or_else(|| home().map(|h| h.join(".local").join("state")))?;
    Some(base.join("liteeq").join("daemon.log"))
}

fn systemctl(args: &[&str]) -> Option<(bool, String)> {
    let out = Command::new("systemctl").arg("--user").args(args).stderr(Stdio::null()).output().ok()?;
    Some((out.status.success(), String::from_utf8_lossy(&out.stdout).trim().to_string()))
}

/// Start `lite-eq daemon` detached from this terminal, so it survives the TUI.
pub fn spawn_daemon(args: &[String]) -> std::io::Result<()> {
    if unit_enabled() == Some(true) {
        // Let systemd own it: logs go to the journal and it restarts on failure.
        if let Some((true, _)) = systemctl(&["start", UNIT]) {
            return Ok(());
        }
    }
    let exe = std::env::current_exe()?;
    let mut cmd = Command::new(exe);
    cmd.arg("daemon").args(args).stdin(Stdio::null());
    match log_path().and_then(|p| {
        std::fs::create_dir_all(p.parent()?).ok()?;
        std::fs::File::create(p).ok()
    }) {
        Some(log) => {
            cmd.stdout(log.try_clone()?).stderr(log);
        }
        None => {
            cmd.stdout(Stdio::null()).stderr(Stdio::null());
        }
    }
    // SAFETY: setsid is async-signal-safe; it detaches the child from our
    // session so closing the terminal does not take the service with it.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut child = cmd.spawn()?;
    std::thread::spawn(move || {
        let _ = child.wait(); // reap it when it exits
    });
    Ok(())
}

/// `None` when systemd's user manager is not available.
pub fn unit_enabled() -> Option<bool> {
    let (ok, out) = systemctl(&["is-enabled", UNIT])?;
    Some(ok && out == "enabled")
}

/// Whether we can manage a user unit at all.
pub fn systemd_available() -> bool {
    systemctl(&["--version"]).is_some_and(|(ok, _)| ok)
}

fn unit_text(exe: &std::path::Path) -> String {
    format!(
        "[Unit]\n\
         Description=liteEQ bass enhancer (audio effect service)\n\
         After=pipewire.service pipewire-pulse.service wireplumber.service\n\
         Wants=pipewire.service pipewire-pulse.service\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={} daemon\n\
         Restart=on-failure\n\
         RestartSec=2\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        exe.display()
    )
}

/// Enable or disable starting the service at login.
pub fn set_autostart(enable: bool) -> Result<(), String> {
    if !systemd_available() {
        return Err("systemd user session not available".into());
    }
    let path = unit_path().ok_or("cannot locate the systemd user directory")?;
    if enable {
        let exe = std::env::current_exe().and_then(|p| p.canonicalize()).map_err(|e| e.to_string())?;
        std::fs::create_dir_all(path.parent().ok_or("bad unit path")?).map_err(|e| e.to_string())?;
        std::fs::write(&path, unit_text(&exe)).map_err(|e| e.to_string())?;
        systemctl(&["daemon-reload"]);
        match systemctl(&["enable", UNIT]) {
            Some((true, _)) => Ok(()),
            _ => Err("systemctl enable failed".into()),
        }
    } else {
        systemctl(&["disable", UNIT]);
        let _ = std::fs::remove_file(&path);
        systemctl(&["daemon-reload"]);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_file_points_at_the_daemon() {
        let text = unit_text(std::path::Path::new("/opt/lite-eq"));
        assert!(text.contains("ExecStart=/opt/lite-eq daemon"));
        assert!(text.contains("WantedBy=default.target"));
        assert!(text.contains("Restart=on-failure"));
    }
}
