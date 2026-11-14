# OpenSilexLink — DS-600 Linux client

Connect USB devices attached to a Silex device server to Linux through the
kernel's USB/IP drivers. Includes a Rust connection manager, PySide6 desktop UI,
CLI, systemd integration, and Debian/RPM packaging. No custom kernel module.

This build extends [bytewarrior/OpenSilexLink](https://github.com/bytewarrior/OpenSilexLink)
(MIT, upstream commit `7198e0ead83f6d1ae6095d64c1945a671579a464`). Upstream
implemented DS-510 support; this checkout adds and validates the DS-600 protocol.
It is an independent project, unaffiliated with Silex Technology.

## Install and launch

Primary validated platform: **Ubuntu 26.04.1, x86-64** with systemd and vhci-hcd.

```sh
sudo apt install ./dist/release/opensilexlink_1.1.0_amd64.deb
sudo usermod -aG opensilexlink "$USER"
# Log out and back in, or activate the group for this launch:
sg opensilexlink -c osl-gui
```

The installed UI binary is `/usr/bin/osl-gui`; launch it with `osl-gui`, `osl gui`,
or **OpenSilexLink** in the desktop menu. If your current session cannot access
the service, the UI offers **Run as administrator** and an interactive sudo
password dialog. Group membership allows normal operation without sudo.
Use **Add server…** to enter its IP,
select a USB device, and click **Connect**. The normal Linux driver handles it.
The UI shows live connection status, server settings, per-device properties,
a system tray and per-device reconnect controls. **Connect all USB devices on
this server** binds all available devices and newly discovered devices on a
selected server, including docking stations. **Reconnect after restart**
works through the system service before login. **Reconnect after an unexpected
disconnect** keeps retrying through outages; deliberate Disconnect cancels it. Devices remain connected when the
window closes. All windows share the managed `opensilexlink.service`.

Unmount storage before disconnecting. Interactive disconnect refuses mounted
filesystems and active swap devices. This release supports multiple USB devices
on **one active device server**; switching servers requires disconnecting its
devices first. Use a trusted host and LAN: USB/IP loopback and Silex traffic do
not provide transport authentication/encryption.

## Validation and scope

The detailed checks below used the DS-600 at **192.168.178.150**, firmware
**1.4.4**. It has since been updated to **1.5.0** with an optional
[persistent 32-interface capacity patch](docs/ds600-firmware.md). Six devices
(18 interfaces) reconnect after reboot with SSH disabled; the owner confirmed
both screens, keyboard, mouse and audio work. [Rollback and recovery](docs/ds600-recovery.md)
are documented separately.

- **SanDisk Extreme 0781:5580:** native storage driver, read-only mount, directory
  listing and matching uncached 64 MiB reads. Validated on the test host at
  `/mnt/silex-ds600`.
- **Cypress Bluetooth 04b4:f901:** native btusb, powered BlueZ controller and
  successful discovery of nearby devices.
- **CSR8510 A10 Bluetooth 0a12:0001 with MOMENTUM 3:** pairing, connection,
  aptX playback and a short mSBC hands-free playback/capture test.
- **ThinkPad DisplayLink dock 17e9:6015:** DisplayLink 6.4 and patched EVDI
  1.15.1+osl1, two 2560×1440 displays at 60 Hz; both screens and smooth cursor
  movement with video paused confirmed by the owner. See the separate
  [DisplayLink setup and EVDI workaround](docs/displaylink.md).
  Dock USB audio: 48 kHz, 16-bit stereo and six-channel playback with simultaneous
  stereo capture, including clean start/stop cycles without disconnecting video.
- **ThinkPad Ethernet 17ef:a359 and Logitech receivers 046d:c52b / 046d:c548:**
  concurrent attachment and native Linux drivers. Ethernet throughput has not
  been tested.
- Automatic tests cover codecs, segmented storage transfers, cancellation,
  orchestration, mounted-device refusal, IPC, settings and desktop behavior.

Writable storage durability, long-running audio and recovery under packet loss,
other Bluetooth profiles and other kernels need further validation.
The package is a tested local release; these remaining checks prevent a blanket
production-readiness claim. See [hardware results](docs/ds600.md) and
[installation and operations](docs/operations.md) for exact coverage and recovery.

## Build

Requires Rust/Cargo 1.93.1, Python 3 with pytest/PySide6, and nfpm 2.45.0 for packages.

```sh
cargo test --locked --manifest-path daemon/Cargo.toml
QT_QPA_PLATFORM=offscreen python3 -m pytest -q client/tests
packaging/build-packages.sh 1.1.0 dist/release
```

The build produces `.deb`, `.rpm`, and `SHA256SUMS` in `dist/release/`. Installation is tested on the Ubuntu target host and in Debian 13 / Fedora 45
containers (dependency resolution and GUI imports).
The package includes desktop dependencies, systemd units, license and operations
documentation. Install hooks preserve settings; upgrades leave live connections
running until the administrator unmounts drives and restarts the service.

For foreground use without the desktop service:

```sh
cargo build --locked --release --manifest-path daemon/Cargo.toml
./daemon/target/release/silex-daemon discover --server 192.168.178.150
sudo ./daemon/target/release/silex-daemon connect 192.168.178.150 --all --escalate none
```

Run only one manager at a time. Ctrl-C/SIGTERM detaches devices and releases
reservations. [Protocol notes](docs/protocol.md) describe the wire format;
[LICENSE](LICENSE) retains the upstream MIT attribution.

## License and releases

Application source is MIT, retaining the original bytewarrior attribution.
EVDI module patches are separately GPL-2.0-only; dependency notices accompany
the binary packages. Vendor firmware and repacked EVDI binaries are excluded
from public releases. See the [license review](docs/licensing.md) and
[release instructions](docs/releases.md).
