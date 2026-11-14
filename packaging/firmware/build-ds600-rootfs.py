#!/usr/bin/env python3
"""Build and verify the pinned DS-600 1.5.0 capacity patch; never flash a device."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import stat
import struct
import subprocess
import tempfile


VENDOR_SHA256 = "3e210fba10e042d132625959643092fd1183a6bebc6e5995fac9f5157101e9b5"
SCRIPT_SHA256 = "e05d534bd443b631b5e650df7505e0fed56f7af3bb67b9395b2f00be305cf56a"
MODULE_SHA256 = "4bd9a0120a8cd5a30c3c0bb41f48440c63fdbe136f3a8f24f61e53201fad449c"
ROOTFS_OFFSET = 0x300000  # Offset in the decoded image, based at flash 0x100000.
PARTITION_BYTES = 0x980000  # Observed rootfs partition after official 1.5.0 boot.
IMAGE_TIME = 1721359293  # Fixed filesystem creation time of the installed local build.


def require(condition, message):
    if not condition:
        raise ValueError(message)


def sha256(data):
    return hashlib.sha256(data).hexdigest()


def decode_vendor(data):
    require(sha256(data) == VENDOR_SHA256, "Input is not the pinned official 1.5.0 image")
    position = int.from_bytes(data[4:6], "big")
    count = int.from_bytes(data[position - 4:position], "big")
    keys = bytes.fromhex("9fc3f3bc8acce5d7")
    records = []
    previous_end = 0x100000
    for expected_sequence in range(count):
        kind, sequence, address, size = struct.unpack_from(">HHIH", data, position)
        payload = data[position + 10:position + 10 + size]
        checksum = int.from_bytes(data[position + 10 + size:position + 12 + size], "big")
        require(kind == 3 and sequence == expected_sequence, "Unexpected vendor record")
        require(size <= 480 and len(payload) == size, "Invalid vendor record size")
        require(sum(payload) & 0xFFFF == checksum, "Vendor record checksum mismatch")
        if size:
            require(address >= previous_end, "Overlapping vendor records")
            require(any(start <= address and address + size <= end for start, end in
                        [(0x100000, 0x110000), (0x110000, 0x400000),
                         (0x400000, 0x400000 + PARTITION_BYTES)]), "Record exceeds partition")
            records.append((address, bytes(value ^ keys[sequence & 7] for value in payload)))
            previous_end = address + size
        position += size + 12
    decoded = bytearray(previous_end - 0x100000)
    for address, payload in records:
        decoded[address - 0x100000:address - 0x100000 + len(payload)] = payload
    require(decoded[ROOTFS_OFFSET:ROOTFS_OFFSET + 4] == b"hsqs", "SquashFS not found")
    return decoded


def inventory(root):
    entries = {}
    for directory, directories, files in os.walk(root, followlinks=False):
        paths = [Path(directory), *(Path(directory) / name for name in directories + files)]
        for path in paths:
            info = path.lstat()
            entry = {"mode": info.st_mode, "uid": info.st_uid, "gid": info.st_gid,
                     "mtime": int(info.st_mtime)}
            if stat.S_ISREG(info.st_mode):
                entry.update(size=info.st_size, sha256=sha256(path.read_bytes()))
            elif stat.S_ISLNK(info.st_mode):
                entry["target"] = os.readlink(path)
            elif stat.S_ISCHR(info.st_mode) or stat.S_ISBLK(info.st_mode):
                entry["device"] = info.st_rdev
            entries[str(path.relative_to(root))] = entry
    return entries


def run(*arguments, **kwargs):
    subprocess.run([str(argument) for argument in arguments], check=True, **kwargs)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("vendor_image", type=Path)
    parser.add_argument("output", type=Path, help="New SquashFS file; must not already exist")
    args = parser.parse_args()
    require(os.geteuid() == 0, "Run with sudo to preserve vendor ownership and device nodes")
    require(not args.output.exists(), "Output already exists; refusing to overwrite")
    args.output.parent.mkdir(parents=True, exist_ok=True)
    decoded = decode_vendor(args.vendor_image.read_bytes())
    patch = Path(__file__).with_name("ds600-1.5.0-maxconn32.patch")
    with tempfile.TemporaryDirectory(prefix="ds600-rootfs-") as temporary:
        work = Path(temporary)
        source = work / "decoded.bin"
        source.write_bytes(decoded)
        root = work / "rootfs"
        run("unsquashfs", "-no-progress", "-o", ROOTFS_OFFSET, "-d", root, source)
        before = inventory(root)
        script = root / "etc/init.d/S90svu"
        original = script.read_bytes()
        require(sha256(original) == SCRIPT_SHA256, "Unexpected vendor startup script")
        require(sha256((root / "lib/modules/sxuptp/sxuptp.ko").read_bytes()) == MODULE_SHA256,
                "Unexpected vendor driver")
        expected = original.replace(b"MAXCONN=15\n", b"MAXCONN=32\n").replace(
            b"\t\t${INSMOD} ${KMOD_DIR}/sxuptp.ko\n",
            b"\t\t${INSMOD} ${KMOD_DIR}/sxuptp.ko maxconn=${MAXCONN}\n")
        script_times = script.stat()
        directory_times = script.parent.stat()
        with patch.open("rb") as patch_input:
            run("patch", "--fuzz=0", "-p1", "-d", root, stdin=patch_input)
        require(script.read_bytes() == expected, "Patch changed more than the intended settings")
        os.utime(script, ns=(script_times.st_atime_ns, script_times.st_mtime_ns))
        os.utime(script.parent, ns=(directory_times.st_atime_ns, directory_times.st_mtime_ns))
        run("bash", "-n", script)
        run("mksquashfs", root, args.output, "-noappend", "-comp", "gzip", "-b", 131072,
            "-no-xattrs", "-mkfs-time", IMAGE_TIME, "-no-progress", "-processors", 2)
        require(args.output.stat().st_size <= PARTITION_BYTES, "Patched image exceeds rootfs partition")
        verified = work / "verified"
        run("unsquashfs", "-no-progress", "-d", verified, args.output)
        after = inventory(verified)
        require(before.keys() == after.keys(), "Filesystem entries changed")
        changed = [name for name in before if before[name] != after[name]]
        require(changed == ["etc/init.d/S90svu"], f"Unexpected filesystem changes: {changed}")
        for field in ("mode", "uid", "gid", "mtime"):
            require(before[changed[0]][field] == after[changed[0]][field], "Script metadata changed")
        require((verified / changed[0]).read_bytes() == expected, "Repacked script mismatch")
        report = {"base_version": "1.5.0", "vendor_sha256": VENDOR_SHA256,
                  "changed_files": changed, "filesystem_entries_verified": len(before),
                  "metadata_preserved": True, "kernel_module_unchanged": True,
                  "partition_bytes": PARTITION_BYTES, "image_bytes": args.output.stat().st_size,
                  "sha256": sha256(args.output.read_bytes()), "web_uploadable": False}
        args.output.with_suffix(args.output.suffix + ".json").write_text(json.dumps(report, indent=2) + "\n")
        print(json.dumps(report, indent=2))


if __name__ == "__main__":
    main()
