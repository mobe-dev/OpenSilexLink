//! Minimal USB/IP server that exports a Silex USB device to the local kernel.
//!
//! The Linux client (`usbip list -r 127.0.0.1`, `usbip attach -r 127.0.0.1 -b <busid>`)
//! connects here; `vhci-hcd` imports the device and enumerates it over the URB stream,
//! which we translate 1:1 into the Silex tunnel.
//!
//! Concurrency: the reader routes each URB (via [`SilexDevice::session_for`]) to the queue
//! of its connection, so control, iso and interrupt do not block each other. A plain
//! control-only connection runs a synchronous worker; an iso connection runs a pipelined
//! worker; bulk and interrupt connections run a multiplex sender+reader so a pending
//! read cannot block writes. Responses are matched to requests by sequence number.
//! UNLINK is answered immediately; a send lock serializes RET_SUBMIT/RET_UNLINK.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use anyhow::{bail, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Mutex, Notify};
use tokio::time::timeout;

use super::device::SilexDevice;
use super::proto::{self as up, OpHeader, UsbDevice, UsbInterface, UsbipCmdSubmit, UsbipCmdUnlink, UsbipRetSubmit, UsbipRetUnlink};
use super::session::SilexSession;
use super::translate::{
    build_request, fill_ret_from_response, iso_fill_ret, iso_fill_ret_out, iso_out_request_params,
    iso_request_params, ret_for, submit_to_silex,
};

const EPIPE: i32 = -32;
const KEEPALIVE: Duration = Duration::from_secs(10);
// Max iso requests kept "in flight" (a deep, continuous pipeline buffer like SX Virtual
// Link, which keeps ~200 ahead). The kernel actually bounds the depth (submitted iso URBs);
// this is just a generous ceiling against unbounded look-ahead.
const ISO_MAX_INFLIGHT: usize = 64;

#[derive(Clone)]
struct DeviceSlot {
    busid: String,
    dev_id: String,
    busnum: u32,
    devnum: u32,
}

pub struct UsbipServer {
    server_ip: String,
    mac: [u8; 6],
    host_ip: Option<String>,
    bind: (String, u16),
    device_port: u16,
    devices: StdMutex<Vec<DeviceSlot>>,
    active: Arc<Mutex<Vec<Arc<SilexDevice>>>>,
    listener: Mutex<Option<TcpListener>>,
    usb_password: Option<Vec<u8>>,
    device_speeds: StdMutex<HashMap<String, u32>>,
}

impl UsbipServer {
    /// `devices` is a list of (busid, dev_id). busnum=1, devnum counts up from 2.
    pub fn new(
        server_ip: &str,
        mac: [u8; 6],
        host_ip: Option<String>,
        bind: (String, u16),
        device_port: u16,
        devices: Vec<(String, String)>,
    ) -> Self {
        let devices = devices
            .into_iter()
            .enumerate()
            .map(|(i, (busid, dev_id))| DeviceSlot {
                busid,
                dev_id,
                busnum: 1,
                devnum: 2 + i as u32,
            })
            .collect();
        Self {
            server_ip: server_ip.to_string(),
            mac,
            host_ip,
            bind,
            device_port,
            devices: StdMutex::new(devices),
            active: Arc::new(Mutex::new(Vec::new())),
            listener: Mutex::new(None),
            usb_password: None,
            device_speeds: StdMutex::new(HashMap::new()),
        }
    }

    pub fn with_usb_password(mut self, password: Option<Vec<u8>>) -> Self {
        self.usb_password = password;
        self
    }

    pub fn with_device_speeds(self, devices: &[crate::protocol::udp_discovery::UsbDeviceEntry]) -> Self {
        self.update_speeds(devices);
        self
    }

    fn update_speeds(&self, devices: &[crate::protocol::udp_discovery::UsbDeviceEntry]) {
        let mut speeds = self.device_speeds.lock().unwrap();
        speeds.clear();
        for device in devices {
            let speed = match device.tag.as_bytes().first() {
                Some(b'L') => 1,
                Some(b'F') => 2,
                Some(b'H') => 3,
                Some(b'S') => 5,
                _ => continue,
            };
            speeds.insert(device.dev_id.clone(), speed);
        }
    }

    pub fn update_devices(&self, devices: Vec<(String, String)>, speeds: &[crate::protocol::udp_discovery::UsbDeviceEntry]) {
        *self.devices.lock().unwrap() = devices.into_iter().map(|(busid, dev_id)| {
            let number = busid.strip_prefix("1-").and_then(|n| n.parse::<u32>().ok()).unwrap_or(1);
            DeviceSlot { busid, dev_id, busnum: 1, devnum: 1 + number }
        }).collect();
        self.update_speeds(speeds);
    }

