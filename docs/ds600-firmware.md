# DS-600 interface limit investigation

The test DS-600 at `192.168.178.150`, MAC `84:25:3f:3f:64:23`, was upgraded to
the official **1.5.0** firmware on 2026-11-14, then given a two-line startup
patch requesting **32 USB interfaces**. It boots with **18 interfaces across
six physical devices**, including the previously missing Logi Bolt keyboard
receiver. The vendor kernel module and its registration check are unchanged.
The previous 1.4.4 flash and the installed stock 1.5.0 rootfs were backed up.

This increases an interface budget, not a guaranteed physical-device count.
The measured setup uses 18 interfaces; sustained operation at the full limit
of 32 has not been validated. See [rollback and physical recovery](ds600-recovery.md).

## Source and evidence

Silex's [product page](https://www.silextechnology.com/connectivity-solutions/device-connectivity/ds-600)
advertises a maximum of 15 connections and links the
[official 1.5.0 firmware archive](https://www.silextechnology.com/hubfs/Resource%20firmware/DS-600%20Firmware%20Updates/DS-600_1_5_0.zip?hsLang=en-us).
The extracted image identifies itself as 1.5.0 in `etc/default/romver.conf`.
The published changes are OpenSSL 3 support and an option to disable SSL
3.0/TLS 1.0/1.1. The running device reports `I8021X_LEGACY_TLS=DISABLE`.
The release notes do not enumerate additional security fixes.

Hashes below pin the exact inspected files, not arbitrary images with the same
version string:

| File | SHA-256 |
| --- | --- |
| `DS-600_1_5_0/DS-600.bin` (12,558,171 bytes) | `3e210fba10e042d132625959643092fd1183a6bebc6e5995fac9f5157101e9b5` |
| `lib/modules/sxuptp/sxuptp.ko` | `4bd9a0120a8cd5a30c3c0bb41f48440c63fdbe136f3a8f24f61e53201fad449c` |
| `etc/init.d/S90svu` before patching | `e05d534bd443b631b5e650df7505e0fed56f7af3bb67b9395b2f00be305cf56a` |

In that ARM32 Linux 3.10.70 module:

- `maxconn` is an integer module parameter with default 15 and mode `0644`.
  Its normal Linux sysfs path is `/sys/module/sxuptp/parameters/maxconn`.
- Its default is at `.data + 0x14`, file offset `0xb224`. The `__param` entry
  at section offset `0x50` points to that integer and `param_ops_int`.
- `sxuptp_register_interface` subtracts the device's already registered
  interfaces from the global count, adds the complete configuration's interface
  count, and compares the result against `maxconn`. The ARM comparison and
  rejection branch are at `.text + 0x580` and `.text + 0x584`.
- Exceeding the budget takes the rejection path at `.text + 0x634`, returning
  `-ENODEV`. This can hide an entire composite device rather than advertising
  only its remaining interfaces.
- Registration uses dynamically allocated list entries. The inspected code
  does not establish that every userspace component or resource pool supports
  32; finding this parameter is not a guarantee of stability at that value.

The module copied from the original running 1.4.4 device has the same SHA-256
as the 1.5.0 module. A live sysfs change on 1.4.4 first restored the keyboard,
and the owner confirmed it worked. The module comparison and runtime test
establish that the same parameter exists on this unit's original firmware;
that was not known from inspecting the 1.5.0 download alone.

`sxdevd` also reads this parameter during initialization and caches the budget.
After a runtime change it initially kept warning about a 15-interface limit;
restarting that daemon made its logged budget 32. Passing the parameter before
starting `sxdevd`, as the persistent patch does, addresses both checks.

## Runtime option

If an authenticated shell on a matching device is available, the module
parameter offers a reversible experiment before changing firmware:

```sh
# These paths belong to the DS-600, not the Ubuntu client.
cat /sys/module/sxuptp/parameters/maxconn
cat /sys/module/sxuptp/parameters/numconn
printf '%s\n' 32 > /sys/module/sxuptp/parameters/maxconn
cat /sys/module/sxuptp/parameters/maxconn
```

This modifies the registration budget, not existing USB registrations. Restart
`/usr/sbin/sxdevd` afterward so its cached budget is refreshed and previously
rejected devices can be enumerated. The sysfs write alone does not persist
across reboot. To revert a runtime experiment, first unplug the added devices
until `numconn` is at most 15; lowering `maxconn` alone does not evict devices
already registered. The persistent startup patch must also be removed to keep
the lower value after reboot.

SSH was temporarily enabled using the authenticated native diagnostics page
`/private/mainte/debug_main.htm`, then disabled again after installation.
No SSH watcher, saved remote administrator password, or SSH dependency is
required by the capacity patch or the Linux connection manager.

## Persistent patch and build

The reviewable patch is
[`packaging/firmware/ds600-1.5.0-maxconn32.patch`](../packaging/firmware/ds600-1.5.0-maxconn32.patch).
It sets `MAXCONN=32` and passes `maxconn=${MAXCONN}` to `insmod sxuptp.ko`.
Changing the existing, unused `MAXCONN` declaration alone would have no effect.
The vendor kernel module remains byte-for-byte unchanged.

The offline builder pins the vendor input hash, verifies each data record,
extracts the filesystem with its original ownership and device nodes, applies
the patch, and repacks it as gzip SquashFS 4.0. It re-extracts its result and
compares all 1,311 entries, requiring only `etc/init.d/S90svu` contents to differ
and preserving that file's permissions, ownership and timestamp. Root is needed
for filesystem metadata, not for network access. It never contacts or flashes
a device. Dependencies are Python 3, squashfs-tools, patch and bash.

```sh
sudo python3 packaging/firmware/build-ds600-rootfs.py \
  dist/ds600-recovery/DS-600-1.5.0-stock.bin \
  /tmp/DS-600-1.5.0-maxconn32-rootfs.squashfs
```

The installed artifact is `dist/DS-600-1.5.0-maxconn32-rootfs.squashfs`,
9,601,024 bytes, SHA-256:

```
739c745e0fca91fbf4a3d883e75b500efa42f72bbc27d4220c56404753e0604f
```

An independent build with the supplied builder produced the same hash. The
builder refuses unknown vendor images and existing output files. The local
filesystem creation time is fixed for reproducibility; individual vendor file
timestamps are retained. It emits a JSON validation report alongside its image.

This artifact is **not a web-uploadable firmware container**. No capacity patch
is applied by normal OpenSilexLink package installation.

## Installation and verification

The official, unmodified 1.5.0 image was installed through the native web updater
first. It changed the flash layout: rootfs is now `0x980000` bytes, followed by
a `0x280000` log partition. The custom rootfs fits this smaller partition.
The bootloader, kernel and device tree were not rewritten by the capacity patch.

The complete custom rootfs was transferred to the device, its SHA-256 checked,
and staged as `/tmp/firmware/rootfs`. The native shutdown updater in
`/etc/init.d/rcK` then ran `sxcpsync -v` against the rootfs MTD partition and
rebooted. The resulting flash prefix was read back and matched the image hash.
Use a complete temporary file and checksum verification before staging anything
under `/tmp/firmware`; a later reboot writes files found there to flash.

After the patched boot, firmware version was 1.5.0, `maxconn` was 32, `numconn`
was 18, and the current `sxdevd` log reported a budget of 32. These values were
observed before any post-boot sysfs write. Offline checks also exercised the
actual ARM capacity comparison in six boundary/accounting cases and captured
the startup module argument using stub commands. The owner confirmed keyboard
input during the earlier runtime experiment.

SSH was disabled in saved configuration, followed by another normal restart
to check enumeration without remote administration. Subsequent client/device
checks are recorded in [hardware validation](ds600.md). Long-running operation
with more peripherals and the full 32-interface budget remains untested.

## Container limitation

The vendor update container was decoded into a device tree, kernel and SquashFS
filesystem. Its data records have verifiable 16-bit payload sums and rotating
XOR encoding. The downloaded file also contains a terminal record and two-byte
trailer whose complete meaning has not been established. We did not manufacture
a replacement container. Using the unchanged vendor image for the base upgrade
and its native rootfs staging path for the small patch avoids guessing those
fields. The web upload flow invokes reboot and flash writes; uploading is not
a safe way to test a candidate container.
