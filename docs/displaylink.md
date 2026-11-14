# DisplayLink dock on the DS-600

The test host uses Synaptics DisplayLink **6.4.0-22** and a separately patched
EVDI **1.15.1+osl1** kernel module. OpenSilexLink transports the dock's USB
devices; the DisplayLink userspace driver and EVDI provide the displays.
Install DisplayLink using the [official Ubuntu download and repository](https://www.synaptics.com/products/displaylink-graphics/downloads/ubuntu).
The core OpenSilexLink package does not install or modify graphics drivers.

## Validated result

On Ubuntu 26.04.1, kernel 7.0.0-38-generic, GNOME/Wayland, the ThinkPad Hybrid
USB-C with USB-A Dock (17e9:6015) provides two 2560×1440 displays at 60 Hz.
The owner confirmed both screens and smooth cursor movement with video paused.
Patched DKMS modules built for both installed kernels, 7.0.0-38 and 7.0.0-43;
only 7.0.0-38 was tested running. No host reboot was required, but the graphical
session was restarted to release and reload EVDI.

Initially one screen was black with a working cursor and cursor movement lagged
when the framebuffer did not change. EVDI logged `Failed to map scanout buffer`.
These mapping errors stopped after the patch was activated. The final setup
also enables TCP_NODELAY on incoming OpenSilexLink USB/IP connections. The effect
of the two changes on cursor latency was not measured separately.

The dock additionally exposes Ethernet, audio and USB receivers. Enable
**Connect all USB devices on this server** in OpenSilexLink to attach all
available functions, including newly discovered devices. Dock audio transport
has passed short full-duplex tests at 48 kHz/S16_LE (stereo or six-channel
playback with stereo capture), including stream stops without losing video.
Ethernet throughput remains untested. See [hardware results](ds600.md).

If starting audio makes both screens disappear, distinguish an audio tunnel
reset from a graphics failure: `journalctl -u opensilexlink` shows an
isochronous transfer error, and the composite dock disappears from `lsusb`.
This build fixes the DS-600 interval field and handles capture cancellation
concurrently. Disabling the dock's sound device is not required.

## Optional mapping workaround

This is a local experimental EVDI build, not a Synaptics production release.
The kernel source is GPL-2.0-only and separate from OpenSilexLink's MIT license.
The package retains the vendor copyright and library binaries, and includes
the complete patched module source under `/usr/src/evdi-1.15.1+osl1`.

The builder pins Synaptics package `evdi-1.15.1-197_amd64.deb`, SHA256:

```
0201848d1ad7283d28ca1ba9b81edf8581c07b89c5ad5302b773e615b1d0489a
```

It applies:

1. [Upstream PR 604](https://github.com/DisplayLink/evdi/pull/604), commit
   `b877fbf74538dbf595324b83edc86d1057c2e179`, by Kieran (TigerWolf), adding
   GEM vmap/vunmap callbacks for PRIME mappings.
2. A local refinement keeping importer mappings separate from the painter's
   cached mapping and balancing page pins on failure, success and unmap.

This addresses the class of mapping failure discussed in
[EVDI issue 591](https://github.com/DisplayLink/evdi/issues/591). The separate
experimental vblank timer proposal is not included.

Build with Debian packaging tools, curl, patch and Python 3 installed:

```sh
packaging/evdi/build-package.sh
# Or supply the pinned vendor package already downloaded:
packaging/evdi/build-package.sh /path/to/evdi-1.15.1-197_amd64.deb dist
```

The script verifies the vendor checksum and produces
`dist/evdi_1.15.1-197+osl1_amd64.deb`. It does not install it. Before replacing
EVDI, retain the existing graphics packages for rollback and have an SSH or
console session available. Install the package and check DKMS builds:

```sh
sudo apt install ./dist/evdi_1.15.1-197+osl1_amd64.deb
dkms status
modinfo evdi
```

This package replaces `evdi-dkms` and `libevdi1`. A reboot is the straightforward
way to activate it; `modinfo` alone describes the file on disk, not necessarily
the loaded module. Check `/sys/module/evdi/version` and
`/sys/module/evdi/srcversion` after activation. Reloading without reboot requires
stopping the graphical session and DisplayLink service, releasing EVDI devices,
and unloading the module; it interrupts the desktop.

## Recovery and rollback

Check `systemctl status displaylink-driver`, `dkms status`, and
`journalctl -k -b` for driver and mapping errors. Check USB attachment separately
with `osl ipc list_servers` and `lsusb`.

To return to Ubuntu's packages, remove the local package and reinstall the
distribution packages, then reboot:

```sh
sudo apt remove evdi
sudo apt install evdi-dkms libevdi1
sudo reboot
```

If a Synaptics `evdi` package was installed previously, reinstall that saved
vendor package instead of the distribution pair. APT may need to remove or
reinstall `displaylink-driver` to satisfy the selected provider; review its
transaction and restore the driver afterward. Keep the saved packages until
the rollback has been verified.
