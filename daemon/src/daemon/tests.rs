//! Daemon orchestration tests using injected fakes (no hardware, no real UDP).

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::Result;

use super::*;
use crate::discovery::DeviceServer;
use crate::protocol::peer as pp;
use crate::protocol::udp_discovery::UsbDeviceEntry;
use crate::settings::SettingsStore;
use crate::usbip_control::UsbipOps;

const MAC: [u8; 6] = [0x00, 0x11, 0x22, 0x33, 0x44, 0x55];
const MAC_STR: &str = "00:11:22:33:44:55";

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn temp_store() -> Arc<SettingsStore> {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("osl-daemon-{}-{}.json", std::process::id(), n));
    let _ = std::fs::remove_file(&path);
    Arc::new(SettingsStore::new(Some(path)))
}

#[derive(Default)]
struct FakeUsbip {
    attached: Mutex<HashMap<String, i32>>,
    next: Mutex<i32>,
    calls: Mutex<Vec<String>>,
    mounted: AtomicBool,
}

impl UsbipOps for FakeUsbip {
    fn ensure_detachable(&self, _port: i32) -> Result<()> {
        if self.mounted.load(Ordering::Relaxed) { bail!("Unmount /media/test before disconnecting"); }
        Ok(())
    }
    fn ensure_module(&self) -> Result<()> {
        self.calls.lock().unwrap().push("ensure_module".into());
        Ok(())
    }
    fn used_ports(&self) -> HashSet<i32> {
        self.attached.lock().unwrap().values().copied().collect()
    }
    fn attach(&self, busid: &str) -> Result<()> {
        self.calls.lock().unwrap().push(format!("attach {busid}"));
        let mut n = self.next.lock().unwrap();
        *n += 1;
        self.attached.lock().unwrap().insert(busid.to_string(), *n);
        Ok(())
    }
    fn detach(&self, port: i32) -> Result<()> {
        self.calls.lock().unwrap().push(format!("detach {port}"));
        self.attached.lock().unwrap().retain(|_, v| *v != port);
        Ok(())
    }
}

impl FakeUsbip {
    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

#[derive(Default)]
struct FakePeer {
    sent: Mutex<Vec<(SocketAddr, PeerMessage)>>,
}

impl PeerSender for FakePeer {
    fn send(&self, addr: SocketAddr, packet: Vec<u8>) {
        if let Ok(m) = pp::parse(&packet) {
            self.sent.lock().unwrap().push((addr, m));
        }
    }
}

impl FakePeer {
    fn types(&self) -> Vec<u16> {
        self.sent.lock().unwrap().iter().map(|(_, m)| m.msg_type).collect()
    }
    fn dests(&self) -> Vec<SocketAddr> {
        self.sent.lock().unwrap().iter().map(|(a, _)| *a).collect()
    }
}

fn server() -> DeviceServer {
    DeviceServer {
        ip: "192.0.2.143".into(),
        mac: MAC.to_vec(),
        model: "DS-510".into(),
        version: String::new(),
        hostname: "usbserver".into(),
        attached_device: String::new(),
        devices: vec![
            UsbDeviceEntry::new("dev01", "Generic Mass Storage Device", 0x058f, 0x6387, "H111"),
            UsbDeviceEntry { usb_class: 0x08, ..UsbDeviceEntry::new("dev02", "USB Flash Memory", 0x0930, 0x6544, "H112") },
        ],
    }
}

fn make() -> (Arc<Daemon>, Arc<FakeUsbip>, Arc<FakePeer>) {
    let fake = Arc::new(FakeUsbip::default());
    let fpeer = Arc::new(FakePeer::default());
    let d = Daemon::new(temp_store(), fake.clone() as Arc<dyn UsbipOps>, false);
    *d.peer.lock().unwrap() = Some(fpeer.clone() as Arc<dyn PeerSender>);
    (d, fake, fpeer)
}

fn req(user: &str) -> Vec<u8> {
    pp::build(pp::TYPE_REQUEST, MAC, 0x0930, 0x6544, "USB Flash Memory", "H112", user)
}

fn resp(t: u16, user: &str) -> Vec<u8> {
    pp::build(t, MAC, 0x0930, 0x6544, "USB Flash Memory", "H112", user)
}

fn req_addr() -> SocketAddr {
    "192.0.2.9:54077".parse().unwrap()
}

fn holder_addr() -> SocketAddr {
    format!("192.0.2.50:{}", pp::PORT).parse().unwrap()
}

fn statuses(snap: &Value) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for srv in snap["servers"].as_array().unwrap() {
        for d in srv["devices"].as_array().unwrap() {
            out.insert(d["dev_id"].as_str().unwrap().to_string(), d["status"].as_str().unwrap().to_string());
        }
    }
    out
}

