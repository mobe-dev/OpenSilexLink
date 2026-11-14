//! Silex TCP tunnel session to a USB device on the device server.
//!
//! Flow (from the capture):
//!   1. UDP CONNECT, phase QUERY  -> device answers "OK"
//!   2. UDP CONNECT, phase COMMIT -> device answers ACK
//!   3. TCP connection to <server>:19540 -> the USB tunnel (see silex_frame)
//!   Disconnect: UDP DISCONNECT QUERY + COMMIT, close TCP.
//!
//! The device index (high byte of the sel word in every tunnel request) selects the
//! target device (dev01->0x03, dev02->0x04, devNN->NN+2). Without the correct index
//! all transfers go to the first device.
//!
//! Concurrency: the TCP stream is split into a read half and a write half, each behind
//! its own mutex. Plain request/response transfers (control/bulk/iso batch) serialize
//! through `io_lock`. The multiplex path (control + interrupt on one connection) instead
//! sends on the write half and reads on the read half concurrently, matching responses
//! to requests by their sequence number.

use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use anyhow::{bail, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio::time::timeout;

use crate::protocol::silex_frame::{SilexFrame, REQUEST_TAG};
use crate::protocol::silex_iso_codec::compress_iso_out;
use crate::protocol::udp_discovery::{self as ud, Message, Phase};

/// Iso request as passed to [`SilexSession::iso_batch`]:
/// (endpoint, packets[(offset, length)], start_frame, maxpacket, total_length).
pub type IsoInReq = (u8, Vec<(u32, u32)>, u32, u16, u32);
/// Iso response: (silex status1, resp_packets[(offset, actual_length, status)], data).
pub type IsoResp = (u16, Vec<(u32, u32, u32)>, Vec<u8>);

/// Error from a single tunnel transfer.
#[derive(Debug)]
pub enum TransferError {
    /// The read timed out; the URB fails but the session stays alive.
    Timeout,
    /// The tunnel was closed / an I/O error occurred; the session is dead.
    Closed,
    Other(anyhow::Error),
}

impl std::fmt::Display for TransferError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TransferError::Timeout => write!(f, "tunnel transfer timed out"),
            TransferError::Closed => write!(f, "tunnel closed"),
            TransferError::Other(e) => write!(f, "tunnel error: {e}"),
        }
    }
}
impl std::error::Error for TransferError {}

use crate::discovery::local_ip_towards;

pub struct SilexSession {
    pub server_ip: String,
    pub port: u16,
    pub mac: [u8; 6],
    pub dev_id: String,
    pub host_ip: String,
    pub dev_index: u8,
    timeout: Duration,
    pub keepalive_interval: Duration,
    read: Mutex<Option<OwnedReadHalf>>,
    write: Mutex<Option<OwnedWriteHalf>>,
    seq: StdMutex<u16>,
    io_lock: Mutex<()>, // serializes whole request/response transfers
    // Iso frame counter (never 0): SX Virtual Link derives start_frame from the system
    // clock; we seed it clock-based (large, realtime-ish) and count up per 1 ms frame.
    iso_frame: StdMutex<u32>,
    // Iso-OUT format map: endpoint(0x0N) -> (wMaxPacketSize, nch, subframe_bytes). None =
    // not loaded yet. Only Audio/AudioStreaming endpoints are listed; controls whether an
    // OUT iso endpoint may be compressed.
    iso_out_format: StdMutex<Option<HashMap<u8, (u16, u8, u8)>>>,
    endpoint_params: StdMutex<HashMap<u8, (u16, u16)>>,
    ka: StdMutex<Option<JoinHandle<()>>>,
    usb_password: Option<Vec<u8>>,
    partial_bulk: StdMutex<HashMap<(u16, u16), Vec<u8>>>,
}

impl SilexSession {
    #[cfg(test)]
    pub async fn use_test_stream(&self, stream: TcpStream) {
        let (read, write) = stream.into_split();
        *self.read.lock().await = Some(read);
        *self.write.lock().await = Some(write);
    }
    pub fn new(
        server_ip: &str,
        mac: [u8; 6],
        dev_id: &str,
        host_ip: Option<String>,
        port: u16,
        keepalive_interval: Duration,
    ) -> Self {
        Self::with_dev_index(server_ip, mac, dev_id, host_ip, port, keepalive_interval, None)
    }

    /// Like [`new`], but forces the tunnel device index (sub-interface sessions of a
    /// composite device all use the primary's index).
    pub fn with_dev_index(
        server_ip: &str,
        mac: [u8; 6],
        dev_id: &str,
        host_ip: Option<String>,
        port: u16,
        keepalive_interval: Duration,
        dev_index: Option<u8>,
    ) -> Self {
        let host_ip = host_ip
            .or_else(|| local_ip_towards(server_ip))
            .unwrap_or_default();
        let dev_index =
            dev_index.unwrap_or_else(|| ud::UsbDeviceEntry::new(dev_id, "", 0, 0, "").dev_index());
        let seed = (now_millis() & 0x3FFF_FFFF).max(1);
        Self {
            server_ip: server_ip.to_string(),
            port,
            mac,
            dev_id: dev_id.to_string(),
            host_ip,
            dev_index,
            timeout: Duration::from_secs(3),
            keepalive_interval,
            read: Mutex::new(None),
            write: Mutex::new(None),
            seq: StdMutex::new(0),
            io_lock: Mutex::new(()),
            iso_frame: StdMutex::new(seed),
            iso_out_format: StdMutex::new(None),
            endpoint_params: StdMutex::new(HashMap::new()),
            ka: StdMutex::new(None),
            usb_password: None,
            partial_bulk: StdMutex::new(HashMap::new()),
        }
    }