    /// Bind the listening socket up front so callers can attach without racing.
    /// Returns the bound port.
    pub async fn bind(&self) -> Result<u16> {
        let listener = TcpListener::bind((self.bind.0.as_str(), self.bind.1)).await?;
        let port = listener.local_addr()?.port();
        *self.listener.lock().await = Some(listener);
        Ok(port)
    }

    fn make_session(&self, dev_id: &str) -> Arc<SilexSession> {
        Arc::new(SilexSession::new(
            &self.server_ip,
            self.mac,
            dev_id,
            self.host_ip.clone(),
            self.device_port,
            KEEPALIVE,
        ).with_usb_password(self.usb_password.clone()))
    }

    /// A sub-interface session of a composite device: same sel index as the primary.
    fn make_sub_session(&self, dev_id: &str, dev_index: u8) -> Arc<SilexSession> {
        Arc::new(SilexSession::with_dev_index(
            &self.server_ip,
            self.mac,
            dev_id,
            self.host_ip.clone(),
            self.device_port,
            KEEPALIVE,
            Some(dev_index),
        ).with_usb_password(self.usb_password.clone()))
    }

    fn slot(&self, busid: &str) -> Option<DeviceSlot> {
        self.devices.lock().unwrap().iter().find(|d| d.busid == busid).cloned()
    }

    /// Disconnect all active devices cleanly (against stuck reservations).
    pub async fn shutdown(&self) {
        let devices = { self.active.lock().await.clone() };
        for dev in devices {
            dev.disconnect_all().await;
        }
    }

    pub async fn serve(self: Arc<Self>) -> Result<()> {
        let listener = match self.listener.lock().await.take() {
            Some(l) => l,
            None => TcpListener::bind((self.bind.0.as_str(), self.bind.1)).await?,
        };
        let devs: Vec<String> = self.devices.lock().unwrap().iter().map(|d| format!("{}->{}", d.busid, d.dev_id)).collect();
        tracing::info!("USB/IP server on {}:{} (@ {}: {})", self.bind.0, self.bind.1, self.server_ip, devs.join(", "));
        loop {
            let (conn, addr) = listener.accept().await?;
            let me = self.clone();
            tokio::spawn(async move {
                if let Err(e) = me.handle(conn).await {
                    tracing::warn!("[{addr}] connection ended: {e}");
                }
            });
        }
    }

    async fn handle(self: Arc<Self>, mut conn: TcpStream) -> Result<()> {
        // Small interrupt/control completions (mouse and cursor updates) must
        // not wait behind Nagle's algorithm while bulk framebuffer traffic is idle.
        conn.set_nodelay(true)?;
        let mut op = [0u8; OpHeader::SIZE];
        conn.read_exact(&mut op).await?;
        let header = OpHeader::parse(&op)?;
        match header.code {
            up::OP_REQ_DEVLIST => self.handle_devlist(conn).await,
            up::OP_REQ_IMPORT => self.handle_import(conn).await,
            other => bail!("unsupported OP code {other:#06x}"),
        }
    }

    async fn read_device_info(
        &self,
        sess: &SilexSession,
        slot: &DeviceSlot,
    ) -> Result<UsbDevice> {
        let desc = sess.get_device_descriptor().await.map_err(|e| anyhow::anyhow!("{e}"))?;
        if desc.len() < 18 {
            bail!("short device descriptor");
        }
        let cfg = sess.control_in(0x80, 0x06, 0x0200, 0x0000, 9).await.map_err(|e| anyhow::anyhow!("{e}"))?;
        let bcd_usb = u16::from_le_bytes([desc[2], desc[3]]);
        Ok(UsbDevice {
            path: format!("/silex/{}", slot.dev_id),
            busid: slot.busid.clone(),
            busnum: slot.busnum,
            devnum: slot.devnum,
            speed: self.device_speeds.lock().unwrap().get(&slot.dev_id).copied().unwrap_or_else(|| speed_from_bcd(bcd_usb)),
            id_vendor: u16::from_le_bytes([desc[8], desc[9]]),
            id_product: u16::from_le_bytes([desc[10], desc[11]]),
            bcd_device: u16::from_le_bytes([desc[12], desc[13]]),
            b_device_class: desc[4],
            b_device_subclass: desc[5],
            b_device_protocol: desc[6],
            b_configuration_value: if cfg.len() > 5 { cfg[5] } else { 1 },
            b_num_configurations: desc[17],
            b_num_interfaces: if cfg.len() > 4 { cfg[4] } else { 1 },
        })
    }

