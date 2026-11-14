# Publication and license review

Reviewed on 2026-11-14 for OpenSilexLink 1.1.0. This review covers the files
and release assets in this repository; it does not establish patent rights or
override agreements governing separately obtained vendor software.

| Component | License / publication status | Distribution requirements |
| --- | --- | --- |
| Original OpenSilexLink and application changes | MIT; source and application packages may be published | Retain `LICENSE`, including bytewarrior's copyright and permission notice. |
| Rust dependencies linked into the daemon | MIT / Apache-2.0 choices; Unicode data also Unicode-3.0 | Ship the generated `THIRD_PARTY_NOTICES.txt` in both packages and as a release asset. Build stops for new, unreviewed license identifiers or missing texts. |
| Rust standard library | Primarily MIT / Apache-2.0, with component exceptions | Rust 1.93.1 is pinned; its distribution copyright and license texts are retained in `LICENSES/Rust-1.93.1-copyright.txt` and the generated notices. Review notices when changing the compiler. |
| PySide6 / Qt | Separate open-source LGPL/GPL/commercial licensing | Loaded from distribution packages; no Qt/PySide6 libraries are bundled in our packages. Users can replace these runtime libraries. Follow Qt's applicable terms if distributing them yourself. |
| EVDI module patches | GPL-2.0-only; not covered by the root MIT license | Preserve attribution and distribute under GPL-2.0-only with `LICENSES/GPL-2.0-only.txt`. The first patch retains Kieran (TigerWolf)'s authorship and upstream commit reference. |
| Repacked optional EVDI `.deb` | **Excluded from public release** | The recipe retains a vendor LGPL-2.1-or-later userspace `libevdi` binary without its corresponding library source. Kernel module source alone does not cover this gap. Supply verified matching library source and build scripts before redistributing that binary. |
| DisplayLink Manager / SX Virtual Link | Vendor software; not included | Obtain separately from the vendor under its terms. |
| Silex stock or modified firmware / root filesystem images | **Redistribution permission not established; excluded** | Silex's GPL/LGPL notice applies to the covered components, not automatically the complete firmware. Obtain permission and satisfy all applicable source obligations before distributing images. |
| Firmware patch recipe | Locally written builder: MIT; minimal modification instructions supplied as source | Users supply their own vendor image. No Silex executable, kernel module, full script or firmware image is included. This is not a grant to redistribute vendor firmware. |
| Device backups, configuration, captures and keys | Private operational material | Keep out of Git and release assets. |

The root MIT license remains intact. It does not relicense the separately
licensed EVDI patches, third-party notices or vendor materials. Silex and
DisplayLink names identify compatibility; no vendor endorsement is claimed.

## Release boundary

Public binary release assets are **only** the OpenSilexLink `.deb`, `.rpm`,
`THIRD_PARTY_NOTICES.txt`, and their `SHA256SUMS`. GitHub also provides source
archives of the tagged repository. Workflows enumerate these assets explicitly;
they never upload `dist/*`. `dist/` is ignored and may contain private backups
or locally built firmware that must stay on the test host.

The notices generator reads the actual locked Linux Cargo dependency graph and
copies the crates' license/copyright/notice files. It includes all offered
license texts rather than stripping alternative license grants. The retained
Rust distribution notice is from Ubuntu's `libstd-rust-1.93`, version
`1.93.1+dfsg-0ubuntu6`; it conservatively includes toolchain components beyond
the standard library linked into the daemon.

No firmware binary is in the tracked source. Firmware testing and reverse
engineering were performed separately from the repository. The source review
found no incorporated vendor SDK or decompiled vendor program in the application.
This is a repository/license assessment, not a legal opinion on every
jurisdiction's interoperability or reverse-engineering rules.

## Primary references

- [Upstream OpenSilexLink license](https://github.com/bytewarrior/OpenSilexLink/blob/main/LICENSE)
- [MIT license terms](https://opensource.org/license/mit)
- [Qt for Python licensing](https://doc.qt.io/qtforpython-6/licenses.html)
- [EVDI library SPDX declaration](https://github.com/DisplayLink/evdi/blob/v1.15.1/library/evdi_lib.c)
- [EVDI upstream module patch and attribution](https://github.com/DisplayLink/evdi/pull/604)
- [GNU GPL v2](https://www.gnu.org/licenses/old-licenses/gpl-2.0.html)
- [GNU LGPL v2.1](https://www.gnu.org/licenses/old-licenses/lgpl-2.1.html)
- [Silex DS-600 firmware and component source notice](https://www.silextechnology.com/connectivity-solutions/device-connectivity/ds-600)
- [Rust copyright information](https://github.com/rust-lang/rust/blob/1.93.1/COPYRIGHT)
