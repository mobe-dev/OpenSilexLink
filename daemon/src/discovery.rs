//! Live discovery of Silex device servers on the LAN (UDP/19540).
//!
//! Two combined paths: passive (the device periodically sends announcements
//! 0x0002/0x9506) and active (we broadcast a GET USB_DEVICE_LIST query plus a poll,
//! which makes the device announce itself and add us to its notify list).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::time::{timeout, Instant};

use crate::protocol::udp_discovery::{self as ud, Announcement, Message};

/// Device lists larger than 512 bytes require opcode 2 continuation requests.
/// Keep transfers separate by sender and publish only a complete, valid list.
#[derive(Default)]
struct DeviceLists(HashMap<SocketAddr, (Vec<u8>, Instant)>);

impl DeviceLists {
    fn receive(&mut self, addr: SocketAddr, data: &[u8]) -> (Option<Message>, Option<Vec<u8>>) {
        self.0.retain(|_, (_, seen)| seen.elapsed() < Duration::from_secs(5));
        if data.len() < 8 {
            return (ud::parse(data).ok(), None);
        }
        let opcode = u16::from_be_bytes([data[0], data[1]]);
        let more = u16::from_be_bytes([data[2], data[3]]);
        let offset = u16::from_be_bytes([data[4], data[5]]) as usize;
        let length = u16::from_be_bytes([data[6], data[7]]) as usize;
        let first = opcode == 1 && offset == 0 && data.len() >= 9
            && (data[8] == 0 || data[9..].starts_with(b"usb") || data[9..].starts_with(b"dev"));
        let continuation = opcode == 2 && self.0.contains_key(&addr);
        if opcode == 2 && more <= 1 && !continuation {
            return (None, None);
        }
        if more > 1 || !(first || continuation) {
            return (ud::parse(data).ok(), None);
        }
        if data.len() != 8 + length || length == 0 || offset + length > u16::MAX as usize {
            self.0.remove(&addr);
            return (None, None);
        }
        if first {
            self.0.insert(addr, (Vec::new(), Instant::now()));
        }
        let (payload, seen) = self.0.get_mut(&addr).unwrap();
        // Retransmissions are harmless; missing or conflicting bytes never form a list.
        if offset < payload.len() && offset + length <= payload.len()
            && payload[offset..offset + length] == data[8..] {
        } else if offset == payload.len() {
            payload.extend_from_slice(&data[8..]);
        } else {
            self.0.remove(&addr);
            return (None, None);
        }
        *seen = Instant::now();
        let total = payload.len();
        let mut reply = ud::build_get_device_list(ud::Phase::Query, Some(total as u16));
        if more == 1 {
            reply[..2].copy_from_slice(&2u16.to_be_bytes());
            return (None, Some(reply));
        }
        let (payload, _) = self.0.remove(&addr).unwrap();
        let mut complete = vec![0, 1, 0, 0, 0, 0];
        complete.extend_from_slice(&(total as u16).to_be_bytes());
        complete.extend_from_slice(&payload);
        let message = ud::parse(&complete).ok().filter(|m| matches!(m, Message::DeviceList(_)));
        reply[..2].copy_from_slice(&3u16.to_be_bytes());
        (message, Some(reply))
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeviceServer {
    pub ip: String,
    pub mac: Vec<u8>,
    pub model: String,
    pub version: String,
    pub hostname: String,
    pub attached_device: String,
    pub devices: Vec<ud::UsbDeviceEntry>,
}

impl DeviceServer {
    /// Physical connector, not the negotiated USB speed or a downstream hub port.
    /// DS-600 firmware S90svu assigns buses 4/5 to USB 3.0 and bus 1 to USB 2.0.
    pub fn usb_port(&self, device: &ud::UsbDeviceEntry) -> Option<&'static str> {
        if !self.model.eq_ignore_ascii_case("DS-600") { return None; }
        let (bus, _) = device.dev_id.strip_prefix("usb")?.split_once('-')?;
        match bus {
            "4" | "5" => Some("usb3"),
            "1" => Some("usb2"),
            _ => None,
        }
    }

    pub fn new(ip: &str) -> Self {
        Self { ip: ip.to_string(), ..Default::default() }
    }

    pub fn mac_str(&self) -> String {
        if self.mac.len() == 6 {
            self.mac.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":")
        } else {
            String::new()
        }
    }

    pub fn update_from_announcement(&mut self, ann: &Announcement) {
        self.mac = ann.mac.clone();
        self.model = ann.model.clone();
        self.version = ann.version.clone();
        self.hostname = ann.hostname.clone();
        self.attached_device = ann.attached_device.clone();
    }
}

