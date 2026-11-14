//! Privileged attach/detach of the USB/IP device (vhci) for the daemon.
//!
//! `usbip attach/detach` and `modprobe vhci-hcd` need root, obtained at runtime via an
//! escalation: `pkexec` (desktop polkit prompt), `sudo` (`sudo -n`, non-interactive for
//! headless/test setups), or `none` (already root).
//!
//! One prompt per session instead of per connect: if the privileged helper is installed
//! (see packaging/), pkexec runs through it (a polkit action with `auth_admin_keep`);
//! otherwise it falls back to `pkexec usbip` (a prompt per connect). The USB/IP *server*
//! runs unprivileged inside the daemon; only the client-side attach to vhci needs root.

use std::collections::HashSet;
use std::path::Path;
use std::process::Command;

use anyhow::{bail, Result};
use crate::command::{CommandRunner, COMMAND_TIMEOUT};

pub const REMOTE: &str = "127.0.0.1";

const SEARCH_DIRS: &[&str] = &["/usr/sbin", "/sbin", "/usr/bin", "/bin", "/usr/local/sbin", "/usr/local/bin"];

/// Install location of the privileged helper (must match exec.path in the polkit policy).
pub const HELPER_PATH: &str = "/usr/libexec/silex-usbip-helper";

/// Privileged usbip operations (injectable so the daemon is testable with a fake).
pub trait UsbipOps: Send + Sync {
    fn ensure_idle(&self) -> Result<()> { Ok(()) }
    fn ensure_module(&self) -> Result<()>;
    fn used_ports(&self) -> HashSet<i32>;
    fn attach(&self, busid: &str) -> Result<()>;
    fn detach(&self, port: i32) -> Result<()>;
    fn ensure_detachable(&self, _port: i32) -> Result<()> { Ok(()) }
}

pub fn find_helper() -> Option<String> {
    let cand = std::env::var("SILEX_USBIP_HELPER").unwrap_or_else(|_| HELPER_PATH.to_string());
    if Path::new(&cand).exists() {
        Some(cand)
    } else {
        None
    }
}

/// Absolute path to `prog` (pkexec requires absolute paths, no PATH lookup).
pub fn resolve(prog: &str) -> String {
    if prog.starts_with('/') {
        return prog.to_string();
    }
    if let Ok(path) = std::env::var("PATH") {
        for dir in path.split(':') {
            let cand = format!("{dir}/{prog}");
            if Path::new(&cand).exists() {
                return cand;
            }
        }
    }
    for dir in SEARCH_DIRS {
        let cand = format!("{dir}/{prog}");
        if Path::new(&cand).exists() {
            return cand;
        }
    }
    prog.to_string()
}

fn escalation_prefix(mode: &str) -> Result<Vec<String>> {
    match mode {
        "pkexec" => Ok(vec![resolve("pkexec")]),
        "sudo" => Ok(vec![resolve("sudo"), "-n".to_string()]),
        "none" => Ok(Vec::new()),
        other => bail!("unknown escalation mode: {other:?}"),
    }
}

pub struct UsbipControl {
    escalation: String,
    remote: String,
    helper: Option<String>,
    runner: CommandRunner,
}

impl UsbipControl {
    pub fn new(escalation: &str) -> Self {
        Self {
            escalation: escalation.to_string(),
            remote: REMOTE.to_string(),
            helper: find_helper(),
            runner: CommandRunner::default(),
        }
    }

    /// Build the command line including escalation.
    fn build_cmd(&self, argv: &[&str]) -> Vec<String> {
        if self.escalation == "pkexec" {
            if let Some(helper) = &self.helper {
                let helper_args: Option<Vec<String>> = match argv.first().copied() {
                    Some("usbip") => Some(argv[1..].iter().map(|s| s.to_string()).collect()),
                    Some("modprobe") => Some(vec!["modprobe".to_string()]),
                    _ => None,
                };
                if let Some(hargs) = helper_args {
                    let mut cmd = vec![resolve("pkexec"), helper.clone()];
                    cmd.extend(hargs);
                    return cmd;
                }
            }
        }
        let mut cmd = escalation_prefix(&self.escalation).unwrap_or_default();
        cmd.push(resolve(argv[0]));
        cmd.extend(argv[1..].iter().map(|s| s.to_string()));
        cmd
    }

