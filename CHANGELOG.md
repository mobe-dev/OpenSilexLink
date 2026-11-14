# Changelog

## 1.1.1 (unreleased)

- Add separate saved connection passwords for the DS-600's USB 3.0 and USB 2.0
  physical connectors, with masked entry, replacement and forgetting in the UI.
- Use the same USB 3.0 password for both USB 2-speed and SuperSpeed dock
  functions, including composite sub-interfaces. Support manual, connect-all,
  startup and unexpected-loss connections with the appropriate credentials.
- Keep passwords in a separate owner-only file and out of settings responses
  and status broadcasts; preserve deliberate-disconnect behavior.
- Reject a tunnel whose USB vendor/product IDs differ from discovery before
  reading its configuration or attaching it; log tunnel read failures.
- Report unavailable daemon connections as connection errors in the desktop
  client instead of raising an unhandled assertion; clean up failed IPC sends.
- Read the device's active USB configuration before import and acknowledge
  duplicate configuration requests locally. This avoids resetting a Pixel's
  webcam gadget during Linux enumeration while forwarding actual changes.

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