    async fn probe_device(&self, slot: &DeviceSlot) -> Result<UsbDevice> {
        let sess = self.make_session(&slot.dev_id);
        sess.connect().await?;
        let info = self.read_device_info(&sess, slot).await;
        sess.disconnect().await;
        info
    }

    /// Read the full config descriptor and build the composite router (one session per
    /// sub-interface). A simple device yields a single-session router.
    async fn build_device(self: &Arc<Self>, primary: Arc<SilexSession>, slot: &DeviceSlot) -> Result<SilexDevice> {
        let full_cfg = primary.get_config_descriptor().await.map_err(|e| anyhow::anyhow!("{e}"))?;
        let me = self.clone();
        let dev_index = primary.dev_index;
        let make_sub = move |did: &str| me.make_sub_session(did, dev_index);
        SilexDevice::build(primary, &full_cfg, &slot.dev_id, make_sub).await
    }

    async fn handle_devlist(self: Arc<Self>, mut conn: TcpStream) -> Result<()> {
        let mut entries = Vec::new();
        let slots = self.devices.lock().unwrap().clone();
        for slot in &slots {
            let dev = self.probe_device(slot).await?;
            let ifaces = vec![UsbInterface {
                b_interface_class: if dev.b_device_class != 0 { dev.b_device_class } else { 8 },
                b_interface_subclass: if dev.b_device_subclass != 0 { dev.b_device_subclass } else { 6 },
                b_interface_protocol: 80,
            }];
            entries.push((dev, ifaces));
        }
        conn.write_all(&up::build_rep_devlist(&entries)).await?;
        Ok(())
    }

    async fn handle_import(self: Arc<Self>, mut conn: TcpStream) -> Result<()> {
        let mut busid_buf = [0u8; 32];
        conn.read_exact(&mut busid_buf).await?;
        let busid = crate::wire::cstr_field(&busid_buf, 0, Some(32));
        let slot = match self.slot(&busid) {
            Some(s) => s.clone(),
            None => {
                conn.write_all(&up::build_rep_import(None)).await?;
                return Ok(());
            }
        };
        // One session for probe AND the URB loop - no disconnect/reconnect, otherwise the
        // device rejects the immediate reconnect ("NG"). For composite devices,
        // SilexDevice::build connects an extra session per sub-interface.
        let sess = self.make_session(&slot.dev_id);
        let dev = match async {
            sess.connect().await?;
            self.read_device_info(&sess, &slot).await
        }
        .await
        {
            Ok(dev) => dev,
            Err(e) => {
                tracing::warn!("import failed (connect/probe): {e}");
                sess.disconnect().await;
                conn.write_all(&up::build_rep_import(None)).await?;
                return Ok(());
            }
        };
        let device = match self.build_device(sess.clone(), &slot).await {
            Ok(d) => Arc::new(d),
            Err(e) => {
                tracing::warn!("import failed (composite build): {e}");
                sess.disconnect().await;
                conn.write_all(&up::build_rep_import(None)).await?;
                return Ok(());
            }
        };
        self.active.lock().await.push(device.clone());
        // Start keepalive on every connection; the pump stops it for iso/mux connections
        // (which keepalive themselves) and leaves it running on plain connections.
        for s in device.sessions() {
            s.start_keepalive();
        }
        let result = async {
            conn.write_all(&up::build_rep_import(Some(&dev))).await?;
            urb_pump(conn, device.clone()).await
        }
        .await;
        {
            let mut act = self.active.lock().await;
            act.retain(|d| !Arc::ptr_eq(d, &device));
        }
        device.disconnect_all().await;
        result
    }
}

fn speed_from_bcd(bcd_usb: u16) -> u32 {
    // usb_device_speed: 2=full, 3=high
    if bcd_usb >= 0x0200 {
        3
    } else {
        2
    }
}

/// A request in flight on a mux connection: a real URB, or a keepalive (discarded).
enum Inflight {
    Cmd(UsbipCmdSubmit),
    Keepalive,
}

/// Shared state of one imported device's URB pump.
enum WorkerCommand {
    Submit(UsbipCmdSubmit),
    Cancel(u32),
}

struct Pump {
    wr: Mutex<OwnedWriteHalf>, // USB/IP socket write half (send RET_SUBMIT/RET_UNLINK)
    cancelled: StdMutex<HashSet<u32>>,
    routes: StdMutex<HashMap<u32, Arc<SilexSession>>>,
    alive: AtomicBool,
    shutdown: Notify, // breaks the reader when a worker hits a fatal tunnel error
}

