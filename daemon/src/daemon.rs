//! `silex-daemon` orchestration: discovery, state, IPC surface, connect/disconnect,
//! auto-connect/-reconnect, and the host-to-host "request use" peer protocol.
//!
//! The daemon holds the truth (which device servers + USB devices exist and their
//! status) and pushes changes as a `servers` event to subscribed clients (the GUI).

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, OnceLock};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use serde_json::{json, Map, Value};
use tokio::task::JoinHandle;

use crate::discovery::{local_ip_towards, DeviceServer, DiscoveryListener};
use crate::credentials::CredentialsStore;
use crate::command::{CommandRunner, COMMAND_TIMEOUT};
use crate::ipc::{Handler, IpcAddr, IpcServer, Peer};
use crate::protocol::peer::{self as peerproto, PeerEndpoint, PeerMessage, PeerSender};
use crate::protocol::udp_discovery as ud;
use crate::settings::SettingsStore;
use crate::usbip::proto::TCP_PORT;
use crate::usbip::server::UsbipServer;
use crate::usbip_control::{UsbipControl, UsbipOps};

pub const STATUS_AVAILABLE: &str = "available";
pub const STATUS_CONNECTED: &str = "connected";
pub const STATUS_IN_USE: &str = "in_use";
pub const STATUS_RECONNECTING: &str = "reconnecting";

const REQUEST_ACK_TIMEOUT: Duration = Duration::from_secs(8);
const REQUEST_RESULT_TIMEOUT: Duration = Duration::from_secs(180);
const RECONNECT_CHECK_INTERVAL: Duration = Duration::from_secs(3);
const RECONNECT_BACKOFF_MS: &[u64] = &[500, 1000, 2000, 4000, 8000];
const DISCONNECT_REFRESH_TIMEOUT: Duration = Duration::from_secs(12);
const RELEASE_OPTIMISTIC_WINDOW: Duration = Duration::from_secs(15);

type Key = (String, String); // (server_mac, dev_id)

pub fn device_key(d: &ud::UsbDeviceEntry) -> String {
    format!("{:04x}:{:04x}:{}", d.id_vendor, d.id_product, d.description)
}

fn device_identity(d: &ud::UsbDeviceEntry) -> String {
    format!("{}:{}", device_key(d), d.tag_base())
}

#[derive(Clone)]
struct Connection {
    server_mac: String,
    dev_id: String,
    busid: String,
    port: Option<i32>,
    description: String, // for notices when the device disappears
    entry: ud::UsbDeviceEntry,
}

struct Outgoing {
    acked: bool,
    timer: Option<JoinHandle<()>>,
    #[allow(dead_code)]
    owner: String,
}

struct GraceTimer {
    handle: JoinHandle<()>,
    addr: SocketAddr,
    msg: PeerMessage,
}

pub struct Daemon {
    credentials: CredentialsStore,
    store: Arc<SettingsStore>,
    usbip: Arc<dyn UsbipOps>,
    start_usbip_server: bool,
    ipc: OnceLock<Arc<IpcServer>>,
    listener: Mutex<Option<Arc<DiscoveryListener>>>,
    peer: Mutex<Option<Arc<dyn PeerSender>>>,
    peer_user: String,

    servers: Mutex<HashMap<String, DeviceServer>>, // key: server IP
    last_seen: Mutex<HashMap<String, Instant>>,
    our_ip: Mutex<HashMap<String, String>>,
    host_cache: Mutex<HashMap<String, String>>,
    connections: Mutex<HashMap<Key, Connection>>,
    failed_attach_ports: Mutex<Option<HashSet<i32>>>, // occupancy before an uncertain attach
    busids: Mutex<HashMap<String, String>>,
    usbip_server: Mutex<Option<Arc<UsbipServer>>>,
    recently_released: Mutex<HashMap<Key, Instant>>,
    autoconnect_pending: Mutex<HashSet<Key>>,
    manual_disconnects: Mutex<HashSet<Key>>,
    reconnecting: Mutex<HashMap<Key, Connection>>,
    startup_done: Mutex<HashSet<Key>>, // (server MAC, stable device identity)
    export_identities: Mutex<HashMap<String, String>>,
    pending_requests: Mutex<HashMap<Key, (SocketAddr, PeerMessage)>>,
    grace_timers: Mutex<HashMap<Key, GraceTimer>>,
    outgoing: Mutex<HashMap<Key, Outgoing>>,
    connect_lock: tokio::sync::Mutex<()>,
    shutting_down: AtomicBool,
    export_mac: Mutex<Option<String>>,
    export_task: Mutex<Option<JoinHandle<()>>>,
}

fn local_user() -> String {
    std::env::var("USER")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| hostname())
        .unwrap_or_else(|| "silex".to_string())
}

fn hostname() -> Option<String> {
    std::fs::read_to_string("/proc/sys/kernel/hostname").ok().map(|s| s.trim().to_string())
}

fn mac_array(mac: &[u8]) -> Result<[u8; 6]> {
    if mac.len() != 6 {
        bail!("device MAC unknown");
    }
    let mut m = [0u8; 6];
    m.copy_from_slice(mac);
    Ok(m)
}

impl Daemon {
    pub fn new(store: Arc<SettingsStore>, usbip: Arc<dyn UsbipOps>, start_usbip_server: bool) -> Arc<Self> {
        Arc::new(Self {
            credentials: CredentialsStore::new(store.credentials_path()),
            store,
            usbip,
            start_usbip_server,
            ipc: OnceLock::new(),
            listener: Mutex::new(None),
            peer: Mutex::new(None),
            peer_user: local_user(),
            servers: Mutex::new(HashMap::new()),
            last_seen: Mutex::new(HashMap::new()),
            our_ip: Mutex::new(HashMap::new()),
            host_cache: Mutex::new(HashMap::new()),
            connections: Mutex::new(HashMap::new()),
            failed_attach_ports: Mutex::new(None),
            busids: Mutex::new(HashMap::new()),
            usbip_server: Mutex::new(None),
            recently_released: Mutex::new(HashMap::new()),
            autoconnect_pending: Mutex::new(HashSet::new()),
            manual_disconnects: Mutex::new(HashSet::new()),
            reconnecting: Mutex::new(HashMap::new()),
            startup_done: Mutex::new(HashSet::new()),
            export_identities: Mutex::new(HashMap::new()),
            pending_requests: Mutex::new(HashMap::new()),
            grace_timers: Mutex::new(HashMap::new()),
            outgoing: Mutex::new(HashMap::new()),
            connect_lock: tokio::sync::Mutex::new(()),
            shutting_down: AtomicBool::new(false),
            export_mac: Mutex::new(None),
            export_task: Mutex::new(None),
        })
    }

    /// Convenience constructor for production: real usbip control.
    pub fn with_escalation(store: Arc<SettingsStore>, escalation: &str) -> Arc<Self> {
        Self::new(store, Arc::new(UsbipControl::new(escalation)), true)
    }

    fn attach_ipc(self: &Arc<Self>, addr: IpcAddr) -> Arc<IpcServer> {
        let weak = Arc::downgrade(self);
        let handler: Handler = Arc::new(move |method, params, peer| {
            let weak = weak.clone();
            Box::pin(async move {
                match weak.upgrade() {
                    Some(d) => d.handle(method, params, peer).await,
                    None => bail!("daemon shut down"),
                }
            })
        });
        let server = Arc::new(IpcServer::new(addr, handler));
        let _ = self.ipc.set(server.clone());
        server
    }