    pub fn with_usb_password(mut self, password: Option<Vec<u8>>) -> Self {
        self.usb_password = password;
        self
    }

    // -- UDP handshake ----------------------------------------------------- //
    async fn udp_transaction(&self, query: &[u8]) -> Result<Message> {
        let sock = UdpSocket::bind("0.0.0.0:0").await?;
        sock.connect((self.server_ip.as_str(), self.port)).await?;
        sock.send(query).await?;
        let mut buf = [0u8; 4096];
        let n = timeout(self.timeout, sock.recv(&mut buf))
            .await
            .map_err(|_| anyhow::anyhow!("UDP timeout"))??;
        crate::wire::need(&buf[..n], 8, "control response")?;
        if buf[..4] != [0, 1, 0, 0] {
            bail!("unexpected control response header");
        }
        let declared = crate::wire::rd_u16(&buf, 6) as usize;
        if crate::wire::rd_u16(&buf, 4) != 0 || declared != n - 8 {
            bail!("invalid control response offset or length");
        }
        let response = ud::parse(&buf[..n])?;
        // The same socket/source port must span query and commit. Commit even
        // DENY/NG so the next transaction does not hit a retained response.
        sock.send(&ud::build_commit(query, declared)?).await?;
        let n = timeout(self.timeout, sock.recv(&mut buf))
            .await
            .map_err(|_| anyhow::anyhow!("UDP commit timeout"))??;
        if buf[..n] != [0, 3, 0, 0, 0, 0, 0, 0] {
            bail!("unexpected control commit acknowledgement");
        }
        Ok(response)
    }

    pub async fn connect(&self) -> Result<()> {
        self.connect_with(8, Duration::from_millis(500)).await
    }