impl Pump {
    fn close(&self) {
        self.alive.store(false, Ordering::SeqCst);
        self.shutdown.notify_one();
    }
    fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    async fn send(&self, data: &[u8]) -> std::io::Result<()> {
        self.wr.lock().await.write_all(data).await
    }

    /// True when the URB should be processed (not cancelled meanwhile).
    fn take(&self, seqnum: u32) -> bool {
        let cancelled = self.cancelled.lock().unwrap().remove(&seqnum);
        if cancelled { self.routes.lock().unwrap().remove(&seqnum); }
        !cancelled
    }

    /// Send RET_SUBMIT (with a cancel re-check). false => end the connection/worker.
    async fn complete(&self, ret: &UsbipRetSubmit) -> bool {
        self.routes.lock().unwrap().remove(&ret.seqnum);
        if self.cancelled.lock().unwrap().remove(&ret.seqnum) {
            return true;
        }
        if self.send(&ret.to_bytes()).await.is_err() {
            self.close();
            return false;
        }
        true
    }

    async fn abort_tunnel(&self, cmd: &UsbipCmdSubmit, e: impl std::fmt::Display) {
        tracing::warn!(
            "URB seq={} tunnel error: {e} [ep={:#04x} {} iso={}]",
            cmd.seqnum, cmd.ep, if cmd.is_in() { "IN" } else { "OUT" }, cmd.is_iso()
        );
        let ret = UsbipRetSubmit {
            seqnum: cmd.seqnum,
            devid: cmd.devid,
            direction: cmd.direction,
            ep: cmd.ep,
            status: EPIPE,
            ..Default::default()
        };
        let _ = self.send(&ret.to_bytes()).await;
        self.close();
    }
}

fn contains(set: &[Arc<SilexSession>], s: &Arc<SilexSession>) -> bool {
    set.iter().any(|x| Arc::ptr_eq(x, s))
}

async fn urb_pump(conn: TcpStream, device: Arc<SilexDevice>) -> Result<()> {
    let (rd, wr) = conn.into_split();
    let pump = Arc::new(Pump {
        wr: Mutex::new(wr),
        cancelled: StdMutex::new(HashSet::new()),
        routes: StdMutex::new(HashMap::new()),
        alive: AtomicBool::new(true),
        shutdown: Notify::new(),
    });

    let sessions = device.sessions();
    // DS-600 supports sequence-based cancellation for iso too. A FIFO reader
    // blocks cancellation at stream stop and eventually times out, taking the
    // other functions of a composite dock down with it.
    let iso: Vec<_> = device.iso_sessions().into_iter()
        .filter(|session| !session.dev_id.starts_with("usb")).collect();
    let mux: Vec<Arc<SilexSession>> = device
        .multiplex_sessions()
        .into_iter()
        .filter(|s| !contains(&iso, s))
        .collect();

    // Iso and mux connections are served by their own task(s) with their own keepalive ->
    // stop the auto-keepalive so there is only one reader per socket.
    for s in iso.iter().chain(mux.iter()) {
        s.stop_keepalive();
    }

    let mut senders: Vec<(Arc<SilexSession>, mpsc::Sender<WorkerCommand>)> = Vec::new();
    let mut tasks = Vec::new();
    for s in &sessions {
        let (tx, rx) = mpsc::channel::<WorkerCommand>(64);
        senders.push((s.clone(), tx));
        if contains(&iso, s) {
            tasks.push(tokio::spawn(iso_worker(pump.clone(), s.clone(), rx)));
        } else if contains(&mux, s) {
            let inflight = Arc::new(StdMutex::new(HashMap::new()));
            tasks.push(tokio::spawn(mux_sender(pump.clone(), s.clone(), rx, inflight.clone(), device.clone())));
            tasks.push(tokio::spawn(mux_reader(pump.clone(), s.clone(), inflight.clone())));
        } else {
            tasks.push(tokio::spawn(plain_worker(pump.clone(), s.clone(), rx)));
        }
    }

    reader_loop(rd, pump.clone(), device.clone(), senders).await;
    // `senders` was moved into reader_loop and dropped there -> queue workers see their
    // channel close and exit. Abort any task still blocked on device I/O (mux long-poll)
    // and await it, so it releases the session read/write locks before disconnect_all.
    pump.alive.store(false, Ordering::SeqCst);
    for t in tasks {
        t.abort();
        let _ = t.await;
    }
    Ok(())
}

