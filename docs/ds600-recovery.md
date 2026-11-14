# DS-600 rollback and recovery

Target: DS-600 `84:25:3f:3f:64:23`, `192.168.178.150`.
Recovery artifacts are in `dist/ds600-recovery/` in this checkout. That directory
is private because the flash/configuration backups can contain credentials.
Copy it to another machine before relying on this host as the only recovery
source. `backup-manifest.json` records the original 1.4.4 files' sizes and SHA-256
hashes; `stock-1.5.0-rootfs-backup.json` records the later stock 1.5.0 rootfs.

## If the web interface still works

Open `http://192.168.178.150`, log in with the device's administrator password,
and select **Maintenance → Firmware Update**. Upload:

```
dist/ds600-recovery/DS-600-1.5.0-stock.bin
```

Its SHA-256 is:

```
3e210fba10e042d132625959643092fd1183a6bebc6e5995fac9f5157101e9b5
```

This is the complete, unmodified [vendor 1.5.0 firmware](https://www.silextechnology.com/connectivity-solutions/device-connectivity/ds-600).
The vendor's update procedure is saved alongside it as
`ds600_firmupguide_eng_xb.pdf`. Allow several minutes for writing and the
automatic reboot; keep power connected throughout. Confirm version **1.5.0**
afterward. Reinstalling this image removes the capacity patch and restores the
15-interface default. USB devices disconnect during the update.

The file `dist/DS-600-1.5.0-maxconn32-rootfs.squashfs` is only a root filesystem;
**it is not a web-uploadable firmware file**. Do not submit it to that form.

## If only the network configuration or administrator login is lost

The [vendor setup guide](https://www.silextechnology.com/hubfs/Resource%20PDF/ds600_setupguide_eng_xc.pdf)
describes factory configuration reset: disconnect power, hold the push switch
while reconnecting power, and release when the orange LINK LED turns on.
Find the device again by MAC address because DHCP may assign another IP.
This resets configuration; it does not restore an overwritten root filesystem
or remove the capacity patch.

## If Linux does not boot

There is no tested network-only recovery for a broken root filesystem. Physical
console access is required for the following fallback. UART pad location and
electrical levels have not been verified on this particular unit; identify them
from suitable hardware documentation or measurements before connecting an
adapter. This recovery procedure has not been exercised on physical hardware.

The backed-up bootloader is Marvell U-Boot 2013.01. Its boot environment uses
`ttyS0,115200`, `loadaddr=0x02000000`, and SPI bus/chip-select **1:0**.
`bootdelay` was changed from 0 to **3** and read back, providing an opportunity
to interrupt autoboot. Bootloader machine code was not modified.

The binary includes `tftpboot`, `crc32`, and the SPI `sf probe/read/erase/write`
commands. At an accessible U-Boot prompt, configure a temporary TFTP server on
the same isolated Ethernet segment containing **only the stock 1.5.0 rootfs
backup** needed below. Confirm `help sf`, `help tftpboot`, and `printenv loadaddr`
on the actual console before proceeding. Current upstream
[SPI command documentation](https://docs.u-boot.org/en/latest/usage/cmd/sf.html)
is a syntax reference, not proof that every option exists in this old build.

The saved `mtd6-rootfs-stock-1.5.0.bin` is exactly **9,961,472 bytes** (`0x980000`),
SHA-256 `69c746903a0a4349d3691ebccfacb77c0ef7766681970648aeaa4b012f8ec327`,
CRC32 **cbc27fb3**. For the observed 1.5.0 layout only, with the TFTP server at
`192.168.178.40`, the intended console sequence is:

```text
setenv ipaddr 192.168.178.150
setenv serverip 192.168.178.40
tftpboot 0x02000000 mtd6-rootfs-stock-1.5.0.bin
printenv filesize
crc32 0x02000000 0x980000
```

Stop unless the transfer size is `980000` and the CRC is `cbc27fb3`. Only after
those checks, restore the matching rootfs partition:

```text
sf probe 1:0
sf erase 0x400000 0x980000
sf write 0x02000000 0x400000 0x980000
sf read 0x03000000 0x400000 0x980000
crc32 0x03000000 0x980000
```

Readback must again produce **cbc27fb3** before `reset`. These commands leave
the bootloader, environment, configuration, device tree, kernel and log partition
alone. Do not use the 12 MiB **1.4.4** raw rootfs backup with these offsets and
lengths: 1.5.0 split that area into a smaller rootfs and a separate log partition.
If the bootloader cannot be reached or its commands/layout differ, stop rather
than guessing offsets; hardware recovery or vendor service is then necessary.
