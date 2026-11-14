Optional EVDI PRIME mapping workaround

Publication status: SOURCE PATCHES ONLY in the public release. Do not upload
the repacked vendor .deb: its retained libevdi binary is LGPL-2.1-or-later,
and this recipe does not include its matching corresponding library source.
See docs/licensing.md. These two module patches are GPL-2.0-only; the full
license text is in LICENSES/GPL-2.0-only.txt. The build recipe itself is MIT.

This is a local build of Synaptics EVDI 1.15.1-197. The kernel module is
GPL-2.0-only; its license is separate from OpenSilexLink's MIT license.
The complete patched module source is included under /usr/src, and the vendor
copyright file and upstream library binaries are retained in the package.

Base package SHA256:
0201848d1ad7283d28ca1ba9b81edf8581c07b89c5ad5302b773e615b1d0489a

Patch source: https://github.com/DisplayLink/evdi/pull/604
Commit: b877fbf74538dbf595324b83edc86d1057c2e179
Original author: Kieran (TigerWolf).

The first patch adds GEM vmap/vunmap callbacks. The second keeps those
importer mappings separate from the painter's cached object mapping, so an
export/import cycle cannot overwrite or unmap the painter's own mapping.
Page pins are balanced on success, failure and unmap.

This is an experimental workaround for a black display with a working cursor
on the Ubuntu GNOME/Wayland test VM. It is not a Synaptics production release.
It does not include the separate experimental vblank timer proposal.

Build from the repository with packaging/evdi/build-package.sh. The build
downloads and verifies the pinned vendor package, applies both patches and
creates a separate evdi Debian package. It does not install or activate it.

Build validation and hardware results are recorded in docs/displaylink.md.
Keep the original evdi-dkms and libevdi1 packages available for rollback.
