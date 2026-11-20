//! `wishlang service install|uninstall|status`: keep the daemon running from login on, with
//! launchd (macOS) or systemd --user (Linux). Without a service the daemon starts with the first
//! editor and exits when idle.

use anyhow::{Context, Result, bail};
use std::path::PathBuf;
use std::process::Command;

const LABEL: &str = "dev.wishlang.daemon";

fn exe() -> Result<PathBuf> {
    Ok(std::env::current_exe()?.canonicalize()?)
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
}

fn unit_path() -> Result<PathBuf> {
    if cfg!(target_os = "macos") {
        Ok(home().join(format!("Library/LaunchAgents/{LABEL}.plist")))
    } else if cfg!(target_os = "linux") {
        Ok(home().join(".config/systemd/user/wishlang.service"))
    } else {
        bail!("`wishlang service` supports launchd (macOS) and systemd (Linux)")
    }
}

/// The unit file for this platform, running `exe daemon --no-idle-exit`.
fn unit(exe: &std::path::Path) -> String {
    let exe = exe.display();
    if cfg!(target_os = "macos") {
        let log = crate::daemon::home().join("daemon.out.log");
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{LABEL}</string>
  <key>ProgramArguments</key><array><string>{exe}</string><string>daemon</string><string>--no-idle-exit</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardErrorPath</key><string>{}</string>
</dict>
</plist>
"#,
            log.display()
        )
    } else {
        format!("[Unit]\nDescription=wishlang daemon\n\n[Service]\nExecStart={exe} daemon --no-idle-exit\nRestart=on-failure\n\n[Install]\nWantedBy=default.target\n")
    }
}

fn run(cmd: &mut Command) -> Result<bool> {
    let out = cmd.output().with_context(|| format!("running {:?}", cmd.get_program()))?;
    Ok(out.status.success())
}

fn uid() -> String {
    Command::new("id").arg("-u").output().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default()
}

pub fn install() -> Result<String> {
    let path = unit_path()?;
    std::fs::create_dir_all(path.parent().unwrap())?;
    std::fs::create_dir_all(crate::daemon::home())?;
    // A daemon started by an editor would hold the socket; let the service's take over.
    crate::daemon::stop();
    std::fs::write(&path, unit(&exe()?))?;
    if cfg!(target_os = "macos") {
        let target = format!("gui/{}", uid());
        let _ = run(Command::new("launchctl").args(["bootout", &format!("{target}/{LABEL}")]));
        if !run(Command::new("launchctl").args(["bootstrap", &target]).arg(&path))? {
            bail!("launchctl bootstrap failed for {}", path.display());
        }
    } else {
        run(Command::new("systemctl").args(["--user", "daemon-reload"]))?;
        if !run(Command::new("systemctl").args(["--user", "enable", "--now", "wishlang.service"]))? {
            bail!("systemctl --user enable --now wishlang.service failed");
        }
    }
    Ok(format!("installed {}: the daemon now runs from login on", path.display()))
}

pub fn uninstall() -> Result<String> {
    let path = unit_path()?;
    if cfg!(target_os = "macos") {
        let _ = run(Command::new("launchctl").args(["bootout", &format!("gui/{}/{LABEL}", uid())]));
    } else {
        let _ = run(Command::new("systemctl").args(["--user", "disable", "--now", "wishlang.service"]));
    }
    if path.exists() {
        std::fs::remove_file(&path)?;
        Ok(format!("removed {}; editors start the daemon on demand again", path.display()))
    } else {
        Ok("no service installed".into())
    }
}

/// Whether the unit is installed, and which binary it runs.
pub fn status() -> Result<serde_json::Value> {
    let path = unit_path()?;
    let text = std::fs::read_to_string(&path).ok();
    let current = exe()?.display().to_string();
    Ok(serde_json::json!({
        "installed": text.is_some(),
        "unit": path,
        "runs_this_binary": text.as_deref().is_some_and(|t| t.contains(&current)),
        "daemon": crate::daemon::status(),
    }))
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_unit_runs_the_daemon_without_idle_exit() {
        let u = super::unit(std::path::Path::new("/opt/bin/wishlang"));
        assert!(u.contains("/opt/bin/wishlang") && u.contains("daemon") && u.contains("--no-idle-exit"));
    }
}