    pub async fn connect_with(&self, retries: u32, retry_delay: Duration) -> Result<()> {
        // The device can answer "NG" while it still holds a stale reservation (e.g.
        // after the server closed a previous tunnel itself). Actively release the
        // reservation before each retry.
        let mut ok = false;
        let mut last = String::new();
        for attempt in 0..=retries {
            let query = match &self.usb_password {
                Some(password) => ud::build_password_connect(&self.mac, &self.host_ip, &self.dev_id, password)?,
                None => ud::build_connect(&self.mac, &self.host_ip, &self.dev_id, Phase::Query, &ud::CONNECT_TOKEN, None)?,
            };
            match self.udp_transaction(&query).await {
                Ok(Message::Simple(r)) if r.text.starts_with("OK") => {
                    ok = true;
                    break;
                }
                Ok(Message::Simple(r)) if r.text.starts_with("DENY") => {
                    bail!("CONNECT denied: check the DS-600 USB port connection password and device-server settings");
                }
                Ok(other) => last = format!("{other:?}"),
                Err(e) => last = e.to_string(),
            }
            if attempt < retries {
                self.udp_release().await;
                tokio::time::sleep(retry_delay).await;
            }
        }
        if !ok {
            bail!("CONNECT query rejected: {last}");
        }
        let tcp_result = timeout(
            self.timeout,
            async {
                loop {
                    match TcpStream::connect((self.server_ip.as_str(), self.port)).await {
                        Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
                            // DS-600 starts its listener asynchronously after OK.
                            tokio::time::sleep(Duration::from_millis(50)).await;
                        }
                        result => break result,
                    }
                }
            },
        )
        .await;
        let stream = match tcp_result {
            Ok(Ok(stream)) => stream,
            failed => {
                // The UDP commit has already reserved the device. A failed TCP
                // connection must release it, including failures during probe.
                self.udp_release().await;
                return match failed {
                    Ok(Err(e)) => Err(e.into()),
                    _ => Err(anyhow::anyhow!("TCP connect timeout")),
                };
            }
        };
        // Iso audio is a small request/response stream with ~1000 rounds/s. Without
        // TCP_NODELAY Nagle delays small requests and throughput collapses.
        stream.set_nodelay(true).ok();
        let (rd, wr) = stream.into_split();
        *self.read.lock().await = Some(rd);
        *self.write.lock().await = Some(wr);
        *self.seq.lock().unwrap() = 0;
        self.partial_bulk.lock().unwrap().clear();
        Ok(())
    }

    // -- keepalive: keeps the reservation/session alive while idle --------- //
    pub fn start_keepalive(self: &Arc<Self>) {
        if self.keepalive_interval.is_zero() {
            return;
        }
        let s = self.clone();
        let handle = tokio::spawn(async move {
            // The device has an idle timeout on the TCP tunnel (UDP polls do not
            // count). Generate real tunnel traffic periodically: a harmless
            // GET_DESCRIPTOR (result discarded).
            loop {
                tokio::time::sleep(s.keepalive_interval).await;
                if s.get_device_descriptor().await.is_err() {
                    return;
                }
            }
        });
        *self.ka.lock().unwrap() = Some(handle);
    }

    /// Stop the auto-keepalive task (the iso/mux workers keepalive themselves).
    pub fn stop_keepalive(&self) {
        if let Some(h) = self.ka.lock().unwrap().take() {
            h.abort();
        }
    }

    /// Release the device reservation (DISCONNECT QUERY+COMMIT, UDP only).
    pub async fn udp_release(&self) {
        if let Ok(pkt) = ud::build_disconnect(&self.mac, &self.host_ip, &self.dev_id, Phase::Query, None) {
            let _ = self.udp_transaction(&pkt).await;
        }
    }

    pub async fn disconnect(&self) {
        self.stop_keepalive();
        {
            *self.read.lock().await = None; // dropping closes the TCP connection
            *self.write.lock().await = None;
        }
        self.udp_release().await;
    }

    // -- sequence / iso frame counters ------------------------------------- //
    fn next_seq(&self) -> u16 {
        let mut s = self.seq.lock().unwrap();
        let cur = *s;
        *s = s.wrapping_add(1);
        cur
    }

    /// Reserve `n` consecutive iso frame numbers, return the first. The device rejects
    /// start_frame=0 and expects monotonically rising frames; Linux only reports 0 on
    /// URB_ISO_ASAP, so we count ourselves.
    pub fn alloc_iso_frames(&self, n: u32) -> u32 {
        let mut f = self.iso_frame.lock().unwrap();
        let base = *f;
        *f = f.wrapping_add(n);
        if *f == 0 {
            *f = 1;
        }
        base
    }

    // -- low-level tunnel I/O ---------------------------------------------- //
    fn encode_request(&self, frame: &SilexFrame) -> Vec<u8> {
        if self.dev_id.starts_with("usb") {
            frame.to_ds600_request_bytes()
        } else {
            frame.to_bytes()
        }
    }

    async fn send_bytes(&self, bytes: &[u8]) -> std::result::Result<(), TransferError> {
        let mut w = self.write.lock().await;
        let s = w.as_mut().ok_or(TransferError::Closed)?;
        map_io(timeout(self.timeout, s.write_all(bytes)).await)
    }

    /// Read one response frame off the read half. `block` = wait without a timeout (for
    /// the long-poll mux reader); otherwise the per-session timeout applies.
    async fn recv_frame(&self, block: bool) -> std::result::Result<SilexFrame, TransferError> {
        let mut r = self.read.lock().await;
        let s = r.as_mut().ok_or(TransferError::Closed)?;
        let to = if block { None } else { Some(self.timeout) };
        let ds600 = self.dev_id.starts_with("usb");
        loop {
        let mut head = vec![0u8; if ds600 { 10 } else { 12 }];
        read_exact_opt(s, &mut head, to).await?;
        let dlen = u32::from_be_bytes([head[6], head[7], head[8], head[9]]) as usize;
        if dlen > 16 * 1024 * 1024 {
            return Err(TransferError::Other(anyhow::anyhow!("Silex response exceeds 16 MiB")));
        }
        let mut full = head.to_vec();
        if dlen > 0 {
            let mut body = vec![0u8; dlen];
            read_exact_opt(s, &mut body, to).await?;
            full.extend_from_slice(&body);
        }
        let mut frame = if ds600 {
            SilexFrame::parse_ds600_response(&full)
        } else {
            SilexFrame::parse(&full, 0).map(|(frame, _)| frame)
        }.map_err(TransferError::Other)?;
        tracing::debug!(device = %self.dev_id, seq = frame.seq, status = frame.status1,
            length = frame.data.len(), "Silex response");
        if ds600 && frame.opcode & 0xff == 3 {
            let mut pending = self.partial_bulk.lock().unwrap();
            let prior = pending.get(&(frame.opcode, frame.seq)).map(Vec::len).unwrap_or(0);
            if pending.len() >= 128 || prior + frame.data.len() > 16 * 1024 * 1024 {
                pending.clear();
                return Err(TransferError::Other(anyhow::anyhow!("Silex continuation limit exceeded")));
            }
            if !assemble_bulk_response(&mut pending, &mut frame) {
                continue;
            }
        }
        return Ok(frame);
        }
    }

    /// Send a request frame (seq already assigned) and read its response atomically.
    async fn transfer(&self, req: SilexFrame) -> std::result::Result<SilexFrame, TransferError> {
        let bytes = self.encode_request(&req);
        let _g = self.io_lock.lock().await;
        self.send_bytes(&bytes).await?;
        self.recv_frame(false).await
    }

    // -- frame builders (assign seq/sel, do not send) ---------------------- //
    pub fn build_control_frame(&self, bm_request_type: u8, b_request: u8, w_value: u16,
                               w_index: u16, w_length: u16, out_data: Vec<u8>) -> SilexFrame {
        let is_out = bm_request_type & 0x80 == 0;
        let opcode = if is_out { 0x8001 } else { 0x0001 };
        SilexFrame {
            opcode,
            seq: self.next_seq(),
            is_request: true,
            endpoint: Some(0),
            bm_request_type: Some(bm_request_type),
            b_request: Some(b_request),
            w_value: Some(w_value),
            w_index: Some(w_index),
            w_length: Some(w_length as u32),
            data: if is_out { out_data } else { Vec::new() },
            sel: Some((self.dev_index as u16) << 8),
            tag: REQUEST_TAG,
            ..Default::default()
        }
    }

    pub fn build_bulk_frame(&self, endpoint: u8, direction_in: bool, length: u32, out_data: Vec<u8>) -> SilexFrame {
        let opcode = if direction_in { 0x0003 } else { 0x8003 };
        let ep = (endpoint & 0x7F) | if direction_in { 0x80 } else { 0x00 };
        SilexFrame {
            opcode,
            seq: self.next_seq(),
            is_request: true,
            endpoint: Some(ep),
            w_length: Some(length),
            data: if direction_in { Vec::new() } else { out_data },
            sel: Some(((self.dev_index as u16) << 8) | ep as u16),
            tag: REQUEST_TAG,
            ..Default::default()
        }
    }

    /// Interrupt request (opcode 0x02). `maxpacket` already includes the 0x8000 flag
    /// (= 0x8000 | wMaxPacketSize), `interval` = bInterval.
    pub fn build_interrupt_frame(&self, endpoint: u8, direction_in: bool, length: u32,
                                 maxpacket: u16, interval: u16, out_data: Vec<u8>) -> SilexFrame {
        let opcode = if direction_in { 0x0002 } else { 0x8002 };
        let ep = (endpoint & 0x7F) | if direction_in { 0x80 } else { 0x00 };
        SilexFrame {
            opcode,
            seq: self.next_seq(),
            is_request: true,
            endpoint: Some(ep),
            w_length: Some(length),
            maxpacket,
            interval,
            data: if direction_in { Vec::new() } else { out_data },
            sel: Some(((self.dev_index as u16) << 8) | ep as u16),
            tag: REQUEST_TAG,
            ..Default::default()
        }
    }

    pub fn build_iso_in_frame(&self, endpoint: u8, packets: Vec<(u32, u32)>, start_frame: u32,
                          maxpacket: u16, total_length: u32) -> SilexFrame {
        let ep = (endpoint & 0x7F) | 0x80;
        SilexFrame {
            opcode: 0x0004,
            seq: self.next_seq(),
            is_request: true,
            endpoint: Some(ep),
            iso_packets: packets,
            start_frame,
            maxpacket,
            interval: self.iso_interval(ep),
            total_length,
            sel: Some(((self.dev_index as u16) << 8) | ep as u16),
            tag: REQUEST_TAG,
            ..Default::default()
        }
    }

    pub fn build_iso_out_frame(&self, endpoint: u8, packets: Vec<(u32, u32)>, audio: Vec<u8>,
                           start_frame: u32, maxpacket: u16, nch: Option<u8>) -> SilexFrame {
        let ep = endpoint & 0x7F; // OUT: no 0x80 bit
        let mut req = SilexFrame {
            opcode: 0x8004,
            seq: self.next_seq(),
            is_request: true,
            endpoint: Some(ep),
            iso_packets: packets.clone(),
            start_frame,
            maxpacket,
            interval: self.iso_interval(ep),
            data: audio.clone(),
            sel: Some(((self.dev_index as u16) << 8) | ep as u16),
            tag: REQUEST_TAG,
            ..Default::default()
        };
        if let Some(n) = nch {
            let mut desc = Vec::with_capacity(packets.len() * 12);
            for (off, length) in &packets {
                desc.extend_from_slice(&off.to_be_bytes());
                desc.extend_from_slice(&length.to_be_bytes());
                desc.extend_from_slice(&0u32.to_be_bytes());
            }
            if let Some(payload) = compress_iso_out(&desc, &audio, n) {
                req.iso_out_payload = Some(payload);
            }
        }
        req
    }

    // -- high-level transfers ---------------------------------------------- //
    pub async fn control(
        &self,
        bm_request_type: u8,
        b_request: u8,
        w_value: u16,
        w_index: u16,
        w_length: u16,
        out_data: Vec<u8>,
    ) -> std::result::Result<(u16, Vec<u8>), TransferError> {
        let req = self.build_control_frame(bm_request_type, b_request, w_value, w_index, w_length, out_data);
        let resp = self.transfer(req).await?;
        Ok((resp.status1, resp.data))
    }

    pub async fn bulk(
        &self,
        endpoint: u8,
        direction_in: bool,
        length: u32,
        out_data: Vec<u8>,
    ) -> std::result::Result<(u16, Vec<u8>), TransferError> {
        let req = self.build_bulk_frame(endpoint, direction_in, length, out_data);
        let resp = self.transfer(req).await?;
        Ok((resp.status1, resp.data))
    }

    // -- iso pipelined path (single-owner worker) -------------------------- //
    /// Send one iso-IN request without waiting for the response (pipelining). Read the
    /// response later via [`iso_recv`]. The iso connection is served by exactly one
    /// worker task, so no io_lock is needed.
    pub async fn iso_send(&self, ep: u8, packets: Vec<(u32, u32)>, start_frame: u32,
                          maxpacket: u16, total_length: u32) -> std::result::Result<(), TransferError> {
        let f = self.build_iso_in_frame(ep, packets, start_frame, maxpacket, total_length);
        self.send_bytes(&self.encode_request(&f)).await
    }

    /// Send one iso-OUT (playback) request without waiting for the 12-byte ACK.
    pub async fn iso_send_out(&self, ep: u8, packets: Vec<(u32, u32)>, audio: Vec<u8>,
                              start_frame: u32, maxpacket: u16, nch: Option<u8>) -> std::result::Result<(), TransferError> {
        let f = self.build_iso_out_frame(ep, packets, audio, start_frame, maxpacket, nch);
        self.send_bytes(&self.encode_request(&f)).await
    }

    /// Read one iso response (FIFO to the send order).
    pub async fn iso_recv(&self) -> std::result::Result<IsoResp, TransferError> {
        let resp = self.recv_frame(false).await?;
        Ok((resp.status1, resp.iso_resp_packets, resp.data))
    }

    /// Pipelined iso-IN: send ALL requests at once, then read ALL responses (FIFO). This
    /// removes the per-URB round-trip latency -> realtime throughput for audio.
    pub async fn iso_batch(&self, requests: Vec<IsoInReq>) -> std::result::Result<Vec<IsoResp>, TransferError> {
        // Build frames (allocates seq) before taking io_lock.
        let frames: Vec<Vec<u8>> = requests
            .iter()
            .map(|(ep, packets, sf, mp, tl)| {
                self.encode_request(&self.build_iso_in_frame(*ep, packets.clone(), *sf, *mp, *tl))
            })
            .collect();
        let _g = self.io_lock.lock().await;
        {
            let mut w = self.write.lock().await;
            let s = w.as_mut().ok_or(TransferError::Closed)?;
            for fr in &frames {
                map_io(timeout(self.timeout, s.write_all(fr)).await)?;
            }
        }
        let mut out = Vec::with_capacity(requests.len());
        for _ in &requests {
            out.push(self.iso_recv().await?);
        }
        Ok(out)
    }

    // -- multiplex path (control + interrupt on one connection) ------------ //
    /// Send one request frame (seq already assigned by a builder), return its seq. For
    /// the mux worker (exactly one sender task per connection).
    pub async fn send_request(&self, frame: SilexFrame) -> std::result::Result<u16, TransferError> {
        let seq = frame.seq;
        self.send_bytes(&self.encode_request(&frame)).await?;
        Ok(seq)
    }

    /// Cancel a DS-600 URB using its original tunnel sequence and endpoint.
    /// The server suppresses the cancelled completion; no reply is expected.
    pub async fn cancel_request(&self, mut frame: SilexFrame) -> std::result::Result<(), TransferError> {
        if !self.dev_id.starts_with("usb") { return Ok(()); }
        frame.opcode |= 0x4000;
        let header_len = match frame.transfer_type() {
            crate::protocol::silex_frame::TransferType::Control => 22,
            crate::protocol::silex_frame::TransferType::Isochronous => 24,
            _ => 18,
        };
        let mut bytes = self.encode_request(&frame);
        bytes.truncate(header_len);
        self.send_bytes(&bytes).await
    }

    /// Read one response (blocking). For the mux reader (exactly one reader task); the
    /// caller dispatches it to the waiting URB by the seq in the response header.
    pub async fn recv_response(&self) -> std::result::Result<SilexFrame, TransferError> {
        self.recv_frame(true).await
    }

    pub async fn control_in(
        &self,
        bm_request_type: u8,
        b_request: u8,
        w_value: u16,
        w_index: u16,
        w_length: u16,
    ) -> std::result::Result<Vec<u8>, TransferError> {
        Ok(self
            .control(bm_request_type, b_request, w_value, w_index, w_length, Vec::new())
            .await?
            .1)
    }

    pub async fn get_device_descriptor(&self) -> std::result::Result<Vec<u8>, TransferError> {
        self.control_in(0x80, 0x06, 0x0100, 0x0000, 0x12).await
    }

    /// Full configuration descriptor (first 9 bytes for wTotalLength, then all of it).
    pub async fn get_config_descriptor(&self) -> std::result::Result<Vec<u8>, TransferError> {
        let head = self.control_in(0x80, 0x06, 0x0200, 0x0000, 9).await?;
        let total = if head.len() >= 4 {
            u16::from_le_bytes([head[2], head[3]])
        } else {
            9
        };
        self.control_in(0x80, 0x06, 0x0200, 0x0000, total).await
    }

    pub fn iso_out_format_loaded(&self) -> bool {
        self.iso_out_format.lock().unwrap().is_some()
    }

    pub fn set_endpoint_config(&self, config: &[u8]) {
        *self.endpoint_params.lock().unwrap() = super::device::parse_endpoint_params(config);
        *self.iso_out_format.lock().unwrap() = Some(parse_iso_out_formats(config));
    }

    fn iso_interval(&self, endpoint: u8) -> u16 {
        self.endpoint_params.lock().unwrap().get(&endpoint).map(|p| p.1).unwrap_or(1)
    }

    /// Load+parse the config descriptor and fill the iso-OUT format map. Call only on
    /// the connection's own (idle) session BEFORE iso requests are in flight, otherwise
    /// the control response would desync the iso stream.
    pub async fn load_iso_out_format(&self) {
        let fmt = match self.get_config_descriptor().await {
            Ok(cfg) => parse_iso_out_formats(&cfg),
            Err(_) => HashMap::new(), // format unknown -> raw passthrough
        };
        *self.iso_out_format.lock().unwrap() = Some(fmt);
    }

    /// (wMaxPacketSize, nch, subframe) for an OUT iso endpoint (0x0N), if it belongs to a
    /// 16-bit AudioStreaming interface; None => send raw.
    pub fn iso_out_info(&self, ep: u8) -> Option<(u16, u8, u8)> {
        self.iso_out_format
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|m| m.get(&(ep & 0x7F)).copied())
    }
}

