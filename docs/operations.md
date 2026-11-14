# Installation and operations

This build targets Linux x86-64 with systemd, USB/IP (`vhci-hcd`), Python 3.9+
and PySide6. The Debian package is validated on Ubuntu 26.04.1 and in a Debian 13 (trixie)
container. The RPM is also installation-tested in a Fedora 45 container.
Container checks cover package dependencies and GUI imports, not kernel USB/IP
attachment or a running systemd service.
Ubuntu 24.04 does not ship the required PySide6 packages; use a newer distribution
or install the Python client separately in a virtual environment.

## Install

```sh
sudo apt install ./dist/release/opensilexlink_1.1.2_amd64.deb
sudo usermod -aG opensilexlink "$USER"
# Log out and back in to activate the new group, then:
osl-gui
```

For the current terminal only, `sg opensilexlink -c osl-gui` activates the group
without logging out. Start the app from the OpenSilexLink desktop menu entry.
The installed launchers are `/usr/bin/osl-gui` and `/usr/bin/osl gui`.
When the current session lacks socket access, the GUI offers **Run as
administrator** or Cancel. The administrator option uses `sudo -A` with a
graphical password dialog; enter your Linux account password, not the DS-600
password. Existing sudo authorization may avoid a password prompt. Your account
must be permitted to run sudo. Cancellation leaves the service running.
The password is passed directly to sudo and is not stored by the app.

Administrator mode is visibly marked, always connects to the installed system
socket, disables per-device application launch, and exits when its window
closes. Normal group-authorized windows retain the tray behavior. Socket ACLs
are respected: access granted by an ACL does not trigger an unnecessary sudo
prompt. A stopped service produces a service error rather than requesting sudo.

Add a server by IPv4 address, select a device and click Connect. Device properties
include startup and recovery preferences. The same two checkboxes are available
below the device list: **Reconnect after restart, before login** and
**Reconnect after an unexpected disconnect**. The startup preference is saved by the root system service and enables that
service at boot. No desktop session is required; networking must be available
system-wide. Startup connection is independent of recovery after a later loss.
Recovery retries indefinitely, with delay capped at eight seconds, until the
device returns or the preference is disabled. A reconnecting device remains
visible in the UI, and Disconnect cancels its pending retries. Manual Disconnect
stays disconnected until explicit Connect, re-enabling its startup preference,
or a system/service restart. Settings affect the shared
service; per-device application launch runs as the desktop user.

Select a server or any of its devices and enable **Connect all USB devices on
this server** to bind every available device, including devices behind docking
station hubs. This policy is saved by server MAC address, works before login
and includes devices added later. Composite interfaces are attached together
as one physical device. Busy devices owned by another computer and protected
USB ports without a saved password are skipped. Each device retains its own unexpected-loss
recovery preference. Manual Disconnect stays disconnected until explicit
Connect, a service restart, or turning connect-all off and on again. Turning
connect-all off leaves active connections and individual startup rules intact.

The same operation is available over IPC:

```sh
osl ipc set_bind_all '{"mac":"00:11:22:33:44:55","enabled":true}'
```

The package starts `opensilexlink.service`. GUI windows and `osl` share its Unix
socket at `/run/opensilexlink/control.sock`; closing the window does not detach
USB devices. The daemon runs as root to load vhci-hcd and attach/detach USB/IP
ports. The socket is root:opensilexlink, mode 0660, in a mode 0750 directory.
Membership of this group authorizes management of the shared USB connections.
No passwords or preconfigured device connections are embedded in the application.

Bluetooth audio additionally needs the desktop user's PipeWire and WirePlumber
services. If headphones pair but fail to connect with an A2DP `Protocol not
available` error, check those services and their startup configuration:

```sh
systemctl --user enable --now pipewire.socket pipewire-pulse.socket pipewire.service pipewire-pulse.service wireplumber.service
```