async fn reader_loop(
    mut rd: OwnedReadHalf,
    pump: Arc<Pump>,
    device: Arc<SilexDevice>,
    senders: Vec<(Arc<SilexSession>, mpsc::Sender<WorkerCommand>)>,
) {
    loop {
        let mut head = [0u8; UsbipCmdSubmit::HEADER_SIZE];
        tokio::select! {
            res = rd.read_exact(&mut head) => {
                if let Err(error) = res {
                    tracing::debug!(%error, "USB/IP request stream ended");
                    break;
                }
            }
            _ = pump.shutdown.notified() => break,
        }
        let command = match up::peek_command(&head) {
            Ok(c) => c,
            Err(error) => { tracing::warn!(%error, "invalid USB/IP command header"); break; },
        };
        if command == up::USBIP_CMD_SUBMIT {
            let mut cmd = match UsbipCmdSubmit::parse(&head, Vec::new()) {
                Ok(c) => c,
                Err(error) => { tracing::warn!(%error, "invalid USB/IP submit"); break; },
            };
            if cmd.transfer_buffer_length > 16 * 1024 * 1024 || (cmd.number_of_packets != u32::MAX && cmd.number_of_packets > 1024) {
                tracing::warn!("USB/IP transfer limit exceeded");
                break;
            }
            if !cmd.is_in() && cmd.transfer_buffer_length > 0 {
                let mut body = vec![0u8; cmd.transfer_buffer_length as usize];
                if rd.read_exact(&mut body).await.is_err() {
                    break;
                }
                cmd.transfer_buffer = body;
            }
            if cmd.is_iso() {
                // On ISO, number_of_packets descriptors (16 B each) follow the
                // transfer_buffer - otherwise the URB stream desyncs.
                let mut descs = vec![0u8; up::ISO_DESC_SIZE * cmd.number_of_packets as usize];
                if rd.read_exact(&mut descs).await.is_err() {
                    break;
                }
                if cmd.set_iso_from(&descs).is_err() {
                    break;
                }
            }
            let target = device.session_for(&cmd);
            tracing::debug!(seq = cmd.seqnum, ep = cmd.ep, direction = cmd.direction,
                length = cmd.transfer_buffer_length, "USB/IP submit");
            if let Some((_s, tx)) = senders.iter().find(|(s, _)| Arc::ptr_eq(s, &target)) {
                pump.routes.lock().unwrap().insert(cmd.seqnum, target.clone());
                if tx.send(WorkerCommand::Submit(cmd)).await.is_err() {
                    break;
                }
            }
        } else if command == up::USBIP_CMD_UNLINK {
            let unlink = match UsbipCmdUnlink::parse(&head) {
                Ok(u) => u,
                Err(_) => break,
            };
            pump.cancelled.lock().unwrap().insert(unlink.unlink_seqnum);
            let route = pump.routes.lock().unwrap().get(&unlink.unlink_seqnum).cloned();
            if let Some(target) = route {
                if let Some((_, tx)) = senders.iter().find(|(s, _)| Arc::ptr_eq(s, &target)) {
                    if tx.send(WorkerCommand::Cancel(unlink.unlink_seqnum)).await.is_err() { break; }
                }
            }
            let bytes = UsbipRetUnlink { seqnum: unlink.seqnum, status: 0 }.to_bytes();
            if pump.send(&bytes).await.is_err() {
                break;
            }
        } else {
            tracing::warn!(command, "unknown USB/IP command");
            break;
        }
    }
}

/// Synchronous worker for a plain bulk/control connection.
async fn plain_worker(pump: Arc<Pump>, sess: Arc<SilexSession>, mut rx: mpsc::Receiver<WorkerCommand>) {
    while let Some(command) = rx.recv().await {
        let WorkerCommand::Submit(cmd) = command else { continue };
        if !pump.take(cmd.seqnum) {
            continue;
        }
        match submit_to_silex(&sess, &cmd).await {
            Ok(ret) => {
                if !pump.complete(&ret).await {
                    return;
                }
            }
            Err(e) => {
                pump.abort_tunnel(&cmd, e).await;
                return;
            }
        }
    }
}