fn now_millis() -> u32 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u32)
        .unwrap_or(1)
}

/// DS-600 status 0x0002 means another bulk segment follows with the same
/// sequence. Other transfers may complete between segments on a mux connection.
fn assemble_bulk_response(pending: &mut HashMap<(u16, u16), Vec<u8>>, frame: &mut SilexFrame) -> bool {
    let key = (frame.opcode, frame.seq);
    if frame.status1 == 2 {
        pending.entry(key).or_default().append(&mut frame.data);
        return false;
    }
    if let Some(mut data) = pending.remove(&key) {
        data.append(&mut frame.data);
        frame.data = data;
    }
    true
}

/// `read_exact` with an optional timeout; maps outcomes to [`TransferError`].
async fn read_exact_opt(
    s: &mut OwnedReadHalf,
    buf: &mut [u8],
    to: Option<Duration>,
) -> std::result::Result<(), TransferError> {
    match to {
        Some(d) => {
            map_io(timeout(d, s.read_exact(buf)).await)?;
            Ok(())
        }
        None => match s.read_exact(buf).await {
            Ok(_) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Err(TransferError::Closed),
            Err(e) => Err(TransferError::Other(e.into())),
        },
    }
}

/// Map a `timeout(...).await` result over an `io::Result` into a `TransferError`.
fn map_io<T>(
    r: std::result::Result<std::io::Result<T>, tokio::time::error::Elapsed>,
) -> std::result::Result<T, TransferError> {
    match r {
        Err(_) => Err(TransferError::Timeout),
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => Err(TransferError::Closed),
        Ok(Err(e)) => Err(TransferError::Other(e.into())),
    }
}

