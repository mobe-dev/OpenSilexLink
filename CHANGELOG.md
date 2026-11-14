# Changelog

## 1.1.0

- Add DS-600 discovery, reservations, packet framing and segmented transfers.
- Support concurrent USB devices and connecting all devices on a server,
  including docking stations and devices discovered later.
- Add systemd-managed connections, reconnect before login after restart, and
  recovery after unexpected disconnects. Deliberate Disconnect cancels recovery.
- Add desktop controls for reconnect settings, server-wide connections and
  interactive administrator startup when service access is unavailable.
- Correct USB cancellation and DS-600 isochronous transfers for audio alongside
  DisplayLink video, Bluetooth, keyboard, mouse and dock Ethernet.
- Package the desktop app, CLI, daemon, service, permissions policy and operating
  instructions as Debian and RPM packages.
- Document hardware validation, firmware recovery and optional EVDI source patches.
- Retain original MIT attribution, include Rust dependency notices and restrict
  release uploads to the application packages and notices.

Validated locally on Ubuntu 26.04.1 with a DS-600 and six attached devices.
See README for limitations. EVDI binaries and firmware images are excluded
from this release; separate vendor drivers may be needed for DisplayLink docks.