/// Best-effort local IP used to reach `peer_ip`.
pub fn local_ip_towards(peer_ip: &str) -> Option<String> {
    let s = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect((peer_ip, ud::PORT)).ok()?;
    Some(s.local_addr().ok()?.ip().to_string())
}

/// Bind a discovery UDP socket: prefer port 19540 (to catch broadcast announcements),
/// falling back to an ephemeral port (send-only / unicast replies) if it is in use.
async fn bind_discovery_socket() -> Result<UdpSocket> {
    let sock = match UdpSocket::bind(("0.0.0.0", ud::PORT)).await {
        Ok(s) => s,
        Err(_) => UdpSocket::bind("0.0.0.0:0").await?,
    };
    sock.set_broadcast(true).ok();
    Ok(sock)
}

async fn send_active(sock: &UdpSocket, targets: &[(String, u16)]) {
    let get = ud::build_get_device_list(ud::Phase::Query, None);
    let poll = ud::build_poll();
    for (host, port) in targets {
        let _ = sock.send_to(&get, (host.as_str(), *port)).await;
        let _ = sock.send_to(&poll, (host.as_str(), *port)).await;
    }
}

fn ingest(servers: &mut HashMap<String, DeviceServer>, src_ip: String, data: &[u8]) {
    let msg = match ud::parse(data) {
        Ok(m) => m,
        Err(_) => return,
    };
    // Only genuine device->host replies create a server entry; our own reflected
    // broadcast commands/polls are ignored (otherwise the local host shows up empty).
    match msg {
        Message::Announcement(ann) => {
            let srv = servers.entry(src_ip.clone()).or_insert_with(|| DeviceServer::new(&src_ip));
            srv.update_from_announcement(&ann);
            if srv.ip.is_empty() {
                srv.ip = ann.ip;
            }
        }
        Message::DeviceList(list) => {
            let srv = servers.entry(src_ip.clone()).or_insert_with(|| DeviceServer::new(&src_ip));
            srv.devices = list.devices;
        }
        _ => {}
    }
}

/// One-shot discovery for `timeout`, returning servers keyed uniquely by IP.
pub async fn discover(timeout_dur: Duration, broadcast: &str) -> Vec<DeviceServer> {
    let sock = match bind_discovery_socket().await {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    send_active(&sock, &[(broadcast.to_string(), ud::PORT)]).await;

    let mut servers: HashMap<String, DeviceServer> = HashMap::new();
    let deadline = Instant::now() + timeout_dur;
    let mut buf = [0u8; 4096];
    let mut lists = DeviceLists::default();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match timeout(remaining, sock.recv_from(&mut buf)).await {
            Ok(Ok((n, addr))) => {
                let (message, reply) = lists.receive(addr, &buf[..n]);
                if let Some(reply) = reply {
                    let _ = sock.send_to(&reply, addr).await;
                }
                if let Some(message) = message {
                    match message {
                        Message::DeviceList(list) => {
                            servers.entry(addr.ip().to_string()).or_insert_with(|| DeviceServer::new(&addr.ip().to_string())).devices = list.devices;
                        }
                        _ => ingest(&mut servers, addr.ip().to_string(), &buf[..n]),
                    }
                }
            }
            _ => break,
        }
    }
    servers.into_values().collect()
}

/// Persistent discovery socket for the daemon: it stays open and receives the periodic
/// announcements at any time (not just in a short query window). Parsed messages are
/// delivered over the returned channel; [`poll`](DiscoveryListener::poll) actively
/// triggers a device-list query (broadcast + optional unicast search addresses).
pub struct DiscoveryListener {
    sock: Arc<UdpSocket>,
    broadcast: String,
}

