//! "Always run as Administrator" without a UAC prompt on every launch.
//!
//! A program can't elevate itself silently, and a manifest that demands
//! Administrator would put a UAC prompt in front of every start - including
//! the one at logon, where nobody may be around to click it (the PC would be
//! unreachable after a reboot). Windows' own answer is a Scheduled Task set to
//! run with the highest privileges: creating it needs Administrator once, and
//! after that anyone in the user's session can start it with no prompt.
//!
//! How it fits together: when the setting is on, an elevated host keeps the
//! task registered (`ensure`). A normal, non-elevated launch (Start menu, the
//! autostart entry at logon) checks the setting, starts the task and exits
//! (`hand_off`), so the app you end up with is always the elevated one.

use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::Command;

use anyhow::{anyhow, Context, Result};

/// Task name in Task Scheduler.
pub const TASK_NAME: &str = "Remote Control (Administrator)";
/// Passed by the task so an elevated copy never tries to hand off again.
pub const ARG: &str = "--admin-task";

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

fn schtasks(args: &[&str]) -> std::io::Result<std::process::Output> {
    Command::new("schtasks").args(args).creation_flags(CREATE_NO_WINDOW).output()
}

/// Whether the task is registered.
pub fn exists() -> bool {
    schtasks(&["/Query", "/TN", TASK_NAME]).map(|o| o.status.success()).unwrap_or(false)
}

/// Start the task (an elevated copy of the host). `true` if Windows accepted it.
pub fn hand_off() -> bool {
    exists() && schtasks(&["/Run", "/TN", TASK_NAME]).map(|o| o.status.success()).unwrap_or(false)
}

/// Register (or refresh) the task for `exe`. Needs Administrator.
pub fn ensure(exe: &Path) -> Result<()> {
    // No trigger: it only ever runs on demand. The settings matter - the
    // defaults would stop it after 72 hours and whenever the PC goes onto
    // battery - and "Parallel" lets a second launch start so it can tell the
    // first one to show its window.
    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo><Description>Starts Remote Control as Administrator without a UAC prompt.</Description></RegistrationInfo>
  <Principals><Principal id="Author"><LogonType>InteractiveToken</LogonType><RunLevel>HighestAvailable</RunLevel></Principal></Principals>
  <Settings>
    <MultipleInstancesPolicy>Parallel</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>false</StartWhenAvailable>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>7</Priority>
  </Settings>
  <Actions Context="Author"><Exec><Command>{exe}</Command><Arguments>{arg}</Arguments></Exec></Actions>
</Task>"#,
        exe = exe.display().to_string().replace('&', "&amp;").replace('<', "&lt;"),
        arg = ARG,
    );
    let path = std::env::temp_dir().join("rc-admin-task.xml");
    // Task Scheduler wants UTF-16 with a byte-order mark.
    let mut bytes = vec![0xFF, 0xFE];
    for u in xml.encode_utf16() {
        bytes.extend_from_slice(&u.to_le_bytes());
    }
    std::fs::write(&path, bytes).context("writing the task definition")?;
    let out = schtasks(&["/Create", "/TN", TASK_NAME, "/XML", &path.to_string_lossy(), "/F"])
        .context("running schtasks")?;
    let _ = std::fs::remove_file(&path);
    if out.status.success() {
        Ok(())
    } else {
        Err(anyhow!("schtasks: {}", String::from_utf8_lossy(&out.stderr).trim()))
    }
}

/// Remove the task if it's there. Needs Administrator.
pub fn remove() {
    if exists() {
        let _ = schtasks(&["/Delete", "/TN", TASK_NAME, "/F"]);
    }
}