Run this as the desktop user. The USB connection manager does not start a root
audio server. DS-600 dock audio uses isochronous transfers; this build handles
their timing and cancellation while keeping the dock's display functions
connected when playback or capture stops. WirePlumber supports per-device
[ALSA rules](https://pipewire.pages.freedesktop.org/wireplumber/daemon/configuration/alsa.html),
but the validated setup does not disable the dock's audio device.

```sh
systemctl status opensilexlink.service
journalctl -u opensilexlink.service -b
osl ipc list_servers
osl ipc get_settings
```

Service settings are written atomically to
`/var/lib/opensilexlink/silex/settings.json`, readable by root only. Back up this
file to preserve search addresses, aliases and connection rules. Development
mode uses the user's XDG configuration directory instead. `SILEX_SOCKET` selects
an alternate client socket when testing.

## DS-600 connection capacity

The DS-600's advertised maximum is [15 connections](https://www.silextechnology.com/connectivity-solutions/device-connectivity/ds-600).
The stock test unit running 1.4.4 advertised 15 USB interfaces, so a composite
device can consume several slots while appearing as one device in this UI.
Inspection of the vendor's downloadable **1.5.0** firmware found a default
`maxconn` of 15 in `sxuptp.ko`; its interface registration check accounts for
the new device's complete interface count. The module subsequently copied from
the running 1.4.4 unit had the identical SHA-256, and its writable parameter
was verified on hardware.
The startup script `/etc/init.d/S90svu` also declares `MAXCONN=15`, but never
uses that variable when loading the module.

The original test setup exhausted this budget with only five physical devices:

| Device | Advertised USB interfaces |
| --- | ---: |
| CSR8510 Bluetooth adapter | 2 |
| Cypress USB-Serial Bridge | 3 |
| Logitech Unifying receiver | 3 |
| DisplayLink dock video/audio function | 5 |
| ThinkPad Ethernet | 2 |
| Total | 15 |

A keyboard added behind the dock was absent from both the DS-600 web status and
its raw discovery list. The host had only five of its sixteen vhci ports in use.
This is not a five-device limit in the client or an exhausted Linux port table.
Disconnecting a device in OpenSilexLink releases its network reservation, but
does not remove its registered interfaces from the server: physically unplug
an unused device to free capacity. Increasing Linux USB/IP ports cannot raise
the server's interface budget.

The test DS-600 has since been upgraded to **1.5.0** and given a persistent
startup patch setting `maxconn=32` before both the driver and `sxdevd` initialize.
The Logi Bolt keyboard receiver adds three interfaces: all **18 interfaces / six
devices** now enumerate after reboot, with SSH disabled. The owner confirmed
both screens, keyboard, mouse and audio work. The full 32-interface budget has
not been stress-tested. See [the patch, build and validation](ds600-firmware.md)
and [rollback instructions](ds600-recovery.md). Firmware modification is a
separate administrative operation; the normal client package does not perform it.

## Storage and shutdown

Use your file manager or `udisksctl` to mount/unmount normal filesystems. The
connection manager attaches USB devices; it does not format disks or choose a
filesystem. Unmount storage before disconnecting it, stopping the service,
removing the package or shutting down its network/server. Interactive disconnect
and peer release refuse a device with mounted filesystems or active block swap.
This check covers mounts visible to the service; mounts in isolated containers
and indirect mappings such as LVM require operator coordination.

The test SanDisk contains a hybrid Proxmox installer ISO. Mount the **whole USB
disk** read-only, not its ISO partition table entries. See `ds600.md`.

SIGINT/SIGTERM detach tracked vhci ports and release server reservations.
Systemd restarts a failed daemon after five seconds, rediscovers configured
servers and reapplies startup-connection rules. Recovery also follows a device whose
USB address changes, retaining its logical export bus ID. Linux may re-enumerate
the device during recovery; this is automatic reattachment, not uninterrupted
USB operation. Open filesystems may need to be unmounted and mounted again. A network outage can cause
storage I/O failures; reconnecting cannot recover writes lost during an outage.
Do not place irreplaceable writable storage on this transport without independent
backups and application-level failure handling. Power loss/restart durability
has not been certified by this project.

Recovery commands have a 30-second deadline in service/headless mode. Desktop
polkit commands allow 120 seconds including the administrator prompt. On timeout,
the daemon signals the command's process group and returns an error without
waiting indefinitely for it to exit. If the tracked child remains in kernel I/O,
further commands are refused until it exits; IPC status requests continue to work.
Logs include the command, PID and timeout or detach failure.

Recovery also checks whether the local exporter ended, because an occupied VHCI
port alone does not prove the remote tunnel is healthy. Stale attachments are
detached before reconnecting. Failed detaches remain tracked for retry, and a
failed attach's partially allocated ports are cleaned up before another attach.
Automatic cleanup checks mounted storage and swap using the same visibility
limits described above. Linux kernel I/O stuck in uninterruptible sleep can
outlast the command deadline; a deadline cannot force the kernel to complete it.

These changes address blocking paths found while investigating a reported DS-600
reboot stall in 1.1.1. The original host incident has not been reproduced on
hardware. Regression tests cover slow detach with responsive IPC, command
timeouts, partial imports, stale occupied ports and shutdown with pending reads.

## Upgrade and remove

Install a newer package with apt. Upgrades preserve settings and leave active
connections running on the old binary. After unmounting USB storage, run
`sudo systemctl restart opensilexlink.service` to use the new binary. This avoids
an unexpected device removal during an upgrade. A scoped needrestart policy
defers automatic restarts of these USB connection services while still reporting
them for an administrator to restart.

Unmount USB storage, then `sudo apt remove opensilexlink`. Removal stops and
disables the shared manager; settings and the authorization group are retained.
Delete `/var/lib/opensilexlink` explicitly if the saved settings are no longer
needed.

## Headless mode and troubleshooting

The optional `silex-connect@SERVER_IP.service` template connects all devices
without a GUI; it conflicts with the shared manager. Use one mode at a time.
For explicit control run the following in the foreground:

```sh
sudo systemctl stop opensilexlink.service
sudo silex-daemon connect 192.168.178.150 --all --escalate none
```

A USB connection password is separate from the web administrator password.
Select the DS-600 (or any of its devices) and click **USB port passwords…**.
Enter the existing password for the **USB 3.0 physical port** and/or the
**USB 2.0 physical port**. This saves client credentials; it does not change the
passwords configured on the Silex itself. Leave a field blank to retain its saved
value; use **Forget saved password** to remove it. Passwords are limited to
16 UTF-8 bytes, with no NUL character.

Every device behind a hub or dock uses the password of the physical connector
the hub plugs into. On the DS-600, bus 4 (USB 2-speed dock functions) and bus 5
(SuperSpeed functions) both use the USB 3.0 port password; bus 1 uses the USB 2.0
port password. Unknown buses/models do not fall back to another port's password.

The service stores credentials separately from general settings in
`/var/lib/opensilexlink/silex/settings.passwords.json`, mode **0600**, inside the
service's private state directory. These passwords are plaintext on disk, so
protect backups accordingly. They are never returned through get-settings or
server status broadcasts. Saving a password does not disconnect existing devices.
Manual connection, connect-all, boot-time reconnect and unexpected-loss recovery
use the matching saved password. Manual Disconnect continues to suppress retries.
A saved password has not been authenticated until connection succeeds; replace
it in this dialog if authentication fails. Devices owned by another computer
remain unavailable until released there.

The standalone foreground helper can still use
`--password-file /path/to/private-file` (one password for that invocation).
That helper is separate from the managed service's per-physical-port credentials.
Authentication encoding and per-port credential routing have automated tests;
successful password login on physical hardware remains to be confirmed with
the owner's port passwords.

If the GUI reports Service offline, check the unit and group membership with
`systemctl status opensilexlink` and `id`. If `vhci-hcd` is missing, install your
distribution's matching kernel module package. If a drive is absent on the
DS-600, disable its NAS service in the web interface and restart the DS-600.
A device held by SX Virtual Link on another computer must be released first.

For a composite webcam or Android phone, release the whole device on the other
computer, including non-video interfaces, and disable its automatic reconnect
while testing. Select **Webcam** in the phone's USB preferences; a USB reset may
return it to its normal phone mode and change its discovery ID. Rediscover it
before reconnecting. The bridge verifies the USB vendor/product IDs against
discovery and rejects mismatches before attaching. It also reads the current
USB configuration and acknowledges duplicate configuration requests locally,
so Linux enumeration does not reconfigure an already active webcam gadget.

Only one server can have active connections in this release; several USB devices
on that server are supported. Another server is rejected explicitly. The local
USB/IP exporter listens only on 127.0.0.1:3240. Linux USB/IP does not authenticate
loopback peers: use a trusted single-user host. The Silex transport is plaintext;
use a trusted LAN, with firewall isolation from untrusted networks.

## Release checks

`packaging/build-packages.sh 1.1.2` runs Rust and Python tests, builds a locked
release binary, compiles translations, and produces Debian/RPM artifacts plus
SHA256SUMS. Install cargo, pytest, PySide6 (for GUI tests), and nfpm 2.45.0.
GUI tests run with Qt's offscreen platform. CI also tests installation in clean
Debian/Ubuntu/Fedora containers; running a workflow is still required before
claiming those platforms were validated. Hardware checks are documented
separately from automated tests. Packages are unsigned local builds.