/// Decode the key fields of a USB device descriptor.
pub fn parse_device_descriptor(desc: &[u8]) -> Result<DeviceDescriptor> {
    if desc.len() < 18 || desc[1] != 0x01 {
        bail!("not a device descriptor");
    }
    Ok(DeviceDescriptor {
        bcd_usb: u16::from_le_bytes([desc[2], desc[3]]),
        b_device_class: desc[4],
        b_device_subclass: desc[5],
        b_device_protocol: desc[6],
        b_max_packet_size0: desc[7],
        id_vendor: u16::from_le_bytes([desc[8], desc[9]]),
        id_product: u16::from_le_bytes([desc[10], desc[11]]),
        bcd_device: u16::from_le_bytes([desc[12], desc[13]]),
        b_num_configurations: desc[17],
    })
}

#[derive(Debug, Clone, Copy)]
pub struct DeviceDescriptor {
    pub bcd_usb: u16,
    pub b_device_class: u8,
    pub b_device_subclass: u8,
    pub b_device_protocol: u8,
    pub b_max_packet_size0: u8,
    pub id_vendor: u16,
    pub id_product: u16,
    pub bcd_device: u16,
    pub b_num_configurations: u8,
}

/// Find the OUT iso endpoints of Audio/AudioStreaming interfaces (bInterfaceClass=0x01,
/// SubClass=0x02) in a configuration descriptor: endpoint(0x0N) -> (wMaxPacketSize, nch,
/// subframe_bytes). Only these may be compressed, matching what the vendor client does
/// (nch = FORMAT_TYPE.bNrChannels [off 4], subframe = bSubframeSize [off 5]). Other iso
/// devices (e.g. UVC video, class 0x0E) do not appear -> they run raw 1:1 (UAC1 layout).
pub fn parse_iso_out_formats(cfg: &[u8]) -> HashMap<u8, (u16, u8, u8)> {
    let mut out = HashMap::new();
    let mut i = 0usize;
    let mut is_audio_streaming = false;
    let mut cur_nch: u8 = 0;
    let mut cur_subframe: u8 = 0;
    let n = cfg.len();
    while i + 2 <= n {
        let blen = cfg[i] as usize;
        if blen < 2 || i + blen > n {
            break;
        }
        let btype = cfg[i + 1];
        if btype == 0x04 && blen >= 9 {
            // INTERFACE
            let cls = cfg[i + 5];
            let sub = cfg[i + 6];
            is_audio_streaming = cls == 0x01 && sub == 0x02;
            cur_nch = 0;
            cur_subframe = 0;
        } else if btype == 0x24 && is_audio_streaming && blen >= 7 && cfg[i + 2] == 0x02 {
            // CS_INTERFACE / FORMAT_TYPE: bNrChannels, bSubframeSize
            cur_nch = cfg[i + 4];
            cur_subframe = cfg[i + 5];
        } else if btype == 0x05 && blen >= 7 {
            // ENDPOINT
            let addr = cfg[i + 2];
            let attrs = cfg[i + 3];
            let mps = u16::from_le_bytes([cfg[i + 4], cfg[i + 5]]) & 0x7FF;
            if is_audio_streaming && (attrs & 0x03) == 0x01 && (addr & 0x80) == 0 {
                // isochronous OUT endpoint of an AudioStreaming interface
                out.insert(addr & 0x7F, (mps, cur_nch, cur_subframe));
            }
        }
        i += blen;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn ds600_cancel_keeps_sequence_endpoint_and_omits_out_payload() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = tokio::net::TcpStream::connect(listener.local_addr().unwrap()).await.unwrap();
        let (mut remote, _) = listener.accept().await.unwrap();
        let session = SilexSession::new("127.0.0.1", [0; 6], "usb1-002:0", None, 19540, Duration::ZERO);
        let (_, write) = client.into_split();
        *session.write.lock().await = Some(write);
        let mut request = session.build_bulk_frame(2, false, 7, vec![0xAB; 7]);
        request.seq = 0x1234;
        session.cancel_request(request).await.unwrap();
        let mut packet = [0; 18];
        remote.read_exact(&mut packet).await.unwrap();
        assert_eq!(&packet[..10], &[0xC0, 3, 0x12, 0x34, 0, 1, 2, 2, 2, 5]);
        assert!(timeout(Duration::from_millis(30), remote.read_u8()).await.is_err());
    }

    #[test]
    fn ds600_bulk_segments_are_one_completion_even_when_interleaved() {
        let mut pending = HashMap::new();
        let mut segment = SilexFrame { opcode: 3, seq: 207, status1: 2,
            data: vec![0x11; 20480], ..Default::default() };
        assert!(!assemble_bulk_response(&mut pending, &mut segment));
        let mut other = SilexFrame { opcode: 3, seq: 208, data: vec![0x55; 13], ..Default::default() };
        assert!(assemble_bulk_response(&mut pending, &mut other));
        assert_eq!(other.data.len(), 13);
        segment.data = vec![0x22; 20480];
        assert!(!assemble_bulk_response(&mut pending, &mut segment));
        segment.status1 = 0;
        segment.data = vec![0x33; 20480];
        assert!(assemble_bulk_response(&mut pending, &mut segment));
        assert_eq!(segment.data.len(), 61440);
        assert_eq!((segment.data[0], segment.data[20480], segment.data[40960]), (0x11, 0x22, 0x33));
        assert!(pending.is_empty());
    }

    #[tokio::test]
    async fn failed_tcp_connect_releases_udp_reservation() {
        // Hold a non-listening TCP socket so TCP connections are refused while
        // a mock control service can still bind the same port over UDP.
        let closed_tcp = tokio::net::TcpSocket::new_v4().unwrap();
        closed_tcp.bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let addr = closed_tcp.local_addr().unwrap();
        let udp = UdpSocket::bind(addr).await.unwrap();
        let mock = tokio::spawn(async move {
            let mut buf = [0u8; 1024];
            let mut query_peer = None;
            for (command, phase) in [
                (ud::CMD_CONNECT, Phase::Query),
                (ud::CMD_CONNECT, Phase::Commit),
                (ud::CMD_DISCONNECT, Phase::Query),
                (ud::CMD_DISCONNECT, Phase::Commit),
            ] {
                let (n, peer) = udp.recv_from(&mut buf).await.unwrap();
                match ud::parse(&buf[..n]).unwrap() {
                    Message::Command(request) => {
                        assert_eq!(request.command, command);
                        assert_eq!(request.phase, phase);
                        assert_eq!(request.dev_id.as_deref(), Some("dev01"));
                        if phase == Phase::Query {
                            query_peer = Some(peer);
                        } else {
                            assert_eq!(Some(peer), query_peer);
                            assert_eq!(request.op_code, 3);
                        }
                    }
                    other => panic!("unexpected request: {other:?}"),
                }
                let response: &[u8] = if phase == Phase::Query {
                    b"\x00\x01\x00\x00\x00\x00\x00\x03OK\x00"
                } else {
                    b"\x00\x03\x00\x00\x00\x00\x00\x00"
                };
                udp.send_to(response, peer).await.unwrap();
            }
        });
        let session = SilexSession::new("127.0.0.1", [0; 6], "dev01",
            Some("127.0.0.1".into()), addr.port(), Duration::ZERO);
        assert!(session.connect_with(0, Duration::ZERO).await.is_err());
        timeout(Duration::from_secs(1), mock).await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn denied_connection_is_committed_without_release_or_retry() {
        let udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = udp.local_addr().unwrap();
        let mock = tokio::spawn(async move {
            let mut buf = [0u8; 1024];
            let (_, peer) = udp.recv_from(&mut buf).await.unwrap();
            udp.send_to(b"\x00\x01\x00\x00\x00\x00\x00\x05DENY\x00", peer).await.unwrap();
            let (n, commit_peer) = udp.recv_from(&mut buf).await.unwrap();
            assert_eq!(peer, commit_peer);
            assert_eq!(&buf[..4], &[0, 3, 0, 1]);
            assert_eq!(crate::wire::rd_u16(&buf[..n], 84), 5);
            udp.send_to(b"\x00\x03\x00\x00\x00\x00\x00\x00", peer).await.unwrap();
            assert!(timeout(Duration::from_millis(100), udp.recv_from(&mut buf)).await.is_err());
        });
        let session = SilexSession::new("127.0.0.1", [0; 6], "usb1-002:0",
            Some("127.0.0.1".into()), addr.port(), Duration::ZERO);
        assert!(session.connect().await.unwrap_err().to_string().contains("USB port connection password"));
        timeout(Duration::from_secs(1), mock).await.unwrap().unwrap();
    }

    #[test]
    fn dev_index_from_dev_id() {
        let s = SilexSession::new("192.0.2.1", [0; 6], "dev02", Some("192.0.2.9".into()), ud::PORT, Duration::ZERO);
        assert_eq!(s.dev_index, 4);
    }

    #[test]
    fn forced_dev_index_overrides() {
        let s = SilexSession::with_dev_index("192.0.2.1", [0; 6], "dev05", Some("192.0.2.9".into()), ud::PORT, Duration::ZERO, Some(3));
        assert_eq!(s.dev_index, 3);
    }

    #[test]
    fn alloc_iso_frames_monotonic_and_nonzero() {
        let s = SilexSession::new("192.0.2.1", [0; 6], "dev01", Some("192.0.2.9".into()), ud::PORT, Duration::ZERO);
        let a = s.alloc_iso_frames(8);
        let b = s.alloc_iso_frames(8);
        assert_eq!(b, a.wrapping_add(8));
        assert_ne!(a, 0);
    }

    #[test]
    fn iso_frames_use_configured_endpoint_interval_in_both_directions() {
        let s = SilexSession::new("192.0.2.1", [0; 6], "usb5-005:0", None, ud::PORT, Duration::ZERO);
        s.set_endpoint_config(&[
            9, 4, 3, 1, 1, 1, 2, 0x20, 0,
            7, 5, 9, 0x0d, 193, 0, 4,
            9, 4, 4, 1, 1, 1, 2, 0x20, 0,
            7, 5, 0x81, 5, 197, 0, 4,
        ]);
        let input = s.build_iso_in_frame(1, vec![(0, 196)], 0x12345678, 0x80c5, 208);
        let output = s.build_iso_out_frame(9, vec![(0, 192)], vec![0; 192], 0x12345678, 0x80c1, None);
        for frame in [input, output] {
            assert_eq!(frame.interval, 4);
            assert_eq!(&s.encode_request(&frame)[12..16], &[0, 4, 0, 0]);
        }
    }

    #[test]
    fn parse_device_descriptor_fields() {
        // 18-byte device descriptor: bLength=18, bType=1, bcdUSB=0x0200, ... vid=0x0930 pid=0x6544
        let desc = [
            0x12, 0x01, 0x00, 0x02, 0x00, 0x00, 0x00, 0x40, 0x30, 0x09, 0x44, 0x65, 0x00,
            0x01, 0x01, 0x02, 0x03, 0x01,
        ];
        let d = parse_device_descriptor(&desc).unwrap();
        assert_eq!(d.bcd_usb, 0x0200);
        assert_eq!(d.id_vendor, 0x0930);
        assert_eq!(d.id_product, 0x6544);
        assert_eq!(d.b_num_configurations, 1);
    }

    #[test]
    fn parse_device_descriptor_rejects_garbage() {
        assert!(parse_device_descriptor(&[0u8; 4]).is_err());
    }

    #[test]
    fn parse_iso_out_formats_picks_audio_streaming_out_endpoint() {
        let cfg = [
            // INTERFACE: class 0x01 (audio), subclass 0x02 (streaming)
            9, 0x04, 1, 1, 1, 0x01, 0x02, 0x00, 0x00,
            // CS_INTERFACE FORMAT_TYPE: nch=2, subframe=2, 16-bit
            11, 0x24, 0x02, 0x01, 0x02, 0x02, 0x10, 0x00, 0x00, 0x00, 0x00,
            // ENDPOINT: addr 0x02 (OUT), iso, mps 200
            9, 0x05, 0x02, 0x01, 0xC8, 0x00, 0x01, 0x00, 0x00,
        ];
        let m = parse_iso_out_formats(&cfg);
        assert_eq!(m.get(&0x02), Some(&(200u16, 2u8, 2u8)));
    }

    #[test]
    fn parse_iso_out_formats_ignores_non_audio() {
        let cfg = [
            // INTERFACE: class 0x0E (video), not audio-streaming
            9, 0x04, 1, 1, 1, 0x0E, 0x02, 0x00, 0x00,
            // ENDPOINT: iso OUT
            9, 0x05, 0x02, 0x01, 0xC8, 0x00, 0x01, 0x00, 0x00,
        ];
        assert!(parse_iso_out_formats(&cfg).is_empty());
    }
}