#[tokio::test]
async fn snapshot_lists_devices_available() {
    let (d, _f, _p) = make();
    d.apply_server(server(), true);
    let snap = d.servers_snapshot();
    let srv = &snap["servers"][0];
    assert_eq!(srv["mac"], "00:11:22:33:44:55");
    assert_eq!(srv["hostname"], "usbserver");
    let st = statuses(&snap);
    assert_eq!(st["dev01"], "available");
    assert_eq!(st["dev02"], "available");
    // key + type shape
    assert_eq!(srv["devices"][1]["key"], "0930:6544:USB Flash Memory");
    assert_eq!(srv["devices"][1]["type"], "Mass storage device");
}

fn password_server() -> DeviceServer {
    let mut srv = server();
    srv.model = "DS-600".into();
    srv.devices = [("usb4-007:0", "F414221M0"), ("usb5-003:0", "S512M0"), ("usb1-002:0", "F111M0")]
        .into_iter().enumerate().map(|(i, (id, tag))| {
            let mut d = UsbDeviceEntry::new(id, "protected", 1, i as u16, tag);
            d.status_flags = 0x10;
            d
        }).collect();
    srv
}

#[tokio::test]
async fn physical_port_passwords_enable_only_matching_devices_without_exposing_secrets() {
    let (d, _, _) = make();
    d.apply_server(password_server(), true);
    assert!(d.connect(MAC_STR, "usb4-007:0").await.unwrap_err().to_string().contains("password required"));
    let status = d.set_port_passwords(MAC_STR, json!({"usb3":"private-three"}).as_object().unwrap()).await.unwrap();
    assert_eq!(status, json!({"usb2":false,"usb3":true}));
    let snap = d.servers_snapshot();
    let devices = snap["servers"][0]["devices"].as_array().unwrap();
    assert!(devices.iter().filter(|dev| dev["usb_port"] == "usb3").all(|dev| dev["password_configured"] == true));
    assert!(devices.iter().filter(|dev| dev["usb_port"] == "usb2").all(|dev| dev["password_configured"] == false));
    assert!(!snap.to_string().contains("private-three"));
    assert!(!d.store.get().to_value().to_string().contains("private-three"));
    let passwords = d.export_passwords(&password_server());
    assert_eq!(passwords.get("usb4-007:0"), Some(&b"private-three".to_vec()));
    assert_eq!(passwords.get("usb5-003:0"), Some(&b"private-three".to_vec()));
    assert!(!passwords.contains_key("usb1-002:0"));
    d.connect(MAC_STR, "usb4-007:0").await.unwrap();
    assert!(d.connect(MAC_STR, "usb1-002:0").await.is_err());
    d.shutdown().await;
}

#[tokio::test]
async fn bind_all_uses_saved_physical_port_secrets_after_service_restart() {
    let (d, _, _) = make();
    let srv = password_server();
    d.apply_server(srv.clone(), true);
    d.set_port_passwords(MAC_STR, json!({"usb3":"private-three"}).as_object().unwrap()).await.unwrap();
    d.set_bind_all(MAC_STR, true).await.unwrap();
    wait_for_connections(&d, 2).await;
    d.set_port_passwords(MAC_STR, json!({"usb2":"private-two"}).as_object().unwrap()).await.unwrap();
    wait_for_connections(&d, 3).await;
    d.shutdown().await;
    let restarted = Daemon::new(d.store.clone(), Arc::new(FakeUsbip::default()), false);
    restarted.apply_server(srv, true);
    restarted.maybe_autoconnect();
    wait_for_connections(&restarted, 3).await;
    assert_eq!(restarted.credentials.password(MAC_STR, "usb2"), Some(b"private-two".to_vec()));
    restarted.disconnect(MAC_STR, "usb1-002:0").await.unwrap();
    restarted.maybe_autoconnect();
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(!restarted.connections.lock().unwrap().contains_key(&(MAC_STR.into(), "usb1-002:0".into())));
    restarted.shutdown().await;
}