    fn run_priv(&self, argv: &[&str]) -> Result<()> {
        let cmd = self.build_cmd(argv);
        // Allow time for an interactive polkit prompt on desktop sessions.
        let deadline = if self.escalation == "pkexec" {
            std::time::Duration::from_secs(120)
        } else { COMMAND_TIMEOUT };
        let out = self.runner.run(Command::new(&cmd[0]).args(&cmd[1..]), deadline);
        match out {
            Ok(o) if o.status.success() => Ok(()),
            Ok(o) => {
                let detail = String::from_utf8_lossy(if !o.stderr.is_empty() { &o.stderr } else { &o.stdout });
                bail!("{} failed: {}", argv.join(" "), detail.trim());
            }
            Err(e) => bail!("{} failed: {e}", argv.join(" ")),
        }
    }
}

impl UsbipOps for UsbipControl {
    fn ensure_idle(&self) -> Result<()> { self.runner.ensure_idle() }
    fn ensure_detachable(&self, port: i32) -> Result<()> {
        let root = Path::new("/sys/devices/platform");
        let mut busid = None;
        for entry in std::fs::read_dir(root)? {
            let entry = entry?;
            if !entry.file_name().to_string_lossy().starts_with("vhci_hcd.") { continue; }
            if let Ok(status) = std::fs::read_to_string(entry.path().join("status")) {
                // An interrupted import may never have created a local USB
                // device. There can be no mounted storage on that port yet.
                if importing_without_device(&status, port) { return Ok(()); }
                busid = busid.or_else(|| local_busid(&status, port));
            }
        }
        let busid = busid.ok_or_else(|| anyhow::anyhow!("cannot identify USB/IP port {port}; refusing to detach"))?;
        let usb_path = std::fs::canonicalize(format!("/sys/bus/usb/devices/{busid}"))?;
        for line in std::fs::read_to_string("/proc/self/mountinfo")?.lines() {
            let fields: Vec<_> = line.split_whitespace().collect();
            if fields.len() < 5 { continue; }
            if let Ok(block) = std::fs::canonicalize(format!("/sys/dev/block/{}", fields[2])) {
                if block.starts_with(&usb_path) {
                    bail!("Unmount {} before disconnecting this USB device", fields[4]);
                }
            }
        }
        for line in std::fs::read_to_string("/proc/swaps")?.lines().skip(1) {
            if let Some(source) = line.split_whitespace().next() {
                if let Ok(device) = std::fs::canonicalize(source) {
                    if let Some(name) = device.file_name() {
                        if let Ok(block) = std::fs::canonicalize(Path::new("/sys/class/block").join(name)) {
                            if block.starts_with(&usb_path) { bail!("Disable swap on {source} before disconnecting"); }
                        }
                    }
                }
            }
        }
        Ok(())
    }
    fn ensure_module(&self) -> Result<()> {
        // Only load if not already loaded -> saves a second pkexec prompt.
        if Path::new("/sys/module/vhci_hcd").exists() {
            return Ok(());
        }
        self.run_priv(&["modprobe", "vhci-hcd"])
    }

    fn used_ports(&self) -> HashSet<i32> {
        // `usbip port` also reads USB device attributes. Those reads can wait
        // for a reset to finish, which in turn needs this daemon to answer URBs.
        // The vhci status table contains all occupancy data without device I/O.
        let mut ports = HashSet::new();
        if let Ok(entries) = std::fs::read_dir("/sys/devices/platform") {
            for entry in entries.flatten() {
                if !entry.file_name().to_string_lossy().starts_with("vhci_hcd.") { continue; }
                if let Ok(status) = std::fs::read_to_string(entry.path().join("status")) {
                    ports.extend(parse_vhci_used_ports(&status));
                }
            }
        }
        ports
    }

    fn attach(&self, busid: &str) -> Result<()> {
        self.run_priv(&["usbip", "attach", "-r", &self.remote, "-b", busid])
    }

    fn detach(&self, port: i32) -> Result<()> {
        let p = port.to_string();
        self.run_priv(&["usbip", "detach", "-p", &p])
    }
}

fn local_busid(status: &str, port: i32) -> Option<String> {
    status.lines().skip(1).find_map(|line| {
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.len() >= 7 && fields[1].parse::<i32>().ok() == Some(port) && fields[6] != "0-0" {
            Some(fields[6].to_string())
        } else { None }
    })
}

fn importing_without_device(status: &str, port: i32) -> bool {
    status.lines().skip(1).any(|line| {
        let fields: Vec<_> = line.split_whitespace().collect();
        fields.len() >= 7 && fields[1].parse::<i32>().ok() == Some(port)
            && fields[2].parse::<u32>().ok() == Some(5) && fields[6] == "0-0"
    })
}

