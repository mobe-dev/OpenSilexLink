//! OpenSilexLink daemon.
//!
//! Background service that speaks the Silex wire protocol and bridges it to the Linux
//! kernel's USB/IP stack (`vhci-hcd`). It owns device discovery, the connection/
//! reservation state, and a line-based JSON IPC surface consumed by the desktop client.
//!
//! Without a subcommand it runs in daemon mode. The `discover`/`probe`/`serve`
//! subcommands are bring-up helpers for checking the tunnel against real hardware.

mod daemon;
mod command;
mod credentials;
mod discovery;
mod ipc;
mod protocol;
mod settings;
mod usbip;
mod usbip_control;
mod wire;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Result};

use crate::daemon::Daemon;
use crate::ipc::IpcAddr;
use crate::protocol::udp_discovery as ud;
use crate::settings::SettingsStore;
use crate::usbip::proto::TCP_PORT;
use crate::usbip::server::UsbipServer;
use crate::usbip::session::{parse_device_descriptor, SilexSession};
use crate::usbip_control::{UsbipControl, UsbipOps};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("--version") | Some("-V") => {
            println!("silex-daemon {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some("--help") | Some("-h") | Some("help") => {
            println!("silex-daemon: Silex USB device server client\n\n\
                discover [--server IP] [--timeout SECONDS]\n\
                connect SERVER_IP [DEV_ID | --all] [--escalate sudo|pkexec|none] [--password-file PATH]\n\
                probe SERVER_IP DEV_ID --mac MAC [--password-file PATH]\n\
                serve SERVER_IP DEV_ID... --mac MAC [--bind IP] [--password-file PATH]\n\
                [--socket PATH] [--escalate sudo|pkexec|none]\n\n\
                connect keeps the USB device attached until Ctrl-C or SIGTERM.");
            Ok(())
        }
        Some("discover") => cmd_discover(&args[2..]).await,
        Some("connect") => cmd_connect(&args[2..]).await,
        Some("probe") => cmd_probe(&args[2..]).await,
        Some("serve") => cmd_serve(&args[2..]).await,
        _ => cmd_daemon(&args[1..]).await,
    }
}

/// Daemon mode: discovery + IPC server + orchestration.
/// `[--socket <path>] [--tcp <host:port>] [--escalate pkexec|sudo|none]`.
async fn cmd_daemon(args: &[String]) -> Result<()> {
    let escalation = get_flag(args, "--escalate").unwrap_or_else(|| "pkexec".to_string());
    if !matches!(escalation.as_str(), "pkexec" | "sudo" | "none") {
        bail!("--escalate must be sudo, pkexec, or none");
    }
    let addr = if let Some(tcp) = get_flag(args, "--tcp") {
        let (host, port) = tcp.rsplit_once(':').ok_or_else(|| anyhow::anyhow!("--tcp needs host:port"))?;
        IpcAddr::Inet(if host.is_empty() { "127.0.0.1".to_string() } else { host.to_string() }, port.parse()?)
    } else if let Some(sock) = get_flag(args, "--socket") {
        IpcAddr::Unix(PathBuf::from(sock))
    } else {
        IpcAddr::Unix(crate::ipc::default_socket_path())
    };
    let store = Arc::new(SettingsStore::new(None));
    let daemon = Daemon::with_escalation(store, &escalation);
    tracing::info!("silex-daemon running (IPC {addr:?}, escalation {escalation})");
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let result = tokio::select! {
        result = daemon.run(addr) => result,
        _ = tokio::signal::ctrl_c() => Ok(()),
        _ = terminate.recv() => Ok(()),
    };
    daemon.shutdown().await;
    result
}

/// `discover [--timeout <secs>]`: find Silex device servers on the LAN.
async fn cmd_discover(args: &[String]) -> Result<()> {
    let secs: f64 = get_flag(args, "--timeout").unwrap_or_else(|| "3".into()).parse()?;
    if !secs.is_finite() || secs <= 0.0 {
        bail!("--timeout must be a finite, positive number");
    }
    let duration = std::time::Duration::try_from_secs_f64(secs)?;
    let broadcast = get_flag(args, "--server")
        .or_else(|| get_flag(args, "--broadcast"))
        .unwrap_or_else(|| "255.255.255.255".to_string());
    let servers = discovery::discover(duration, &broadcast).await;
    if servers.is_empty() {
        println!("No device servers found.");
        return Ok(());
    }
    for srv in servers {
        println!(
            "{}  [{}  {}]  {}  {}",
            if srv.hostname.is_empty() { "?" } else { &srv.hostname },
            srv.mac_str(),
            srv.model,
            srv.ip,
            srv.version
        );
        for d in &srv.devices {
            println!(
                "    {}  {:?}  USB {:04x}:{:04x}  {}{}",
                d.dev_id, d.description, d.id_vendor, d.id_product, d.device_type(),
                if d.password_required() { "  [USB port password required]" } else { "" }
            );
        }
        if srv.devices.is_empty() {
            println!("    No USB devices attached.");
        }
    }
    Ok(())
}