#[tokio::test]
async fn protected_device_recovers_with_its_physical_port_password_after_renumbering() {
    let (d, fake, _) = make();
    let mut srv = password_server();
    d.apply_server(srv.clone(), true);
    d.set_port_passwords(MAC_STR, json!({"usb3":"dock-secret"}).as_object().unwrap()).await.unwrap();
    d.connect(MAC_STR, "usb4-007:0").await.unwrap();
    let port = d.connections.lock().unwrap()[&(MAC_STR.into(), "usb4-007:0".into())].port.unwrap();
    fake.detach(port).unwrap();
    d.check_connections();
    srv.devices[0].dev_id = "usb4-012:0".into();
    d.apply_server(srv.clone(), true);
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if d.connections.lock().unwrap().contains_key(&(MAC_STR.into(), "usb4-012:0".into())) { break; }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await.unwrap();
    assert_eq!(d.export_passwords(&srv).get("usb4-012:0"), Some(&b"dock-secret".to_vec()));
    d.shutdown().await;
}

#[tokio::test]
async fn connect_attaches_and_marks_connected() {
    let (d, fake, _p) = make();
    d.apply_server(server(), true);
    let res = d.connect(MAC_STR, "dev02").await.unwrap();
    assert_eq!(res["connected"], true);
    assert!(fake.calls().iter().any(|c| c == "attach 1-2"));
    assert!(d.connections.lock().unwrap().contains_key(&(MAC_STR.into(), "dev02".into())));
    assert_eq!(statuses(&d.servers_snapshot())["dev02"], "connected");
}

#[tokio::test]
async fn owner_ip_drives_status() {
    let (d, _f, _p) = make();
    let mut s = server();
    s.devices[1].owner_ip = "192.0.2.119".into(); // us
    s.devices.push(UsbDeviceEntry {
        owner_ip: "192.0.2.77".into(),
        ..UsbDeviceEntry::new("dev03", "Other", 0x1, 0x2, "H1")
    });
    d.apply_server(s, true);
    d.our_ip.lock().unwrap().insert("192.0.2.143".into(), "192.0.2.119".into());
    let st = statuses(&d.servers_snapshot());
    assert_eq!(st["dev01"], "available");
    assert_eq!(st["dev02"], "connected"); // owner == our ip
    assert_eq!(st["dev03"], "in_use"); // foreign owner
}