    /// Start all background loops and serve IPC (runs until the listener stops).
    pub async fn run(self: &Arc<Self>, addr: IpcAddr) -> Result<()> {
        let server = self.attach_ipc(addr);

        // Discovery: persistent listener + consumer + poll loop.
        if let Ok((listener, mut rx)) = DiscoveryListener::start("255.255.255.255").await {
            *self.listener.lock().unwrap() = Some(listener.clone());
            let me = self.clone();
            tokio::spawn(async move {
                while let Some((ip, msg)) = rx.recv().await {
                    me.on_disc_message(ip, msg);
                }
            });
            let me = self.clone();
            tokio::spawn(async move { me.poll_loop().await });
        }

        // Peer endpoint (request use).
        if let Ok((pe, mut prx)) = PeerEndpoint::start().await {
            *self.peer.lock().unwrap() = Some(pe as Arc<dyn PeerSender>);
            let me = self.clone();
            tokio::spawn(async move {
                while let Some((addr, data)) = prx.recv().await {
                    me.on_peer_message(addr, data);
                }
            });
        }

        // Connection watchdog (auto-reconnect).
        let me = self.clone();
        tokio::spawn(async move { me.reconnect_loop().await });

        server.serve().await
    }

    pub async fn shutdown(&self) {
        self.shutting_down.store(true, Ordering::SeqCst);
        let _guard = self.connect_lock.lock().await;
        if let Err(error) = self.cleanup_failed_attach().await {
            tracing::error!(%error, "USB/IP failed-attach cleanup during shutdown failed");
        }
        let connections: Vec<_> = self.connections.lock().unwrap().drain().map(|(_, c)| c).collect();
        for conn in connections {
            if let Some(port) = conn.port {
                let usbip = self.usbip.clone();
                match tokio::task::spawn_blocking(move || usbip.detach(port)).await {
                    Ok(Ok(())) => {},
                    result => tracing::error!(port, ?result, "USB/IP detach during shutdown failed"),
                }
            }
        }
        self.clear_export().await;
    }

    async fn clear_export(&self) {
        let server = self.usbip_server.lock().unwrap().take();
        if let Some(server) = server { server.shutdown().await; }
        let task = self.export_task.lock().unwrap().take();
        if let Some(task) = task {
            task.abort();
            let _ = task.await;
        }
        self.busids.lock().unwrap().clear();
        self.export_identities.lock().unwrap().clear();
        *self.export_mac.lock().unwrap() = None;
    }

    async fn clear_idle_export(&self) {
        if self.connections.lock().unwrap().is_empty() && self.reconnecting.lock().unwrap().is_empty() {
            self.clear_export().await;
        }
    }

    // -- broadcast helpers ------------------------------------------------- //
    fn broadcast(&self, event: &str, data: Value) {
        if let Some(s) = self.ipc.get() {
            s.broadcast(event, data, true);
        }
    }

    fn broadcast_servers(self: &Arc<Self>) {
        let snap = self.servers_snapshot();
        self.broadcast("servers", snap);
    }

    fn notice(&self, mac: &str, dev_id: &str, description: &str, state: &str, kind: &str) {
        self.broadcast(
            "notice",
            json!({"mac": mac, "dev_id": dev_id, "description": description, "kind": kind, "state": state}),
        );
    }

    fn broadcast_req_status(&self, mac: &str, dev_id: &str, state: &str, extra: Value) {
        let mut obj = json!({"mac": mac, "dev_id": dev_id, "state": state});
        if let (Value::Object(o), Value::Object(e)) = (&mut obj, extra) {
            for (k, v) in e {
                o.insert(k, v);
            }
        }
        self.broadcast("access_request_status", obj);
    }

    // -- discovery / state ------------------------------------------------- //
    fn poll_targets(&self) -> Vec<(String, u16)> {
        let s = self.store.get();
        let mut targets = Vec::new();
        if s.search_enabled {
            for a in &s.search_addresses {
                if !a.address.is_empty() {
                    targets.push((a.address.clone(), a.port.unwrap_or(ud::PORT)));
                }
            }
        }
        for ip in self.servers.lock().unwrap().keys() {
            targets.push((ip.clone(), ud::PORT));
        }
        targets
    }

    async fn poll_loop(self: Arc<Self>) {
        loop {
            let targets = self.poll_targets();
            let listener = self.listener.lock().unwrap().clone();
            if let Some(l) = listener {
                l.poll(&targets).await;
            }
            self.prune();
            let interval = self.store.get().refresh_interval.max(1) as u64;
            tokio::time::sleep(Duration::from_secs(interval)).await;
        }
    }

    async fn refresh_now(self: &Arc<Self>) {
        let targets = self.poll_targets();
        let listener = self.listener.lock().unwrap().clone();
        if let Some(l) = listener {
            l.poll(&targets).await;
        }
    }

    fn on_disc_message(self: &Arc<Self>, ip: String, msg: ud::Message) {
        // `has_device_list` distinguishes a real device list (even when empty -> it
        // replaces) from a beacon without a list (never replace). This lets us detect the
        // *last* device being unplugged (empty list).
        let (srv, has_device_list) = match msg {
            ud::Message::Announcement(a) => {
                let mut s = DeviceServer::new(&ip);
                s.update_from_announcement(&a);
                (Some(s), false)
            }
            ud::Message::DeviceList(l) => {
                let mut s = DeviceServer::new(&ip);
                s.devices = l.devices;
                (Some(s), true)
            }
            _ => (None, false),
        };
        if let Some(s) = srv {
            if self.apply_server(s, has_device_list) {
                self.broadcast_servers();
                self.maybe_autoconnect();
            }
        }
    }

    fn apply_server(&self, srv: DeviceServer, has_device_list: bool) -> bool {
        let ip = srv.ip.clone();
        self.last_seen.lock().unwrap().insert(ip.clone(), Instant::now());
        let mut map = self.servers.lock().unwrap();
        match map.get_mut(&ip) {
            None => {
                map.insert(ip, srv);
                true
            }
            Some(existing) => {
                let before = existing.clone();
                if !srv.mac.is_empty() {
                    existing.mac = srv.mac;
                    existing.model = srv.model;
                    existing.version = srv.version;
                    existing.hostname = srv.hostname;
                    existing.attached_device = srv.attached_device;
                }
                // A real (possibly empty) device list replaces; a beacon never does.
                let update_devices = if has_device_list { true } else { !srv.devices.is_empty() };
                if update_devices {
                    existing.devices = srv.devices;
                }
                *existing != before
            }
        }
    }