/// Iso connection like SX Virtual Link: send many requests continuously ahead and read the
/// responses trailing (a deep pipeline buffer against jitter). A single task owns the
/// socket; while idle it keepalives itself. Responses arrive FIFO -> in-flight is a deque.
async fn iso_worker(pump: Arc<Pump>, sess: Arc<SilexSession>, mut rx: mpsc::Receiver<WorkerCommand>) {
    let mut inflight: VecDeque<UsbipCmdSubmit> = VecDeque::new();
    let ka = if sess.keepalive_interval.is_zero() { KEEPALIVE } else { sess.keepalive_interval };
    // Load iso-OUT formats ONCE (a control transfer) while nothing is in flight -> controls
    // which OUT endpoints (16-bit audio only) may be compressed.
    if !sess.iso_out_format_loaded() {
        sess.load_iso_out_format().await;
    }
    while pump.is_alive() {
        // 1) send ahead: all immediately available URBs (up to the pipeline ceiling).
        while inflight.len() < ISO_MAX_INFLIGHT {
            match rx.try_recv() {
                Ok(WorkerCommand::Submit(cmd)) => {
                    if !iso_enqueue(&pump, &sess, cmd, &mut inflight).await {
                        return;
                    }
                }
                Ok(WorkerCommand::Cancel(_)) => continue,
                Err(_) => break, // empty or disconnected
            }
        }
        // 2) fetch one response and complete the oldest URB.
        if !inflight.is_empty() {
            match sess.iso_recv().await {
                Ok((status1, packets, data)) => {
                    let cmd = inflight.pop_front().unwrap();
                    let ret = if cmd.is_in() {
                        iso_fill_ret(&cmd, ret_for(&cmd), status1, &packets, data)
                    } else {
                        iso_fill_ret_out(&cmd, ret_for(&cmd), status1)
                    };
                    if !pump.complete(&ret).await {
                        return;
                    }
                }
                Err(e) => {
                    let cmd = inflight[0].clone();
                    pump.abort_tunnel(&cmd, e).await;
                    return;
                }
            }
        } else {
            // Pipeline empty: block for the next URB; on idle (timeout) keepalive ourselves.
            match timeout(ka, rx.recv()).await {
                Ok(Some(WorkerCommand::Submit(cmd))) => {
                    if !iso_enqueue(&pump, &sess, cmd, &mut inflight).await {
                        return;
                    }
                }
                Ok(Some(WorkerCommand::Cancel(_))) => continue,
                Ok(None) => return, // channel closed
                Err(_) => {
                    if sess.get_device_descriptor().await.is_err() {
                        return;
                    }
                }
            }
        }
    }
}

/// Send a URB and add it to `inflight`. false => end the worker/connection.
async fn iso_enqueue(pump: &Pump, sess: &SilexSession, cmd: UsbipCmdSubmit, inflight: &mut VecDeque<UsbipCmdSubmit>) -> bool {
    if !pump.take(cmd.seqnum) {
        return true; // cancelled -> skip
    }
    let send = if cmd.is_in() {
        let (ep, packets, sf, mp, tl) = iso_request_params(sess, &cmd);
        sess.iso_send(ep, packets, sf, mp, tl).await
    } else {
        let (ep, packets, audio, sf, mp, nch) = iso_out_request_params(sess, &cmd);
        sess.iso_send_out(ep, packets, audio, sf, mp, nch).await
    };
    if let Err(e) = send {
        pump.abort_tunnel(&cmd, e).await;
        return false;
    }
    inflight.push_back(cmd);
    true
}

fn ep_info(device: &SilexDevice, cmd: &UsbipCmdSubmit) -> (Option<u8>, u16, u16) {
    let addr = ((cmd.ep as u8) & 0x7F) | if cmd.is_in() { 0x80 } else { 0x00 };
    let (mps, interval) = device.interrupt_params(addr);
    (device.ep_type_for(addr), mps, interval)
}

type MuxInflight = Arc<StdMutex<HashMap<u16, Inflight>>>;

/// Mux sender: take URBs (control/interrupt/bulk/DS-600 iso) off the queue and send them into the
/// tunnel without waiting for the response (the reader dispatches by seq). Interrupt-IN is
/// held open (long-poll) -> must not block control. While idle this sender keepalives.
async fn mux_sender(pump: Arc<Pump>, sess: Arc<SilexSession>, mut rx: mpsc::Receiver<WorkerCommand>, inflight: MuxInflight, device: Arc<SilexDevice>) {
    let ka = if sess.keepalive_interval.is_zero() { KEEPALIVE } else { sess.keepalive_interval };
    while pump.is_alive() {
        match timeout(ka, rx.recv()).await {
            Err(_) => {
                if !mux_keepalive(&pump, &sess, &inflight).await {
                    return;
                }
            }
            Ok(None) => return, // channel closed
            Ok(Some(WorkerCommand::Cancel(usbip_seq))) => {
                // Submit and Cancel share this queue, so cancel cannot overtake
                // the original request on the wire.
                let removed = {
                    let mut waiting = inflight.lock().unwrap();
                    let seq = waiting.iter().find_map(|(seq, entry)| match entry {
                        Inflight::Cmd(cmd) if cmd.seqnum == usbip_seq => Some(*seq),
                        _ => None,
                    });
                    seq.and_then(|seq| waiting.remove(&seq).map(|entry| (seq, entry)))
                };
                if let Some((seq, Inflight::Cmd(cmd))) = removed {
                    let (kind, mps, interval) = ep_info(&device, &cmd);
                    let mut frame = build_request(&sess, &cmd, kind, mps, interval);
                    frame.seq = seq;
                    if let Err(error) = sess.cancel_request(frame).await {
                        pump.abort_tunnel(&cmd, error).await;
                        return;
                    }
                }
                pump.routes.lock().unwrap().remove(&usbip_seq);
                pump.cancelled.lock().unwrap().remove(&usbip_seq);
            }
            Ok(Some(WorkerCommand::Submit(cmd))) => {
                if !pump.take(cmd.seqnum) {
                    continue;
                }
                let (ep_type, mps, interval) = ep_info(&device, &cmd);
                let frame = build_request(&sess, &cmd, ep_type, mps, interval);
                let seq = frame.seq;
                inflight.lock().unwrap().insert(seq, Inflight::Cmd(cmd.clone()));
                if let Err(e) = sess.send_request(frame).await {
                    pump.abort_tunnel(&cmd, e).await;
                    return;
                }
            }
        }
    }
}