/// Extract busy vhci ports from the output of `usbip port`.
/// Lines look like: `Port 00: <Port in Use> ...`.
#[cfg(test)]
pub fn parse_used_ports(text: &str) -> HashSet<i32> {
    let mut ports = HashSet::new();
    for line in text.lines() {
        let line = line.trim_start();
        if let Some(rest) = line.strip_prefix("Port ") {
            if let Some(colon) = rest.find(':') {
                let (num, after) = rest.split_at(colon);
                if after.contains("<Port in Use>") {
                    if let Ok(n) = num.trim().parse::<i32>() {
                        ports.insert(n);
                    }
                }
            }
        }
    }
    ports
}

fn parse_vhci_used_ports(status: &str) -> HashSet<i32> {
    status.lines().skip(1).filter_map(|line| {
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.len() >= 7 && matches!(fields[2].parse::<u32>(), Ok(5 | 6)) {
            fields[1].parse().ok()
        } else { None }
    }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vhci_status_maps_port_to_local_usb_path() {
        let status = "hub port sta spd dev sockfd local_busid\nhs 0000 006 002 00010002 000003 2-1\nss 0008 006 005 00010003 000003 3-1\nhs 0001 004 000 00000000 000000 0-0\n";
        assert_eq!(local_busid(status, 8).as_deref(), Some("3-1"));
        assert_eq!(local_busid(status, 1), None);
    }

    #[test]
    fn interrupted_import_without_a_usb_device_can_be_detached() {
        let status = "hub port sta spd dev sockfd local_busid\nhs 0000 005 002 00010002 000003 0-0\nss 0008 006 005 00010003 000003 3-1\nhs 0001 004 000 00000000 000000 0-0\n";
        assert!(importing_without_device(status, 0));
        assert!(!importing_without_device(status, 8));
        assert!(!importing_without_device(status, 1));
        assert!(!importing_without_device(status, 42));
    }

    #[test]
    fn vhci_status_includes_importing_ports_and_ignores_free_or_failed_ports() {
        let status = "hub port sta spd dev sockfd local_busid\nhs 0000 005 002 00010002 000003 0-0\nss 0008 006 005 00010003 000003 3-1\nhs 0001 004 000 00000000 000000 0-0\nhs 0002 007 000 00000000 000000 0-0\n";
        assert_eq!(parse_vhci_used_ports(status), HashSet::from([0, 8]));
    }

    #[test]
    fn parse_ports() {
        let text = "\
Imported USB devices
====================
Port 00: <Port in Use> at High Speed(480Mbps)
       unknown vendor : unknown product (0930:6544)
Port 01: <Port Available>
Port 02: <Port in Use> at Full Speed(12Mbps)
";
        let ports = parse_used_ports(text);
        assert_eq!(ports, HashSet::from([0, 2]));
    }

    #[test]
    fn build_cmd_without_helper_uses_escalation() {
        let ctl = UsbipControl { escalation: "sudo".into(), remote: REMOTE.into(), helper: None, runner: CommandRunner::default() };
        let cmd = ctl.build_cmd(&["usbip", "attach", "-r", "127.0.0.1", "-b", "1-1"]);
        assert_eq!(cmd[1], "-n");
        assert!(cmd[0].ends_with("sudo"));
        assert!(cmd.iter().any(|c| c == "attach") && cmd.iter().any(|c| c == "1-1"));
    }

    #[test]
    fn build_cmd_none_is_direct() {
        let ctl = UsbipControl { escalation: "none".into(), remote: REMOTE.into(), helper: None, runner: CommandRunner::default() };
        let cmd = ctl.build_cmd(&["usbip", "detach", "-p", "0"]);
        assert!(cmd[0].ends_with("usbip"));
        assert_eq!(cmd.last().unwrap(), "0");
    }

    #[test]
    fn build_cmd_with_helper_routes_through_pkexec() {
        let ctl = UsbipControl {
            escalation: "pkexec".into(),
            remote: REMOTE.into(),
            helper: Some("/usr/libexec/silex-usbip-helper".into()),
            runner: CommandRunner::default(),
        };
        let cmd = ctl.build_cmd(&["usbip", "attach", "-r", "127.0.0.1", "-b", "1-2"]);
        assert!(cmd[0].ends_with("pkexec"));
        assert_eq!(cmd[1], "/usr/libexec/silex-usbip-helper");
        assert_eq!(cmd[2], "attach");
        let m = ctl.build_cmd(&["modprobe", "vhci-hcd"]);
        assert_eq!(m[2], "modprobe");
    }
}