impl DiscoveryListener {
    pub async fn start(broadcast: &str) -> Result<(Arc<Self>, mpsc::Receiver<(String, Message)>)> {
        let sock = Arc::new(bind_discovery_socket().await?);
        let (tx, rx) = mpsc::channel::<(String, Message)>(256);
        let recv_sock = sock.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 4096];
            let mut lists = DeviceLists::default();
            loop {
                match recv_sock.recv_from(&mut buf).await {
                    Ok((n, addr)) => {
                        let (message, reply) = lists.receive(addr, &buf[..n]);
                        if let Some(reply) = reply {
                            let _ = recv_sock.send_to(&reply, addr).await;
                        }
                        if let Some(msg) = message {
                            if tx.send((addr.ip().to_string(), msg)).await.is_err() {
                                return;
                            }
                        }
                    }
                    Err(_) => return,
                }
            }
        });
        Ok((Arc::new(Self { sock, broadcast: broadcast.to_string() }), rx))
    }

    pub async fn poll(&self, addresses: &[(String, u16)]) {
        let mut targets = vec![(self.broadcast.clone(), ud::PORT)];
        targets.extend_from_slice(addresses);
        send_active(&self.sock, &targets).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ds600_physical_connector_does_not_depend_on_speed_or_hub_depth() {
        let srv = DeviceServer { model: "DS-600".into(), ..Default::default() };
        for (dev_id, tag, port) in [
            ("usb4-007:0", "F414221M0", Some("usb3")),
            ("usb4-007:1", "F414221M1", Some("usb3")),
            ("usb5-003:0", "S512M0", Some("usb3")),
            ("usb1-002:0", "F111M0", Some("usb2")),
            ("usb2-002:0", "F211", None),
            ("dev01", "H111", None),
        ] {
            let device = ud::UsbDeviceEntry::new(dev_id, "test", 1, 2, tag);
            assert_eq!(srv.usb_port(&device), port);
            assert_eq!(DeviceServer { model: "DS-510".into(), ..Default::default() }.usb_port(&device), None);
        }
    }

    fn list_fragment(op: u16, more: u16, offset: usize, payload: &[u8]) -> Vec<u8> {
        let mut packet = Vec::new();
        for field in [op, more, offset as u16, payload.len() as u16] {
            packet.extend_from_slice(&field.to_be_bytes());
        }
        packet.extend_from_slice(payload);
        packet
    }

    #[test]
    fn fragmented_list_waits_for_all_bytes_and_requests_continuation() {
        let addr = "192.0.2.1:19540".parse().unwrap();
        let mut reader = DeviceLists::default();
        let mut payload = vec![1];
        payload.extend_from_slice(b"usb5-005:0\0DisplayLink Dock\0");
        payload.extend_from_slice(&[0; 32]);
        let split = 20; // inside the description; the first fragment cannot be parsed
        let (message, next) = reader.receive(addr, &list_fragment(1, 1, 0, &payload[..split]));
        assert!(message.is_none());
        let next = next.unwrap();
        assert_eq!(&next[..2], &[0, 2]);
        assert_eq!(&next[84..86], &(split as u16).to_be_bytes());
        let (message, commit) = reader.receive(addr, &list_fragment(2, 0, split, &payload[split..]));
        let Some(Message::DeviceList(list)) = message else { panic!("expected complete list") };
        assert_eq!(list.devices[0].description, "DisplayLink Dock");
        let commit = commit.unwrap();
        assert_eq!(&commit[..2], &[0, 3]);
        assert_eq!(&commit[84..86], &(payload.len() as u16).to_be_bytes());
        assert!(reader.0.is_empty());
    }

    #[test]
    fn fragmented_list_rejects_missing_bytes_and_expired_transfers() {
        let addr = "192.0.2.1:19540".parse().unwrap();
        let mut reader = DeviceLists::default();
        let first = list_fragment(1, 1, 0, b"\x01usb5-005:0\0Dock");
        reader.receive(addr, &first);
        assert_eq!(reader.receive(addr, &list_fragment(2, 0, 30, b"tail")), (None, None));
        assert!(reader.0.is_empty());
        reader.receive(addr, &first);
        reader.0.get_mut(&addr).unwrap().1 = Instant::now() - Duration::from_secs(6);
        let (message, reply) = reader.receive(addr, &list_fragment(2, 0, 14, b"tail"));
        assert!(!matches!(message, Some(Message::DeviceList(_))));
        assert!(reply.is_none());
        assert!(reader.0.is_empty());
    }

    #[test]
    fn mac_str_formats() {
        let mut s = DeviceServer::new("192.0.2.1");
        assert_eq!(s.mac_str(), "");
        s.mac = vec![0x00, 0x11, 0x22, 0x33, 0x44, 0x55];
        assert_eq!(s.mac_str(), "00:11:22:33:44:55");
    }

    #[test]
    fn update_from_announcement_fills_fields() {
        let ann = Announcement {
            model: "DS-510".into(),
            mac: vec![0, 1, 2, 3, 4, 5],
            ip: "192.0.2.9".into(),
            version: "Ver 2.2.0".into(),
            attached_device: "Mass Storage".into(),
            hostname: "usbserver".into(),
        };
        let mut s = DeviceServer::new("192.0.2.9");
        s.update_from_announcement(&ann);
        assert_eq!(s.model, "DS-510");
        assert_eq!(s.hostname, "usbserver");
        assert_eq!(s.mac_str(), "00:01:02:03:04:05");
    }

    #[tokio::test]
    async fn discover_returns_empty_without_devices() {
        // No device answers on loopback broadcast within a short window.
        let found = discover(Duration::from_millis(150), "127.0.0.1").await;
        assert!(found.is_empty());
    }
}