/// Harmless GET_DESCRIPTOR keepalive; its response is discarded by seq. false => dead.
async fn mux_keepalive(pump: &Pump, sess: &SilexSession, inflight: &StdMutex<HashMap<u16, Inflight>>) -> bool {
    let frame = sess.build_control_frame(0x80, 0x06, 0x0100, 0x0000, 0x12, Vec::new());
    let seq = frame.seq;
    inflight.lock().unwrap().insert(seq, Inflight::Keepalive);
    if sess.send_request(frame).await.is_err() {
        pump.close();
        return false;
    }
    true
}

/// Mux reader: read responses and dispatch each to the waiting URB by seq (responses are
/// NOT FIFO: a control answer can arrive before a still-open interrupt answer).
async fn mux_reader(pump: Arc<Pump>, sess: Arc<SilexSession>, inflight: MuxInflight) {
    while pump.is_alive() {
        let resp = match sess.recv_response().await {
            Ok(r) => r,
            Err(_) => {
                pump.close();
                return;
            }
        };
        let entry = inflight.lock().unwrap().remove(&resp.seq);
        match entry {
            Some(Inflight::Cmd(cmd)) => {
                if !pump.complete(&fill_ret_from_response(&cmd, &resp)).await {
                    return;
                }
            }
            _ => continue, // keepalive / unknown -> discard
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn stream_pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).await.unwrap();
        let (server, _) = listener.accept().await.unwrap();
        (client, server)
    }

    fn bulk_submit(seq: u32, direction: u32, ep: u32) -> Vec<u8> {
        let mut head = vec![0; 48];
        for (offset, value) in [(0, up::USBIP_CMD_SUBMIT), (4, seq), (12, direction), (16, ep), (24, 1)] {
            head[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
        }
        if direction == up::USBIP_DIR_OUT { head.push(0xAA); }
        head
    }

    fn iso_input_submit(seq: u32) -> Vec<u8> {
        let mut bytes = vec![0; 64];
        for (offset, value) in [(0, up::USBIP_CMD_SUBMIT), (4, seq),
            (12, up::USBIP_DIR_IN), (16, 1), (24, 4), (32, 1), (36, 8), (52, 4)] {
            bytes[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
        }
        bytes
    }

    #[tokio::test]
    async fn ds600_iso_cancel_does_not_wait_for_capture_and_next_stream_still_works() {
        timeout(Duration::from_secs(2), async {
            let (tunnel, mut hardware) = stream_pair().await;
            let session = Arc::new(SilexSession::new("127.0.0.1", [0; 6], "usb5-005:0", None, 19540, Duration::ZERO));
            session.use_test_stream(tunnel).await;
            let config = [9, 4, 0, 1, 1, 1, 2, 0x20, 0, 7, 5, 0x81, 5, 4, 0, 4];
            let device = Arc::new(SilexDevice::build(session, &config, "usb5-005:0", |_| panic!("only one interface")).await.unwrap());
            let (mut client, server) = stream_pair().await;
            let pump = tokio::spawn(urb_pump(server, device));
            client.write_all(&iso_input_submit(41)).await.unwrap();
            let mut request = [0; 36];
            hardware.read_exact(&mut request).await.unwrap();
            assert_eq!(&request[..2], &[0, 4]);
            assert_eq!(&request[12..16], &[0, 4, 0, 0]);
            // Capture is pending forever: the sender must still process UNLINK.
            let mut unlink = [0; 48];
            unlink[..4].copy_from_slice(&up::USBIP_CMD_UNLINK.to_be_bytes());
            unlink[4..8].copy_from_slice(&42u32.to_be_bytes());
            unlink[20..24].copy_from_slice(&41u32.to_be_bytes());
            client.write_all(&unlink).await.unwrap();
            let mut cancel = [0; 24];
            hardware.read_exact(&mut cancel).await.unwrap();
            assert_eq!(&cancel[..2], &[0x40, 4]);
            assert_eq!(&cancel[2..10], &request[2..10]);
            let mut ret = [0; 48];
            client.read_exact(&mut ret).await.unwrap();
            assert_eq!(&ret[..4], &up::USBIP_RET_UNLINK.to_be_bytes());
            // Restart capture; no completion for the cancelled request is sent.
            client.write_all(&iso_input_submit(43)).await.unwrap();
            hardware.read_exact(&mut request).await.unwrap();
            let payload = [0, 1, 0, 0, 0, 0, 0, 0, 0, 4, 0, 0, 0, 0, 1, 2, 3, 4];
            let mut reply = request[..4].to_vec();
            reply.extend_from_slice(&[0, 0]);
            reply.extend_from_slice(&(payload.len() as u32).to_be_bytes());
            reply.extend_from_slice(&payload);
            hardware.write_all(&reply).await.unwrap();
            client.read_exact(&mut ret).await.unwrap();
            assert_eq!(&ret[4..8], &43u32.to_be_bytes());
            assert_eq!(&ret[24..28], &4u32.to_be_bytes());
            assert_eq!(&ret[32..36], &1u32.to_be_bytes());
            let mut data = [0; 20];
            client.read_exact(&mut data).await.unwrap();
            assert_eq!(&data[..4], &[1, 2, 3, 4]);
            assert_eq!(&data[12..16], &4u32.to_be_bytes());
            // A real tunnel EOF must wake the USB/IP reader, even with no commands.
            drop(hardware);
            pump.await.unwrap().unwrap();
        }).await.expect("iso cancellation blocked behind a pending capture");
    }

    #[tokio::test]
    async fn bulk_read_does_not_block_write_on_interface_without_interrupt_endpoint() {
        timeout(Duration::from_secs(2), async {
            let (tunnel, mut hardware) = stream_pair().await;
            let session = Arc::new(SilexSession::new("127.0.0.1", [0; 6], "usb5-005:0", None, 19540, Duration::ZERO));
            session.use_test_stream(tunnel).await;
            let config = [
                9, 4, 0, 0, 2, 0xFF, 0, 3, 0,
                7, 5, 0x84, 2, 0, 4, 0,
                7, 5, 0x02, 2, 0, 4, 0,
            ];
            let device = Arc::new(SilexDevice::build(session, &config, "usb5-005:0", |_| panic!("only one interface")).await.unwrap());
            let (mut client, server) = stream_pair().await;
            let pump = tokio::spawn(urb_pump(server, device));
            client.write_all(&bulk_submit(41, up::USBIP_DIR_IN, 4)).await.unwrap();
            client.write_all(&bulk_submit(42, up::USBIP_DIR_OUT, 2)).await.unwrap();
            // The hardware waits for the command before answering the earlier read.
            let mut read = [0; 18];
            hardware.read_exact(&mut read).await.unwrap();
            assert_eq!(&read[..2], &[0, 3]);
            let mut write = [0; 19];
            hardware.read_exact(&mut write).await.unwrap();
            assert_eq!(&write[..2], &[0x80, 3]);
            assert_eq!(write[18], 0xAA);
            for (request, payload) in [(&write[..], &[][..]), (&read[..], &[0xBB][..])] {
                let mut response = vec![request[0], request[1], request[2], request[3], 0, 0];
                response.extend_from_slice(&(payload.len() as u32).to_be_bytes());
                response.extend_from_slice(payload);
                hardware.write_all(&response).await.unwrap();
            }
            let mut ret = [0; 48];
            client.read_exact(&mut ret).await.unwrap();
            assert_eq!(&ret[4..8], &42u32.to_be_bytes());
            assert_eq!(&ret[20..24], &[0; 4]);
            client.read_exact(&mut ret).await.unwrap();
            assert_eq!(&ret[4..8], &41u32.to_be_bytes());
            assert_eq!(&ret[20..24], &[0; 4]);
            assert_eq!(client.read_u8().await.unwrap(), 0xBB);
            drop(client);
            pump.await.unwrap().unwrap();
        }).await.expect("bulk IN blocked a later OUT request");
    }
}