    fn prune(self: &Arc<Self>) {
        let ttl = Duration::from_secs((self.store.get().refresh_interval * 3).max(30) as u64);
        let now = Instant::now();
        let mut changed = false;
        {
            let mut servers = self.servers.lock().unwrap();
            let mut last = self.last_seen.lock().unwrap();
            let stale: Vec<String> = servers
                .keys()
                .filter(|ip| now.duration_since(*last.get(*ip).unwrap_or(&now)) > ttl)
                .cloned()
                .collect();
            for ip in stale {
                let mac = servers.get(&ip).map(DeviceServer::mac_str).unwrap_or_default();
                if self.connections.lock().unwrap().values().any(|c| c.server_mac == mac)
                    || self.reconnecting.lock().unwrap().values().any(|c| c.server_mac == mac) {
                    changed = true;
                    continue;
                }
                servers.remove(&ip);
                last.remove(&ip);
                changed = true;
            }
        }
        if changed {
            self.broadcast_servers();
        }
    }

    fn find_server_by_mac(&self, mac: &str) -> Option<DeviceServer> {
        self.servers.lock().unwrap().values().find(|s| s.mac_str() == mac).cloned()
    }

    fn entry_for(&self, mac: &str, dev_id: &str) -> Option<ud::UsbDeviceEntry> {
        self.find_server_by_mac(mac)?.devices.into_iter().find(|d| d.dev_id == dev_id)
    }

    // -- snapshots --------------------------------------------------------- //
    fn our_ip_for(&self, server_ip: &str) -> String {
        let mut c = self.our_ip.lock().unwrap();
        if let Some(v) = c.get(server_ip) {
            return v.clone();
        }
        let v = local_ip_towards(server_ip).unwrap_or_default();
        c.insert(server_ip.to_string(), v.clone());
        v
    }