/// Foreground headless client: discover, export on loopback, attach to vhci,
/// then detach and release the Silex reservation on Ctrl-C or SIGTERM.
async fn cmd_connect(args: &[String]) -> Result<()> {
    // Install handlers before discovery/attach, so a signal during setup is
    // remembered and still passes through reservation cleanup.
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let pos = positionals(args);
    if pos.is_empty() || pos.len() > 2 {
        bail!("usage: connect SERVER_IP [DEV_ID] [--escalate sudo|pkexec|none]");
    }
    let ip: std::net::Ipv4Addr = pos[0].parse()?;
    let server_ip = ip.to_string();
    let escalation = get_flag(args, "--escalate").unwrap_or_else(|| "sudo".into());
    if !matches!(escalation.as_str(), "sudo" | "pkexec" | "none") {
        bail!("--escalate must be sudo, pkexec, or none");
    }
    let servers = discovery::discover(std::time::Duration::from_secs(2), &server_ip).await;
    let srv = servers.into_iter().find(|s| s.ip == server_ip)
        .ok_or_else(|| anyhow::anyhow!("no Silex server answered at {server_ip}"))?;
    let devices = ud::logical_devices(&srv.devices);
    let all = args.iter().any(|arg| arg == "--all");
    if all && pos.len() != 1 {
        bail!("--all cannot be combined with DEV_ID");
    }
    let selected: Vec<_> = if all {
        devices.iter().collect()
    } else if let Some(id) = pos.get(1) {
        vec![devices.iter().find(|d| d.dev_id() == id)
            .ok_or_else(|| anyhow::anyhow!("{id} is not a primary USB device at {server_ip}"))?]
    } else {
        if devices.len() != 1 {
            bail!("found {} USB devices; specify DEV_ID or --all", devices.len());
        }
        vec![&devices[0]]
    };
    if selected.is_empty() { bail!("no USB devices are available at {server_ip}"); }
    let password = read_usb_password(args)?;
    for device in &selected {
        if device.entries.iter().any(|e| !e.owner_ip.is_empty()) {
            bail!("{} is already in use; release it from its current client first", device.dev_id());
        }
        if password.is_none() && device.entries.iter().any(|e| e.password_required()) {
            bail!("{} requires its USB port connection password; provide --password-file PATH", device.dev_id());
        }
    }
    let mac = parse_mac(&srv.mac_str())?;
    let exports: Vec<_> = selected.iter().enumerate()
        .map(|(i, device)| (format!("1-{}", i + 1), device.dev_id().to_string())).collect();
    let control = Arc::new(UsbipControl::new(&escalation));
    let ctl = control.clone();
    tokio::task::spawn_blocking(move || ctl.ensure_module()).await??;
    let server = Arc::new(UsbipServer::new(
        &server_ip, mac, None, ("127.0.0.1".into(), TCP_PORT), ud::PORT,
        exports.clone(),
    ).with_usb_password(password).with_device_speeds(&srv.devices));
    server.bind().await?;
    let exporter = server.clone();
    let task = tokio::spawn(async move { exporter.serve().await });
    let mut ports = Vec::new();
    let result: Result<()> = async {
        for (busid, dev_id) in &exports {
            let before = control.used_ports();
            let ctl = control.clone();
            let busid = busid.clone();
            tokio::task::spawn_blocking(move || ctl.attach(&busid)).await??;
            let mut new_port = None;
            for _ in 0..20 {
                new_port = control.used_ports().difference(&before).copied().min();
                if new_port.is_some() { break; }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            let port = new_port.ok_or_else(|| anyhow::anyhow!("vhci port could not be identified for {dev_id}"))?;
            ports.push(port);
            println!("Attached {dev_id} from {server_ip} on vhci port {port}.");
        }
        println!("Ctrl-C or SIGTERM to disconnect.");
        tokio::select! {
            _ = interrupt.recv() => {},
            _ = terminate.recv() => {},
        }
        Ok(())
    }.await;
    let mut detach_result = Ok(());
    for port in ports {
        let ctl = control.clone();
        let detached = tokio::task::spawn_blocking(move || ctl.detach(port)).await
            .map_err(anyhow::Error::from).and_then(|r| r);
        if detached.is_err() { detach_result = detached; }
    }
    server.shutdown().await;
    task.abort();
    result?;
    detach_result?;
    println!("Disconnected.");
    Ok(())
}

/// `probe <server_ip> <dev_id> --mac <mac>`: connect and read the device descriptor.
async fn cmd_probe(args: &[String]) -> Result<()> {
    let pos = positionals(args);
    if pos.len() < 2 {
        bail!("usage: probe <server_ip> <dev_id> --mac <mac>");
    }
    let server_ip = &pos[0];
    let dev_id = &pos[1];
    let mac = parse_mac(&get_flag(args, "--mac").ok_or_else(|| anyhow::anyhow!("--mac required"))?)?;

    let sess = SilexSession::new(server_ip, mac, dev_id, None, ud::PORT, std::time::Duration::ZERO)
        .with_usb_password(read_usb_password(args)?);
    println!("CONNECT {server_ip} {dev_id} (host ip {}) ...", sess.host_ip);
    sess.connect().await?;
    let result = async {
        let desc = sess.get_device_descriptor().await.map_err(|e| anyhow::anyhow!("{e}"))?;
        println!("  device descriptor: {}", hex::encode(&desc));
        let info = parse_device_descriptor(&desc)?;
        println!("  idVendor:idProduct = {:04x}:{:04x}", info.id_vendor, info.id_product);
        println!(
            "  bcdUSB={:04x}  bcdDevice={:04x}  Class={:02x}/{:02x}/{:02x}  MaxPacket0={}  Configs={}",
            info.bcd_usb,
            info.bcd_device,
            info.b_device_class,
            info.b_device_subclass,
            info.b_device_protocol,
            info.b_max_packet_size0,
            info.b_num_configurations
        );
        Ok::<(), anyhow::Error>(())
    }
    .await;
    sess.disconnect().await;
    println!("DISCONNECT ok.");
    result
}

/// `serve <server_ip> <dev_id...> --mac <mac> [--bind <ip>] [--host-ip <ip>]`.
async fn cmd_serve(args: &[String]) -> Result<()> {
    let pos = positionals(args);
    if pos.len() < 2 {
        bail!("usage: serve <server_ip> <dev_id...> --mac <mac> [--bind <ip>] [--host-ip <ip>]");
    }
    let server_ip = &pos[0];
    let mac = parse_mac(&get_flag(args, "--mac").ok_or_else(|| anyhow::anyhow!("--mac required"))?)?;
    let bind = get_flag(args, "--bind").unwrap_or_else(|| "127.0.0.1".to_string());
    let host_ip = get_flag(args, "--host-ip");
    let discovered = discovery::discover(std::time::Duration::from_secs(2), server_ip).await;
    let speeds = discovered.iter().find(|server| server.ip == *server_ip)
        .map(|server| server.devices.as_slice()).unwrap_or(&[]);
    let devices: Vec<(String, String)> = pos[1..]
        .iter()
        .enumerate()
        .map(|(i, d)| (format!("1-{}", i + 1), d.clone()))
        .collect();

    let server = Arc::new(UsbipServer::new(
        server_ip,
        mac,
        host_ip,
        (bind, TCP_PORT),
        ud::PORT,
        devices,
    ).with_usb_password(read_usb_password(args)?).with_device_speeds(speeds));
    let srv = server.clone();
    tokio::select! {
        r = srv.serve() => { r?; }
        _ = tokio::signal::ctrl_c() => {
            println!("\nshutting down - disconnecting active sessions ...");
        }
    }
    server.shutdown().await;
    Ok(())
}

// --------------------------------------------------------------------------- //
fn read_usb_password(args: &[String]) -> Result<Option<Vec<u8>>> {
    let Some(path) = get_flag(args, "--password-file") else { return Ok(None) };
    let mut password = std::fs::read(path)?;
    if password.last() == Some(&b'\n') {
        password.pop();
        if password.last() == Some(&b'\r') { password.pop(); }
    }
    if password.len() > 16 || password.contains(&0) {
        bail!("USB port password file must contain at most 16 bytes and no NUL (one final newline is allowed)");
    }
    Ok(Some(password))
}

//  Tiny argument helpers
// --------------------------------------------------------------------------- //
fn get_flag(args: &[String], name: &str) -> Option<String> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == name {
            return it.next().cloned();
        }
    }
    None
}

fn positionals(args: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--all" {
            i += 1;
        } else if args[i].starts_with("--") {
            i += 2; // skip flag and its value
        } else {
            out.push(args[i].clone());
            i += 1;
        }
    }
    out
}

fn parse_mac(s: &str) -> Result<[u8; 6]> {
    let raw = s.replace(':', "");
    let b = hex::decode(raw).map_err(|_| anyhow::anyhow!("invalid MAC hex"))?;
    if b.len() != 6 {
        bail!("MAC must be 6 bytes");
    }
    let mut m = [0u8; 6];
    m.copy_from_slice(&b);
    Ok(m)
}