#[tokio::test]
async fn peer_request_for_unheld_device_only_acks() {
    let (d, _f, fpeer) = make();
    d.apply_server(server(), true); // we hold nothing
    d.on_peer_message(req_addr(), req("Testuser"));
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(fpeer.types(), vec![pp::TYPE_ACK]);
    assert!(d.pending_requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn peer_request_prompts_then_deny_keeps_connection() {
    let (d, _f, fpeer) = make();
    d.apply_server(server(), true);
    d.connect(MAC_STR, "dev02").await.unwrap();
    d.on_peer_message(req_addr(), req("Testuser"));
    assert_eq!(fpeer.types(), vec![pp::TYPE_ACK]);
    assert!(d.pending_requests.lock().unwrap().contains_key(&(MAC_STR.into(), "dev02".into())));
    let res = d.respond_access_request(MAC_STR, "dev02", false).await.unwrap();
    assert_eq!(res["accepted"], false);
    assert_eq!(fpeer.types(), vec![pp::TYPE_ACK, pp::TYPE_DENIED]);
    assert!(d.connections.lock().unwrap().contains_key(&(MAC_STR.into(), "dev02".into())));
}

#[tokio::test]
async fn peer_request_accept_disconnects() {
    let (d, fake, fpeer) = make();
    d.apply_server(server(), true);
    d.connect(MAC_STR, "dev02").await.unwrap();
    d.on_peer_message(req_addr(), req("Testuser"));
    let res = d.respond_access_request(MAC_STR, "dev02", true).await.unwrap();
    assert_eq!(res["accepted"], true);
    assert_eq!(fpeer.types(), vec![pp::TYPE_ACK, pp::TYPE_ACCEPTED, pp::TYPE_RELEASED]);
    assert!(!d.connections.lock().unwrap().contains_key(&(MAC_STR.into(), "dev02".into())));
    assert!(fake.calls().iter().any(|c| c == "detach 1"));
}

#[tokio::test]
async fn prune_phantoms_detaches_unplugged_device() {
    let (d, fake, _p) = make();
    d.apply_server(server(), true);
    d.connect(MAC_STR, "dev02").await.unwrap();
    assert!(d.connections.lock().unwrap().contains_key(&(MAC_STR.into(), "dev02".into())));
    // dev02 unplugged: the device server now reports a list WITHOUT dev02 (authoritative).
    let mut gone = server();
    gone.devices.retain(|e| e.dev_id != "dev02");
    d.apply_server(gone, true);
    d.prune_phantoms();
    assert!(!d.connections.lock().unwrap().contains_key(&(MAC_STR.into(), "dev02".into())));
    assert!(fake.calls().iter().any(|c| c == "detach 1"));
}

#[tokio::test]
async fn ds600_last_device_unplug_detaches_and_keeps_recovery_visible() {
    let (d, fake, _p) = make();
    d.apply_server(server(), true);
    d.connect(MAC_STR, "dev02").await.unwrap();
    // Actual DS-600 empty-list response, through the UDP parser and daemon.
    let message = ud::parse(&hex::decode("000100000000000100").unwrap()).unwrap();
    d.on_disc_message("192.0.2.143".into(), message);
    d.prune_phantoms();
    assert_eq!(statuses(&d.servers_snapshot()).get("dev02").map(String::as_str), Some(STATUS_RECONNECTING));
    assert!(!d.connections.lock().unwrap().contains_key(&(MAC_STR.into(), "dev02".into())));
    assert!(fake.calls().iter().any(|c| c == "detach 1"));
    d.disconnect(MAC_STR, "dev02").await.unwrap();
    assert!(statuses(&d.servers_snapshot()).is_empty());
}

#[tokio::test]
async fn rule_auto_disconnect_immediate() {
    let (d, _f, fpeer) = make();
    d.apply_server(server(), true);
    d.connect(MAC_STR, "dev02").await.unwrap();
    d.store
        .set_device_rule(
            MAC_STR,
            "0930:6544:USB Flash Memory",
            &serde_json::json!({"auto_disconnect_on_request": true, "auto_disconnect_timeout": 0})
                .as_object()
                .unwrap()
                .clone(),
        )
        .unwrap();
    d.on_peer_message(req_addr(), req("Testuser"));
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert_eq!(fpeer.types(), vec![pp::TYPE_ACK, pp::TYPE_ACCEPTED, pp::TYPE_RELEASED]);
    // all replies go to the requester's source port (not 49300)
    assert!(fpeer.dests().iter().all(|a| *a == req_addr()));
    assert!(!d.connections.lock().unwrap().contains_key(&(MAC_STR.into(), "dev02".into())));
}

#[tokio::test]
async fn request_access_sends_to_holder() {
    let (d, _f, fpeer) = make();
    let mut s = server();
    s.devices[1].owner_ip = "192.0.2.50".into();
    d.apply_server(s, true);
    d.our_ip.lock().unwrap().insert("192.0.2.143".into(), "192.0.2.99".into());
    let res = d.request_access(MAC_STR, "dev02").await.unwrap();
    assert_eq!(res["owner_ip"], "192.0.2.50");
    let sent = fpeer.sent.lock().unwrap();
    assert_eq!(sent[0].0, holder_addr());
    assert_eq!(sent[0].1.msg_type, pp::TYPE_REQUEST);
    assert!(d.outgoing.lock().unwrap().contains_key(&(MAC_STR.into(), "dev02".into())));
}

#[tokio::test]
async fn request_access_rejects_non_foreign() {
    let (d, _f, _p) = make();
    d.apply_server(server(), true); // dev01 has no owner
    assert!(d.request_access(MAC_STR, "dev01").await.is_err());
}

#[tokio::test]
async fn request_denied_flow_clears_outgoing() {
    let (d, _f, _p) = make();
    let mut s = server();
    s.devices[1].owner_ip = "192.0.2.50".into();
    d.apply_server(s, true);
    d.our_ip.lock().unwrap().insert("192.0.2.143".into(), "192.0.2.99".into());
    d.request_access(MAC_STR, "dev02").await.unwrap();
    d.on_peer_message(holder_addr(), resp(pp::TYPE_ACK, "Holder"));
    d.on_peer_message(holder_addr(), resp(pp::TYPE_DENIED, "Holder"));
    assert!(!d.outgoing.lock().unwrap().contains_key(&(MAC_STR.into(), "dev02".into())));
}

#[tokio::test]
async fn request_released_autotakes() {
    let (d, fake, _p) = make();
    let mut s = server();
    s.devices[1].owner_ip = "192.0.2.50".into();
    d.apply_server(s, true);
    d.our_ip.lock().unwrap().insert("192.0.2.143".into(), "192.0.2.99".into());
    d.request_access(MAC_STR, "dev02").await.unwrap();
    d.on_peer_message(holder_addr(), resp(pp::TYPE_ACK, "Holder"));
    d.on_peer_message(holder_addr(), resp(pp::TYPE_ACCEPTED, "Holder"));
    d.on_peer_message(holder_addr(), resp(pp::TYPE_RELEASED, "Holder"));
    // autotake runs in a spawned task
    for _ in 0..50 {
        if d.connections.lock().unwrap().contains_key(&(MAC_STR.into(), "dev02".into())) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(d.connections.lock().unwrap().contains_key(&(MAC_STR.into(), "dev02".into())));
    assert!(fake.calls().iter().any(|c| c == "attach 1-2"));
}

#[tokio::test]
async fn respond_without_pending_is_noop() {
    let (d, _f, _p) = make();
    d.apply_server(server(), true);
    let res = d.respond_access_request(MAC_STR, "dev02", true).await.unwrap();
    assert_eq!(res["ok"], false);
}

#[tokio::test]
async fn mounted_storage_refusal_preserves_connection_and_rejects_peer_release() {
    let (d, fake, peer) = make();
    d.apply_server(server(), true);
    d.connect(MAC_STR, "dev02").await.unwrap();
    fake.mounted.store(true, Ordering::Relaxed);
    assert!(d.disconnect(MAC_STR, "dev02").await.unwrap_err().to_string().contains("Unmount"));
    assert!(d.connections.lock().unwrap().contains_key(&(MAC_STR.into(), "dev02".into())));
    assert!(!fake.calls().iter().any(|c| c.starts_with("detach")));
    let message = pp::parse(&req("Requester")).unwrap();
    d.accept_peer_request(req_addr(), message, MAC_STR, "dev02").await;
    assert!(peer.types().contains(&pp::TYPE_DENIED));
    assert!(!peer.types().contains(&pp::TYPE_RELEASED));
    fake.mounted.store(false, Ordering::Relaxed);
    d.disconnect(MAC_STR, "dev02").await.unwrap();
    assert!(d.connections.lock().unwrap().is_empty());
}

#[tokio::test]
async fn another_server_cannot_reuse_first_servers_export() {
    let (d, fake, _) = make();
    d.apply_server(server(), true);
    d.connect(MAC_STR, "dev01").await.unwrap();
    let mut other = server();
    other.ip = "192.0.2.144".into();
    other.mac = vec![6, 5, 4, 3, 2, 1];
    let other_mac = other.mac_str();
    d.apply_server(other, true);
    assert!(d.connect(&other_mac, "dev01").await.unwrap_err().to_string().contains("one device server"));
    assert_eq!(fake.calls().iter().filter(|c| c.starts_with("attach")).count(), 1);
    d.disconnect(MAC_STR, "dev01").await.unwrap();
    d.connect(&other_mac, "dev01").await.unwrap();
    d.shutdown().await;
    assert!(fake.used_ports().is_empty());
}

#[tokio::test]
async fn blocking_attach_leaves_runtime_free_to_serve_the_exporter() {
    struct WaitingUsbip {
        fake: FakeUsbip,
        exporter_ready: Arc<AtomicBool>,
    }
    impl UsbipOps for WaitingUsbip {
        fn ensure_module(&self) -> Result<()> { Ok(()) }
        fn used_ports(&self) -> HashSet<i32> { self.fake.used_ports() }
        fn detach(&self, port: i32) -> Result<()> { self.fake.detach(port) }
        fn attach(&self, busid: &str) -> Result<()> {
            let deadline = Instant::now() + Duration::from_millis(300);
            while !self.exporter_ready.load(Ordering::SeqCst) {
                if Instant::now() > deadline { bail!("exporter was starved by blocking attach"); }
                std::thread::sleep(Duration::from_millis(1));
            }
            self.fake.attach(busid)
        }
    }
    let ready = Arc::new(AtomicBool::new(false));
    let usbip = Arc::new(WaitingUsbip { fake: FakeUsbip::default(), exporter_ready: ready.clone() });
    let daemon = Daemon::new(temp_store(), usbip, false);
    daemon.apply_server(server(), true);
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        ready.store(true, Ordering::SeqCst);
    });
    daemon.connect(MAC_STR, "dev01").await.unwrap();
    daemon.shutdown().await;
}

#[tokio::test]
async fn manual_disconnect_is_not_undone_by_an_autoconnect_rule() {
    let (d, _, _) = make();
    let server = server();
    let key = device_key(&server.devices[0]);
    d.apply_server(server, true);
    d.store.set_device_rule(MAC_STR, &key,
        &serde_json::from_value(json!({"auto_connect":true})).unwrap()).unwrap();
    d.connect(MAC_STR, "dev01").await.unwrap();
    d.disconnect(MAC_STR, "dev01").await.unwrap();
    d.maybe_autoconnect();
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(d.connections.lock().unwrap().is_empty());
    assert!(d.connect_inner(MAC_STR, "dev01", true).await.is_err());
    d.connect(MAC_STR, "dev01").await.unwrap();
    assert!(!d.manual_disconnects.lock().unwrap().contains(&(MAC_STR.into(), "dev01".into())));
    d.shutdown().await;
}

fn recovery_connection() -> Connection {
    let device = server().devices[0].clone();
    Connection { server_mac: MAC_STR.into(), dev_id: device.dev_id.clone(), busid: "1-1".into(),
        port: Some(1), description: device.description.clone(), entry: device }
}

async fn wait_for_connections(d: &Arc<Daemon>, expected: usize) {
    for _ in 0..100 {
        if d.connections.lock().unwrap().len() == expected { return; }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("expected {expected} connected devices, got {}", d.connections.lock().unwrap().len());
}

#[tokio::test]
async fn bind_all_connects_current_and_new_devices_and_respects_manual_disconnect() {
    let (d, _, _) = make();
    let mut srv = server();
    d.apply_server(srv.clone(), true);
    d.set_bind_all(MAC_STR, true).await.unwrap();
    wait_for_connections(&d, 2).await;
    assert_eq!(d.servers_snapshot()["servers"][0]["bind_all"], true);
    d.disconnect(MAC_STR, "dev01").await.unwrap();
    srv.devices.push(UsbDeviceEntry::new("dev03", "Dock receiver", 0x046d, 0xc52b, "F113"));
    d.apply_server(srv, true);
    d.maybe_autoconnect();
    wait_for_connections(&d, 2).await;
    assert!(!d.connections.lock().unwrap().contains_key(&(MAC_STR.into(), "dev01".into())));
    assert!(d.connections.lock().unwrap().contains_key(&(MAC_STR.into(), "dev03".into())));
    d.set_bind_all(MAC_STR, false).await.unwrap();
    assert_eq!(d.connections.lock().unwrap().len(), 2, "disabling does not detach active devices");
    d.set_bind_all(MAC_STR, true).await.unwrap();
    wait_for_connections(&d, 3).await;
    d.shutdown().await;
}

#[tokio::test]
async fn bind_all_survives_restart_and_skips_busy_or_password_protected_devices() {
    let (d, _, _) = make();
    let mut srv = server();
    srv.devices[1].owner_ip = "192.0.2.99".into();
    let mut protected = UsbDeviceEntry::new("dev03", "Protected port", 1, 2, "H113");
    protected.status_flags = 0x10;
    srv.devices.push(protected);
    d.apply_server(srv.clone(), true);
    d.set_bind_all(MAC_STR, true).await.unwrap();
    wait_for_connections(&d, 1).await;
    let store = d.store.clone();
    d.shutdown().await;
    let restarted = Daemon::new(store, Arc::new(FakeUsbip::default()), false);
    restarted.apply_server(srv, true);
    restarted.maybe_autoconnect();
    wait_for_connections(&restarted, 1).await;
    assert!(restarted.connections.lock().unwrap().contains_key(&(MAC_STR.into(), "dev01".into())));
    restarted.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn recovery_keeps_retrying_through_long_outages_and_manual_disconnect_cancels() {
    let (d, _, _) = make();
    d.queue_reconnect(recovery_connection());
    tokio::task::yield_now().await;
    for _ in 0..30 {
        tokio::time::advance(Duration::from_secs(10)).await;
        tokio::task::yield_now().await;
    }
    assert!(d.reconnecting.lock().unwrap().contains_key(&(MAC_STR.into(), "dev01".into())));
    d.disconnect(MAC_STR, "dev01").await.unwrap();
    tokio::time::advance(Duration::from_secs(10)).await;
    tokio::task::yield_now().await;
    assert!(d.reconnecting.lock().unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn disabling_recovery_stops_a_pending_retry_loop() {
    let (d, _, _) = make();
    let connection = recovery_connection();
    let key = device_key(&connection.entry);
    d.queue_reconnect(connection);
    tokio::task::yield_now().await;
    d.store.set_device_rule(MAC_STR, &key,
        &serde_json::from_value(json!({"reconnect_on_error":false})).unwrap()).unwrap();
    tokio::time::advance(Duration::from_secs(10)).await;
    tokio::task::yield_now().await;
    assert!(d.reconnecting.lock().unwrap().is_empty());
}

#[tokio::test]
async fn startup_connection_does_not_override_disabled_loss_recovery() {
    let (d, fake, _) = make();
    let s = server();
    let key = device_key(&s.devices[0]);
    d.apply_server(s.clone(), true);
    d.store.set_device_rule(MAC_STR, &key,
        &serde_json::from_value(json!({"auto_connect":true,"reconnect_on_error":false})).unwrap()).unwrap();
    d.connect(MAC_STR, "dev01").await.unwrap();
    let port = d.connections.lock().unwrap()[&(MAC_STR.into(), "dev01".into())].port.unwrap();
    fake.detach(port).unwrap();
    d.check_connections();
    d.maybe_autoconnect();
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(d.connections.lock().unwrap().is_empty());
    assert!(d.reconnecting.lock().unwrap().is_empty());
    // A fresh manager uses the persisted boot rule, with no desktop client.
    let rebooted = Daemon::new(d.store.clone(), fake.clone(), false);
    rebooted.apply_server(s, true);
    rebooted.maybe_autoconnect();
    for _ in 0..50 {
        if !rebooted.connections.lock().unwrap().is_empty() { break; }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(rebooted.connections.lock().unwrap().len(), 1);
    rebooted.shutdown().await;
}

#[tokio::test]
async fn recovery_tracks_changed_usb_address_without_detaching_the_other_device() {
    let (d, fake, _) = make();
    let mut s = server();
    d.apply_server(s.clone(), true);
    d.connect(MAC_STR, "dev01").await.unwrap();
    d.connect(MAC_STR, "dev02").await.unwrap();
    let first = d.connections.lock().unwrap()[&(MAC_STR.into(), "dev01".into())].clone();
    let other = d.connections.lock().unwrap()[&(MAC_STR.into(), "dev02".into())].clone();
    fake.detach(first.port.unwrap()).unwrap();
    d.check_connections();
    s.devices[0].dev_id = "dev03".into();
    d.apply_server(s, true);
    for _ in 0..150 {
        if d.connections.lock().unwrap().contains_key(&(MAC_STR.into(), "dev03".into())) { break; }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let connections = d.connections.lock().unwrap().clone();
    assert_eq!(connections[&(MAC_STR.into(), "dev03".into())].busid, first.busid);
    assert_eq!(connections[&(MAC_STR.into(), "dev02".into())].port, other.port);
    assert!(d.reconnecting.lock().unwrap().is_empty());
    d.shutdown().await;
}