    fn device_status(&self, srv: &DeviceServer, dev_id: &str, owner_ip: &str) -> &'static str {
        let mac = if srv.mac_str().is_empty() { srv.ip.clone() } else { srv.mac_str() };
        let key = (mac, dev_id.to_string());
        if self.connections.lock().unwrap().contains_key(&key) {
            return STATUS_CONNECTED;
        }
        if !owner_ip.is_empty() {
            if owner_ip == self.our_ip_for(&srv.ip) {
                if let Some(ts) = self.recently_released.lock().unwrap().get(&key) {
                    if ts.elapsed() < RELEASE_OPTIMISTIC_WINDOW {
                        return STATUS_AVAILABLE;
                    }
                }
                return STATUS_CONNECTED;
            }
            return STATUS_IN_USE;
        }
        STATUS_AVAILABLE
    }

    fn owner_host(self: &Arc<Self>, ip: &str) -> String {
        if ip.is_empty() {
            return String::new();
        }
        {
            let mut cache = self.host_cache.lock().unwrap();
            if let Some(name) = cache.get(ip) {
                return name.clone();
            }
            cache.insert(ip.to_string(), String::new()); // mark in progress
        }
        let me = self.clone();
        let ip_owned = ip.to_string();
        tokio::spawn(async move {
            let ip2 = ip_owned.clone();
            let name = tokio::task::spawn_blocking(move || {
                ip2.parse::<std::net::IpAddr>().ok().and_then(|a| dns_lookup::lookup_addr(&a).ok())
            })
            .await
            .ok()
            .flatten();
            if let Some(full) = name {
                let short = full.split('.').next().unwrap_or(&full).to_string();
                if !short.is_empty() {
                    me.host_cache.lock().unwrap().insert(ip_owned, short);
                    me.broadcast_servers();
                }
            }
        });
        String::new()
    }

    fn snapshot_device(self: &Arc<Self>, srv: &DeviceServer, d: &ud::LogicalDevice) -> Value {
        let key = device_key(d.primary());
        let settings = self.store.get();
        let rule = settings.rule_for(&srv.mac_str(), &key);
        let owner_ip = d.owner_ip();
        let recovering = self.reconnecting.lock().unwrap().values().any(|c|
            c.server_mac == srv.mac_str() && device_identity(&c.entry) == device_identity(d.primary()));
        let active = self.connections.lock().unwrap().contains_key(&(srv.mac_str(), d.dev_id().to_string()));
        json!({
            "dev_id": d.dev_id(),
            "description": d.description().trim(),
            "alias": rule.map(|r| r.alias.clone()).unwrap_or_default(),
            "launch_app": rule.map(|r| r.launch_app.clone()).unwrap_or_default(),
            "disconnect_on_app_close": rule.map(|r| r.disconnect_on_app_close).unwrap_or(false),
            "id_vendor": d.id_vendor(),
            "id_product": d.id_product(),
            "tag": d.tag(),
            "speed": d.speed(),
            "type": d.device_type(),
            "usb_class": d.representative_class(),
            "composite": d.is_composite(),
            "password_required": d.primary().password_required(),
            "usb_port": srv.usb_port(d.primary()),
            "password_configured": self.device_password(srv, d.primary()).is_some(),
            "auto_connect": settings.should_auto_connect(&srv.mac_str(), &key),
            "reconnect_on_error": rule.map(|r| r.reconnect_on_error).unwrap_or(true),
            "key": key,
            "status": if recovering && !active { STATUS_RECONNECTING } else { self.device_status(srv, d.dev_id(), &owner_ip) },
            "owner_ip": owner_ip,
            "owner_host": self.owner_host(&owner_ip),
        })
    }

    fn snapshot_server(self: &Arc<Self>, srv: &DeviceServer) -> Value {
        let mut srv = srv.clone();
        let pending: Vec<_> = self.reconnecting.lock().unwrap().values()
            .filter(|c| c.server_mac == srv.mac_str()).cloned().collect();
        for connection in pending {
            if !srv.devices.iter().any(|d| device_identity(d) == device_identity(&connection.entry)) {
                srv.devices.push(connection.entry);
            }
        }
        let srv = &srv;
        let ipv6 = if srv.mac.len() == 6 { ud::mac_to_link_local(&srv.mac) } else { String::new() };
        // Composite devices (multiple interfaces) are collapsed into ONE row like SX
        // Virtual Link; the representative class drives type + icon.
        let devices: Vec<Value> =
            ud::logical_devices(&srv.devices).iter().map(|d| self.snapshot_device(srv, d)).collect();
        json!({
            "ip": srv.ip,
            "mac": srv.mac_str(),
            "model": srv.model,
            "version": srv.version,
            "hostname": srv.hostname,
            "bind_all": self.store.get().bind_all_servers.contains(&srv.mac_str()),
            "usb_port_passwords": self.credentials.status(&srv.mac_str()),
            "ipv6": ipv6,
            "devices": devices,
        })
    }

    fn servers_snapshot(self: &Arc<Self>) -> Value {
        let servers: Vec<DeviceServer> = self.servers.lock().unwrap().values().cloned().collect();
        let list: Vec<Value> = servers.iter().map(|s| self.snapshot_server(s)).collect();
        json!({ "servers": list })
    }

    // -- IPC dispatch ------------------------------------------------------ //
    async fn enable_system_service() -> Result<()> {
        if std::env::var_os("SILEX_SYSTEM_SERVICE").is_some() {
            tokio::task::spawn_blocking(|| -> Result<()> {
                let output = CommandRunner::default().run(std::process::Command::new("/usr/bin/systemctl")
                    .args(["enable", "opensilexlink.service"]), COMMAND_TIMEOUT)?;
                if !output.status.success() {
                    bail!("Cannot enable connection before login: {}", String::from_utf8_lossy(&output.stderr).trim());
                }
                Ok(())
            }).await??;
        }
        Ok(())
    }

    async fn set_bind_all(self: &Arc<Self>, mac: &str, enabled: bool) -> Result<Value> {
        if enabled {
            self.find_server_by_mac(mac).ok_or_else(|| anyhow::anyhow!("device server {mac} not found"))?;
            Self::enable_system_service().await?;
        }
        let was_enabled = self.store.get().bind_all_servers.iter().any(|m| m == mac);
        let settings = self.store.set_bind_all(mac, enabled)?;
        if enabled && !was_enabled {
            // Explicitly enabling the server-wide policy is a new Connect action.
            self.manual_disconnects.lock().unwrap().retain(|(m, _)| m != mac);
            self.startup_done.lock().unwrap().retain(|(m, _)| m != mac);
        }
        self.broadcast("settings", settings.to_value());
        self.broadcast_servers();
        self.maybe_autoconnect();
        Ok(json!({"bind_all": enabled}))
    }

    async fn handle(self: Arc<Self>, method: String, params: Value, peer: Arc<Peer>) -> Result<Value> {
        let sp = |k: &str| -> Result<String> {
            params.get(k).and_then(Value::as_str).map(str::to_string).ok_or_else(|| anyhow::anyhow!("missing parameter {k:?}"))
        };
        match method.as_str() {
            "hello" => Ok(json!({"name": "silex-daemon", "version": 1, "app_version": env!("CARGO_PKG_VERSION"),
                "system_service": std::env::var_os("SILEX_SYSTEM_SERVICE").is_some()})),
            "prepare_shutdown" => {
                let _guard = self.connect_lock.lock().await;
                let connections: Vec<_> = self.connections.lock().unwrap().values().cloned().collect();
                for conn in connections {
                    if let Some(port) = conn.port {
                        let usbip = self.usbip.clone();
                        tokio::task::spawn_blocking(move || usbip.ensure_detachable(port)).await??;
                    }
                }
                Ok(json!({"ready": true}))
            }
            "list_servers" => Ok(self.servers_snapshot()),
            "subscribe" => {
                peer.subscribe();
                peer.send_event("servers", self.servers_snapshot());
                Ok(json!({"subscribed": true}))
            }
            "refresh" => {
                self.refresh_now().await;
                Ok(self.servers_snapshot())
            }
            "get_settings" => Ok(self.store.get().to_value()),
            "set_port_passwords" => {
                let passwords = params.get("passwords").and_then(Value::as_object)
                    .ok_or_else(|| anyhow::anyhow!("missing password updates"))?;
                self.set_port_passwords(&sp("mac")?, passwords).await
            }
            "set_settings" => {
                let s = self.store.update(params.clone())?;
                self.broadcast("settings", s.to_value());
                self.broadcast_servers();
                self.maybe_autoconnect();
                Ok(s.to_value())
            }
            "set_bind_all" => {
                let enabled = params.get("enabled").and_then(Value::as_bool)
                    .ok_or_else(|| anyhow::anyhow!("missing boolean parameter enabled"))?;
                self.set_bind_all(&sp("mac")?, enabled).await
            }
            "set_device_rule" => {
                let mac = sp("mac")?;
                let key = sp("key")?;
                let mut updates = Map::new();
                if let Value::Object(o) = &params {
                    for (k, v) in o {
                        if k != "mac" && k != "key" {
                            updates.insert(k.clone(), v.clone());
                        }
                    }
                }
                if updates.get("auto_connect").and_then(Value::as_bool) == Some(true) {
                    Self::enable_system_service().await?;
                }
                let was_auto_connect = self.store.get().should_auto_connect(&mac, &key);
                let s = self.store.set_device_rule(&mac, &key, &updates)?;
                if updates.get("reconnect_on_error").and_then(Value::as_bool) == Some(false) {
                    self.reconnecting.lock().unwrap().retain(|_, c|
                        c.server_mac != mac || device_key(&c.entry) != key);
                }
                if updates.get("auto_connect").and_then(Value::as_bool) == Some(true) && !was_auto_connect {
                    if let Some(server) = self.find_server_by_mac(&mac) {
                        for device in &server.devices {
                            if device_key(device) == key {
                                self.manual_disconnects.lock().unwrap().remove(&(mac.clone(), device.dev_id.clone()));
                                if !self.connections.lock().unwrap().contains_key(&(mac.clone(), device.dev_id.clone())) {
                                    self.startup_done.lock().unwrap().remove(&(mac.clone(), device_identity(device)));
                                }
                            }
                        }
                    }
                }
                self.broadcast("settings", s.to_value());
                self.broadcast_servers();
                self.maybe_autoconnect();
                Ok(s.to_value())
            }
            "connect" => self.connect(&sp("mac")?, &sp("dev_id")?).await,
            "disconnect" => self.disconnect(&sp("mac")?, &sp("dev_id")?).await,
            "request_access" => self.request_access(&sp("mac")?, &sp("dev_id")?).await,
            "respond_access_request" => {
                let accept = params.get("accept").and_then(Value::as_bool).unwrap_or(false);
                self.respond_access_request(&sp("mac")?, &sp("dev_id")?, accept).await
            }
            other => bail!("unknown method: {other:?}"),
        }
    }

    // -- auto-connect ------------------------------------------------------ //
    fn maybe_autoconnect(self: &Arc<Self>) {
        let settings = self.store.get();
        let servers: Vec<DeviceServer> = self.servers.lock().unwrap().values().cloned().collect();
        for srv in servers {
            let mac = srv.mac_str();
            if mac.is_empty() {
                continue;
            }
            for d in &srv.devices {
                // Connect a composite device only ONCE (via the leading interface M0).
                if !matches!(d.interface_index(), None | Some(0)) {
                    continue;
                }
                if !d.owner_ip.is_empty() || (d.password_required() && self.device_password(&srv, d).is_none()) {
                    continue;
                }
                let key = (mac.clone(), d.dev_id.clone());
                if self.manual_disconnects.lock().unwrap().contains(&key)
                    || self.startup_done.lock().unwrap().contains(&(mac.clone(), device_identity(d)))
                    || self.reconnecting.lock().unwrap().values().any(|c| c.server_mac == mac && device_identity(&c.entry) == device_identity(d)) {
                    continue;
                }
                if self.connections.lock().unwrap().contains_key(&key) {
                    continue;
                }
                {
                    let mut pending = self.autoconnect_pending.lock().unwrap();
                    if pending.contains(&key) {
                        continue;
                    }
                    if !settings.should_auto_connect(&mac, &device_key(d)) {
                        continue;
                    }
                    pending.insert(key.clone());
                }
                let me = self.clone();
                let (m, dev) = key.clone();
                tokio::spawn(async move {
                    let _ = me.connect_inner(&m, &dev, true).await;
                    me.autoconnect_pending.lock().unwrap().remove(&(m, dev));
                });
            }
        }
    }

    // -- connect / disconnect ---------------------------------------------- //
    fn device_password(&self, srv: &DeviceServer, device: &ud::UsbDeviceEntry) -> Option<Vec<u8>> {
        self.credentials.password(&srv.mac_str(), srv.usb_port(device)?)
    }

    fn export_passwords(&self, srv: &DeviceServer) -> HashMap<String, Vec<u8>> {
        srv.devices.iter().filter(|d| d.password_required()).filter_map(|d|
            self.device_password(srv, d).map(|password| (d.dev_id.clone(), password))).collect()
    }

    async fn set_port_passwords(self: &Arc<Self>, mac: &str, updates: &Map<String, Value>) -> Result<Value> {
        let _guard = self.connect_lock.lock().await;
        let srv = self.find_server_by_mac(mac).ok_or_else(|| anyhow::anyhow!("device server not found"))?;
        if !srv.model.eq_ignore_ascii_case("DS-600") { bail!("USB port passwords currently require a DS-600"); }
        self.credentials.update(&srv.mac_str(), updates)?;
        if self.export_mac.lock().unwrap().as_deref() == Some(mac) {
            if let Some(server) = self.usbip_server.lock().unwrap().clone() {
                server.update_passwords(self.export_passwords(&srv));
            }
        }
        self.broadcast_servers();
        self.maybe_autoconnect();
        Ok(self.credentials.status(&srv.mac_str()))
    }

    async fn ensure_export(self: &Arc<Self>, srv: &DeviceServer) -> Result<()> {
        self.clear_idle_export().await;
        if let Some(mac) = self.export_mac.lock().unwrap().as_ref() {
            if mac != &srv.mac_str() {
                bail!("Only one device server can be connected at a time. Disconnect its devices first.");
            }
        }
        let devices = {
            let mut identities = self.export_identities.lock().unwrap();
            let mut busids = self.busids.lock().unwrap();
            busids.clear();
            let mut devices = Vec::new();
            for device in ud::logical_devices(&srv.devices) {
                let identity = device_identity(device.primary());
                let next = format!("1-{}", identities.len() + 1);
                let busid = identities.entry(identity).or_insert(next).clone();
                let dev_id = device.dev_id().to_string();
                busids.insert(dev_id.clone(), busid.clone());
                devices.push((busid, dev_id));
            }
            devices
        };
        let existing = self.usbip_server.lock().unwrap().clone();
        if let Some(server) = existing {
            server.update_devices(devices.clone(), &srv.devices);
            server.update_passwords(self.export_passwords(srv));
        }
        if self.start_usbip_server && self.usbip_server.lock().unwrap().is_none() {
            let server = Arc::new(UsbipServer::new(
                &srv.ip,
                mac_array(&srv.mac)?,
                None,
                ("127.0.0.1".to_string(), TCP_PORT),
                ud::PORT,
                devices,
            ).with_device_speeds(&srv.devices));
            server.update_passwords(self.export_passwords(srv));
            server.bind().await?;
            let s = server.clone();
            let task = tokio::spawn(async move {
                let _ = s.serve().await;
            });
            *self.export_task.lock().unwrap() = Some(task);
            *self.usbip_server.lock().unwrap() = Some(server);
        }
        *self.export_mac.lock().unwrap() = Some(srv.mac_str());
        Ok(())
    }

    async fn connect(self: &Arc<Self>, mac: &str, dev_id: &str) -> Result<Value> {
        self.connect_inner(mac, dev_id, false).await
    }

    /// Caller holds connect_lock. A failed attach can already have allocated a
    /// port. Wait for its command to exit, then detach additions before retrying.
    async fn cleanup_failed_attach(&self) -> Result<()> {
        let before = self.failed_attach_ports.lock().unwrap().clone();
        if let Some(before) = before {
            let usbip = self.usbip.clone();
            tokio::task::spawn_blocking(move || -> Result<()> {
                usbip.ensure_idle()?;
                for port in usbip.used_ports().difference(&before).copied() {
                    usbip.ensure_detachable(port)?;
                    usbip.detach(port)?;
                }
                Ok(())
            }).await??;
            *self.failed_attach_ports.lock().unwrap() = None;
        }
        Ok(())
    }

    async fn connect_inner(self: &Arc<Self>, mac: &str, dev_id: &str, automatic: bool) -> Result<Value> {
        self.cancel_grace_timer(mac, dev_id);
        let _guard = self.connect_lock.lock().await;
        if self.shutting_down.load(Ordering::SeqCst) { bail!("daemon is shutting down"); }
        self.cleanup_failed_attach().await?;
        if automatic && self.manual_disconnects.lock().unwrap().contains(&(mac.to_string(), dev_id.to_string())) {
            bail!("automatic connection suppressed after manual disconnect");
        }
        let srv = self.find_server_by_mac(mac).ok_or_else(|| anyhow::anyhow!("device server {mac} not found"))?;
        let logical = ud::logical_devices(&srv.devices);
        let device = logical.iter().find(|d| d.dev_id() == dev_id)
            .ok_or_else(|| anyhow::anyhow!("{dev_id} is not a primary USB device at {mac}"))?;
        let key = (mac.to_string(), dev_id.to_string());
        if let Some(c) = self.connections.lock().unwrap().get(&key) {
            return Ok(json!({"connected": true, "busid": c.busid, "port": c.port}));
        }
        if device.primary().password_required() && self.device_password(&srv, device.primary()).is_none() {
            bail!("USB port password required. Select this DS-600 and open USB port passwords to save the password for its physical port.");
        }
        self.ensure_export(&srv).await?;
        let busid = self
            .busids
            .lock()
            .unwrap()
            .get(dev_id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no busid for {dev_id}"))?;
        // usbip attach waits for this same runtime's exporter to answer. Running
        // the command on an async worker can strand that freshly spawned task
        // in its local queue and deadlock the entire attach.
        let usbip = self.usbip.clone();
        let attach_busid = busid.clone();
        let (before, attached) = tokio::task::spawn_blocking(move || -> Result<_> {
            usbip.ensure_idle()?;
            usbip.ensure_module()?;
            let before = usbip.used_ports();
            let attached = usbip.attach(&attach_busid);
            Ok((before, attached))
        }).await??;
        if attached.is_err() {
            *self.failed_attach_ports.lock().unwrap() = Some(before.clone());
        }
        attached.map_err(|error| {
            if device.primary().password_required() {
                anyhow::anyhow!("Connection failed. Check the saved password for the {} physical port in USB port passwords. {error}",
                    if srv.usb_port(device.primary()) == Some("usb3") { "USB 3.0" } else { "USB 2.0" })
            } else { error }
        })?;
        // The new vhci port may appear in `usbip port` shortly after attach.
        let mut port = None;
        for _ in 0..20 {
            let usbip = self.usbip.clone();
            let used = tokio::task::spawn_blocking(move || usbip.used_ports()).await?;
            let new: Vec<i32> = used.difference(&before).copied().collect();
            if let Some(&p) = new.iter().min() {
                port = Some(p);
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        if port.is_none() {
            *self.failed_attach_ports.lock().unwrap() = Some(before);
            bail!("USB/IP attach succeeded but its VHCI port could not be identified; cleanup required before retry");
        }
        let desc = srv
            .devices
            .iter()
            .find(|d| d.dev_id == dev_id)
            .map(|d| d.description.trim().to_string())
            .unwrap_or_else(|| dev_id.to_string());
        self.connections.lock().unwrap().insert(
            key.clone(),
            Connection { server_mac: mac.to_string(), dev_id: dev_id.to_string(), busid: busid.clone(), port, description: desc, entry: device.primary().clone() },
        );
        self.startup_done.lock().unwrap().insert((mac.to_string(), device_identity(device.primary())));
        self.recently_released.lock().unwrap().remove(&key);
        self.manual_disconnects.lock().unwrap().remove(&key);
        drop(_guard);
        self.broadcast_servers();
        Ok(json!({"connected": true, "busid": busid, "port": port}))
    }

    async fn disconnect(self: &Arc<Self>, mac: &str, dev_id: &str) -> Result<Value> {
        let _guard = self.connect_lock.lock().await;
        let key = (mac.to_string(), dev_id.to_string());
        let conn = self.connections.lock().unwrap().get(&key).cloned();
        let conn = match conn {
            Some(c) => c,
            None => {
                self.cancel_reconnect(mac, dev_id);
                self.clear_idle_export().await;
                self.broadcast_servers();
                return Ok(json!({"disconnected": true, "note": "reconnection cancelled"}));
            },
        };
        if let Some(port) = conn.port {
            let usbip = self.usbip.clone();
            tokio::task::spawn_blocking(move || -> Result<()> {
                usbip.ensure_detachable(port)?;
                usbip.detach(port)
            }).await??;
        }
        self.connections.lock().unwrap().remove(&key);
        self.cancel_reconnect(mac, dev_id);
        self.clear_idle_export().await;
        self.recently_released.lock().unwrap().insert(key.clone(), Instant::now());
        self.broadcast_servers();
        let me = self.clone();
        let (m, d) = key;
        tokio::spawn(async move { me.refresh_burst(&m, &d).await });
        Ok(json!({"disconnected": true}))
    }

    async fn refresh_burst(self: &Arc<Self>, mac: &str, dev_id: &str) {
        let deadline = Instant::now() + DISCONNECT_REFRESH_TIMEOUT;
        let mut delay = Duration::from_millis(400);
        while Instant::now() < deadline {
            tokio::time::sleep(delay).await;
            self.refresh_now().await;
            tokio::time::sleep(Duration::from_millis(250)).await;
            if self.connections.lock().unwrap().contains_key(&(mac.to_string(), dev_id.to_string())) {
                return;
            }
            match self.entry_for(mac, dev_id) {
                Some(e) if e.owner_ip.is_empty() => return,
                None => return,
                _ => {}
            }
            delay = (delay.mul_f32(1.4)).min(Duration::from_secs(2));
        }
    }

    // -- auto-reconnect ---------------------------------------------------- //
    async fn reconnect_loop(self: Arc<Self>) {
        loop {
            tokio::time::sleep(RECONNECT_CHECK_INTERVAL).await;
            // First prune unplugged devices (no reconnect for what is gone), then handle
            // lost tunnels.
            self.prune_phantoms().await;
            self.check_connections().await;
        }
    }

    // -- phantom cleanup (device disappeared at the server) ---------------- //
    /// True while (mac, dev_id) is still listed in discovery. False once the device is gone
    /// from the device list (unplugged/ejected) OR the whole server has disappeared.
    fn device_present(&self, mac: &str, dev_id: &str) -> bool {
        match self.find_server_by_mac(mac) {
            Some(srv) => srv.devices.iter().any(|d| d.dev_id == dev_id),
            None => false,
        }
    }

    /// Detach vhci devices whose USB device disappeared at the server. Counterpart to
    /// `check_connections`: there the vhci *port* vanishes (tunnel dead -> reconnect); here
    /// the device is absent from discovery. Detach the phantom port and queue
    /// recovery when enabled, so a physical replug is also recovered.
    async fn prune_phantoms(self: &Arc<Self>) {
        let conns: Vec<Connection> = self.connections.lock().unwrap().values().cloned().collect();
        for c in conns {
            if self.device_present(&c.server_mac, &c.dev_id) {
                continue;
            }
            let _guard = self.connect_lock.lock().await;
            if self.shutting_down.load(Ordering::SeqCst) { return; }
            if self.device_present(&c.server_mac, &c.dev_id) { continue; }
            self.handle_device_gone(c).await;
        }
    }

    /// A connected device vanished from discovery (unplugged): detach the phantom vhci
    /// port, drop the connection, tell the GUI and queue recovery. Caller holds
    /// connect_lock so detach cannot race port attribution or a replacement.
    async fn handle_device_gone(self: &Arc<Self>, conn: Connection) {
        let key = (conn.server_mac.clone(), conn.dev_id.clone());
        {
            let conns = self.connections.lock().unwrap();
            match conns.get(&key) {
                Some(c) if c.port == conn.port => {}
                _ => return, // disconnected/replaced meanwhile
            }
        }
        if let Some(port) = conn.port {
            let usbip = self.usbip.clone();
            let result = tokio::task::spawn_blocking(move || -> Result<()> {
                usbip.ensure_detachable(port)?;
                usbip.detach(port)
            }).await;
            if !matches!(result, Ok(Ok(()))) {
                // Keep ownership and retry next pass; never reattach over a port
                // whose detach failed or timed out.
                tracing::warn!(port, busid = %conn.busid, ?result, "stale USB/IP detach failed; retaining attachment for retry");
                return;
            }
        }
        self.connections.lock().unwrap().remove(&key);
        let desc = if conn.description.is_empty() { conn.dev_id.clone() } else { conn.description.clone() };
        self.broadcast_servers();
        self.notice(&conn.server_mac, &conn.dev_id, &desc, "removed", "device");
        self.queue_reconnect(conn);
    }

    async fn check_connections(self: &Arc<Self>) {
        let _guard = self.connect_lock.lock().await;
        if self.shutting_down.load(Ordering::SeqCst) { return; }
        if let Err(error) = self.cleanup_failed_attach().await {
            tracing::warn!(%error, "cannot clean up failed USB/IP attach yet");
            return;
        }
        let conns: Vec<Connection> = self
            .connections
            .lock()
            .unwrap()
            .values()
            .filter(|c| c.port.is_some())
            .cloned()
            .collect();
        if conns.is_empty() {
            return;
        }
        let usbip = self.usbip.clone();
        let ports = match tokio::task::spawn_blocking(move || usbip.used_ports()).await {
            Ok(ports) => ports,
            Err(error) => {
                tracing::warn!(%error, "cannot inspect VHCI ports");
                return;
            }
        };
        let failed = self.usbip_server.lock().unwrap().as_ref()
            .map(|s| s.failed_busids()).unwrap_or_default();
        for c in conns {
            if let Some(p) = c.port {
                if !ports.contains(&p) {
                    self.handle_tunnel_lost(c);
                } else if failed.contains(&c.busid) {
                    tracing::warn!(port = p, busid = %c.busid, "tunnel ended but VHCI port remains occupied");
                    self.handle_device_gone(c).await;
                }
            }
        }
    }

    fn handle_tunnel_lost(self: &Arc<Self>, conn: Connection) {
        let key = (conn.server_mac.clone(), conn.dev_id.clone());
        {
            let mut connections = self.connections.lock().unwrap();
            if connections.get(&key).map(|c| c.port) != Some(conn.port) { return; }
            connections.remove(&key);
        }
        self.queue_reconnect(conn);
    }

    fn reconnect_enabled(&self, conn: &Connection) -> bool {
        self.store.get().rule_for(&conn.server_mac, &device_key(&conn.entry))
            .map(|r| r.reconnect_on_error).unwrap_or(true)
    }

    fn queue_reconnect(self: &Arc<Self>, conn: Connection) {
        let key = (conn.server_mac.clone(), conn.dev_id.clone());
        if self.shutting_down.load(Ordering::SeqCst) || !self.reconnect_enabled(&conn)
            || self.manual_disconnects.lock().unwrap().contains(&key) {
            self.notice(&conn.server_mac, &conn.dev_id, &conn.description, "disconnected", "reconnect");
            self.broadcast_servers();
            return;
        }
        {
            let mut pending = self.reconnecting.lock().unwrap();
            if pending.contains_key(&key) { return; }
            pending.insert(key, conn.clone());
        }
        self.notice(&conn.server_mac, &conn.dev_id, &conn.description, "lost", "reconnect");
        self.broadcast_servers();
        let me = self.clone();
        tokio::spawn(async move { me.reconnect_worker(conn).await; });
    }

    async fn reconnect_worker(self: Arc<Self>, conn: Connection) {
        let key = (conn.server_mac.clone(), conn.dev_id.clone());
        let identity = device_identity(&conn.entry);
        let mut attempt = 0usize;
        loop {
            if self.shutting_down.load(Ordering::SeqCst)
                || !self.reconnect_enabled(&conn)
                || self.manual_disconnects.lock().unwrap().contains(&key)
                || !self.reconnecting.lock().unwrap().contains_key(&key) { break; }
            let delay = RECONNECT_BACKOFF_MS[attempt.min(RECONNECT_BACKOFF_MS.len() - 1)];
            attempt = attempt.saturating_add(1);
            tokio::time::sleep(Duration::from_millis(delay)).await;
            // Recheck cancellation after the wait and before reserving a device.
            if self.shutting_down.load(Ordering::SeqCst) || !self.reconnect_enabled(&conn)
                || self.manual_disconnects.lock().unwrap().contains(&key)
                || !self.reconnecting.lock().unwrap().contains_key(&key) { break; }
            self.refresh_now().await;
            let server = match self.find_server_by_mac(&conn.server_mac) {
                Some(server) => server,
                None => continue,
            };
            let device = match ud::logical_devices(&server.devices).into_iter()
                .find(|d| device_identity(d.primary()) == identity) {
                Some(device) => device.primary().clone(),
                None => continue,
            };
            if !device.owner_ip.is_empty() && device.owner_ip != self.our_ip_for(&server.ip) {
                continue; // another host owns it; wait without disturbing that host
            }
            if self.connect_inner(&conn.server_mac, &device.dev_id, true).await.is_ok() {
                self.reconnecting.lock().unwrap().remove(&key);
                self.notice(&conn.server_mac, &device.dev_id, &conn.description, "reconnected", "reconnect");
                self.broadcast_servers();
                return;
            }
        }
        self.reconnecting.lock().unwrap().remove(&key);
        self.broadcast_servers();
    }

    fn cancel_reconnect(&self, mac: &str, dev_id: &str) {
        let identity = self.entry_for(mac, dev_id).map(|entry| device_identity(&entry));
        let mut pending = self.reconnecting.lock().unwrap();
        let keys: Vec<_> = pending.iter().filter(|(key, connection)| key.0 == mac &&
            (key.1 == dev_id || identity.as_ref() == Some(&device_identity(&connection.entry))))
            .map(|(key, _)| key.clone()).collect();
        for key in keys {
            pending.remove(&key);
            self.manual_disconnects.lock().unwrap().insert(key);
        }
        self.manual_disconnects.lock().unwrap().insert((mac.to_string(), dev_id.to_string()));
    }

    // -- peer: receive side ------------------------------------------------ //
    fn on_peer_message(self: &Arc<Self>, addr: SocketAddr, data: Vec<u8>) {
        let msg = match peerproto::parse(&data) {
            Ok(m) => m,
            Err(_) => return,
        };
        if msg.msg_type == peerproto::TYPE_REQUEST {
            self.handle_peer_request(addr, msg);
        } else {
            self.handle_peer_response(msg);
        }
    }

    fn peer_send(&self, addr: SocketAddr, msg_type: u16, msg: &PeerMessage) {
        if let Some(p) = self.peer.lock().unwrap().clone() {
            p.send(addr, peerproto::build_from(msg_type, msg, &self.peer_user));
        }
    }

    fn find_held_for(&self, msg: &PeerMessage) -> Option<(String, String, String)> {
        let mac = msg.mac_str();
        let srv = self.find_server_by_mac(&mac)?;
        for d in &srv.devices {
            if d.id_vendor == msg.id_vendor
                && d.id_product == msg.id_product
                && d.description.trim() == msg.description.trim()
                && self.connections.lock().unwrap().contains_key(&(mac.clone(), d.dev_id.clone()))
            {
                return Some((mac, d.dev_id.clone(), device_key(d)));
            }
        }
        None
    }

    fn find_device_by_fields(&self, msg: &PeerMessage) -> Option<(String, String)> {
        let mac = msg.mac_str();
        let srv = self.find_server_by_mac(&mac)?;
        for d in &srv.devices {
            if d.id_vendor == msg.id_vendor
                && d.id_product == msg.id_product
                && d.description.trim() == msg.description.trim()
            {
                return Some((mac, d.dev_id.clone()));
            }
        }
        None
    }

    fn handle_peer_request(self: &Arc<Self>, addr: SocketAddr, msg: PeerMessage) {
        self.peer_send(addr, peerproto::TYPE_ACK, &msg); // immediate ACK
        let (mac, dev_id, key) = match self.find_held_for(&msg) {
            Some(v) => v,
            None => return, // we do not hold it
        };
        let rule = self.store.get().rule_for(&mac, &key).cloned();
        if let Some(r) = rule {
            if r.auto_disconnect_on_request {
                let timeout = r.auto_disconnect_timeout.max(0) as u64;
                if timeout == 0 {
                    let me = self.clone();
                    let m = mac.clone();
                    let d = dev_id.clone();
                    tokio::spawn(async move { me.accept_peer_request(addr, msg, &m, &d).await });
                } else {
                    self.arm_grace_timer(&mac, &dev_id, addr, msg.clone(), Duration::from_secs(timeout));
                    self.notice(&mac, &dev_id, msg.description.trim(), "auto_release_pending", "peer");
                }
                return;
            }
        }
        // Otherwise ask the user via the GUI (prompt reacts over IPC).
        self.pending_requests.lock().unwrap().insert((mac.clone(), dev_id.clone()), (addr, msg.clone()));
        self.broadcast(
            "access_request",
            json!({
                "mac": mac, "dev_id": dev_id, "key": key,
                "description": msg.description.trim(),
                "requester": msg.user, "requester_ip": addr.ip().to_string(),
            }),
        );
    }

    async fn accept_peer_request(self: &Arc<Self>, addr: SocketAddr, msg: PeerMessage, mac: &str, dev_id: &str) {
        self.peer_send(addr, peerproto::TYPE_ACCEPTED, &msg);
        match self.disconnect(mac, dev_id).await {
            Ok(_) => self.peer_send(addr, peerproto::TYPE_RELEASED, &msg),
            Err(error) => {
                tracing::warn!(%error, "Cannot release USB device");
                self.peer_send(addr, peerproto::TYPE_DENIED, &msg);
            }
        }
    }

    fn arm_grace_timer(self: &Arc<Self>, mac: &str, dev_id: &str, addr: SocketAddr, msg: PeerMessage, timeout: Duration) {
        let key = (mac.to_string(), dev_id.to_string());
        let me = self.clone();
        let (m, d) = key.clone();
        let handle = tokio::spawn(async move {
            tokio::time::sleep(timeout).await;
            me.grace_fire(&m, &d).await;
        });
        let mut t = self.grace_timers.lock().unwrap();
        if let Some(old) = t.remove(&key) {
            old.handle.abort();
        }
        t.insert(key, GraceTimer { handle, addr, msg });
    }

    async fn grace_fire(self: &Arc<Self>, mac: &str, dev_id: &str) {
        let entry = self.grace_timers.lock().unwrap().remove(&(mac.to_string(), dev_id.to_string()));
        if let Some(g) = entry {
            self.accept_peer_request(g.addr, g.msg.clone(), mac, dev_id).await;
            self.notice(mac, dev_id, g.msg.description.trim(), "auto_released", "peer");
        }
    }

    fn cancel_grace_timer(&self, mac: &str, dev_id: &str) {
        if let Some(g) = self.grace_timers.lock().unwrap().remove(&(mac.to_string(), dev_id.to_string())) {
            g.handle.abort();
        }
    }

    async fn respond_access_request(self: &Arc<Self>, mac: &str, dev_id: &str, accept: bool) -> Result<Value> {
        let pending = self.pending_requests.lock().unwrap().remove(&(mac.to_string(), dev_id.to_string()));
        let (addr, msg) = match pending {
            Some(v) => v,
            None => return Ok(json!({"ok": false, "note": "no open request"})),
        };
        if accept {
            self.accept_peer_request(addr, msg, mac, dev_id).await;
        } else {
            self.peer_send(addr, peerproto::TYPE_DENIED, &msg);
        }
        Ok(json!({"ok": true, "accepted": accept}))
    }

    // -- peer: send side --------------------------------------------------- //
    fn arm_request_timer(self: &Arc<Self>, mac: &str, dev_id: &str, delay: Duration) {
        let key = (mac.to_string(), dev_id.to_string());
        let me = self.clone();
        let (m, d) = key.clone();
        let handle = tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            me.on_request_timeout(&m, &d);
        });
        let mut o = self.outgoing.lock().unwrap();
        if let Some(out) = o.get_mut(&key) {
            if let Some(t) = out.timer.take() {
                t.abort();
            }
            out.timer = Some(handle);
        } else {
            handle.abort();
        }
    }

    fn clear_outgoing(&self, mac: &str, dev_id: &str) {
        if let Some(out) = self.outgoing.lock().unwrap().remove(&(mac.to_string(), dev_id.to_string())) {
            if let Some(t) = out.timer {
                t.abort();
            }
        }
    }

    async fn request_access(self: &Arc<Self>, mac: &str, dev_id: &str) -> Result<Value> {
        let srv = self.find_server_by_mac(mac).ok_or_else(|| anyhow::anyhow!("device server {mac} not found"))?;
        let entry = srv
            .devices
            .iter()
            .find(|d| d.dev_id == dev_id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("{dev_id} not present at {mac}"))?;
        let owner = entry.owner_ip.clone();
        if owner.is_empty() {
            bail!("device is not held by another host");
        }
        if owner == self.our_ip_for(&srv.ip) {
            bail!("device is already held by us");
        }
        if self.peer.lock().unwrap().is_none() {
            bail!("peer endpoint not available");
        }
        let pkt = peerproto::build(
            peerproto::TYPE_REQUEST,
            mac_array(&srv.mac)?,
            entry.id_vendor,
            entry.id_product,
            &peerproto::request_description(&entry.description),
            &entry.tag,
            &self.peer_user,
        );
        self.outgoing.lock().unwrap().insert(
            (mac.to_string(), dev_id.to_string()),
            Outgoing { acked: false, timer: None, owner: owner.clone() },
        );
        self.arm_request_timer(mac, dev_id, REQUEST_ACK_TIMEOUT);
        let addr: SocketAddr = format!("{}:{}", owner, peerproto::PORT).parse()?;
        if let Some(p) = self.peer.lock().unwrap().clone() {
            p.send(addr, pkt);
        }
        self.broadcast_req_status(mac, dev_id, "sent", json!({"owner_ip": owner}));
        Ok(json!({"requested": true, "owner_ip": owner}))
    }

    fn handle_peer_response(self: &Arc<Self>, msg: PeerMessage) {
        let (mac, dev_id) = match self.find_device_by_fields(&msg) {
            Some(v) => v,
            None => return,
        };
        let key = (mac.clone(), dev_id.clone());
        let state;
        let mut do_rearm = false;
        let mut remove = false;
        let mut released = false;
        {
            let mut o = self.outgoing.lock().unwrap();
            let out = match o.get_mut(&key) {
                Some(x) => x,
                None => return,
            };
            match msg.msg_type {
                t if t == peerproto::TYPE_ACK => {
                    out.acked = true;
                    do_rearm = true;
                    state = "acked";
                }
                t if t == peerproto::TYPE_ACCEPTED => state = "accepted",
                // REJECTED: the holder found no matching device - same outcome as DENIED.
                t if t == peerproto::TYPE_DENIED || t == peerproto::TYPE_REJECTED => {
                    state = "denied";
                    remove = true;
                }
                t if t == peerproto::TYPE_RELEASED => {
                    state = "released";
                    remove = true;
                    released = true;
                }
                _ => return,
            }
            if remove {
                if let Some(t) = out.timer.take() {
                    t.abort();
                }
            }
        }
        if remove {
            self.outgoing.lock().unwrap().remove(&key);
        }
        if do_rearm {
            self.arm_request_timer(&mac, &dev_id, REQUEST_RESULT_TIMEOUT);
        }
        self.broadcast_req_status(&mac, &dev_id, state, json!({"holder": msg.user}));
        if released {
            let me = self.clone();
            tokio::spawn(async move { me.autotake(&mac, &dev_id).await });
        }
    }

    fn on_request_timeout(self: &Arc<Self>, mac: &str, dev_id: &str) {
        {
            let contains = self.outgoing.lock().unwrap().contains_key(&(mac.to_string(), dev_id.to_string()));
            if !contains {
                return;
            }
        }
        self.clear_outgoing(mac, dev_id);
        self.broadcast_req_status(mac, dev_id, "no_response", Value::Null);
    }

    async fn autotake(self: &Arc<Self>, mac: &str, dev_id: &str) {
        self.broadcast_req_status(mac, dev_id, "connecting", Value::Null);
        match self.connect(mac, dev_id).await {
            Ok(_) => self.broadcast_req_status(mac, dev_id, "connected", Value::Null),
            Err(e) => self.broadcast_req_status(mac, dev_id, "error", json!({"note": e.to_string()})),
        }
    }
}

#[cfg(test)]
mod tests;
