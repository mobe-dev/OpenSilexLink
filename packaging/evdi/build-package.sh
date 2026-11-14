#!/bin/bash
# SPDX-License-Identifier: MIT
# Build an optional, separately licensed EVDI workaround for the test host.
# Usage: packaging/evdi/build-package.sh [vendor.deb] [output-directory]
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
OUT=$(realpath -m "${2:-$ROOT/dist}")
BUILD=$(mktemp -d)
trap 'rm -rf "$BUILD"' EXIT
BASE=${1:-$BUILD/evdi-vendor.deb}
if [ $# -eq 0 ]; then
    curl -fL --retry 2 \
        https://www.synaptics.com/sites/default/files/Ubuntu/pool/stable/main/amd64/evdi-1.15.1-197_amd64.deb \
        -o "$BASE"
fi
printf '%s  %s\n' 0201848d1ad7283d28ca1ba9b81edf8581c07b89c5ad5302b773e615b1d0489a "$BASE" | sha256sum -c -
dpkg-deb -R "$BASE" "$BUILD/package"
MODULE="$BUILD/package/usr/src/evdi-1.15.1"
patch --batch --fuzz=0 -d "$MODULE" -p2 < "$ROOT/packaging/evdi/0001-upstream-gem-vmap.patch"
patch --batch --fuzz=0 -d "$MODULE" -p1 < "$ROOT/packaging/evdi/0002-independent-prime-mapping.patch"
python3 - "$BUILD/package" "$ROOT" <<'PY'
import hashlib
import re
import sys
from pathlib import Path
package, root = map(Path, sys.argv[1:])
source = package / 'usr/src/evdi-1.15.1'
for name in ('dkms.conf', 'dkms_install.sh'):
    path = source / name
    path.write_text(path.read_text().replace('1.15.1', '1.15.1+osl1'))
source.rename(source.with_name('evdi-1.15.1+osl1'))
for name in ('postinst', 'prerm'):
    path = package / 'DEBIAN' / name
    path.write_text(path.read_text().replace('1.15.1', '1.15.1+osl1'))
control = package / 'DEBIAN/control'
text = re.sub(r'^Version: .*$', 'Version: 1.15.1-197+osl1', control.read_text(), flags=re.M)
text = text.rstrip() + '\nX-Local-Build: OpenSilexLink PRIME mapping workaround\n'
control.write_text(text)
doc = package / 'usr/share/doc/evdi'
(doc / 'README.local').write_text((root / 'packaging/evdi/README.txt').read_text())
for path in (root / 'packaging/evdi').glob('*.patch'):
    (doc / path.name).write_bytes(path.read_bytes())
(doc / 'COPYING.GPL-2').write_bytes(Path('/usr/share/common-licenses/GPL-2').read_bytes())
files = sorted(p for p in package.rglob('*') if p.is_file() and not p.is_symlink() and 'DEBIAN' not in p.relative_to(package).parts)
(package / 'DEBIAN/md5sums').write_text(''.join(
    f'{hashlib.md5(p.read_bytes()).hexdigest()}  {p.relative_to(package)}\n' for p in files))
PY
mkdir -p "$OUT"
dpkg-deb --root-owner-group --build "$BUILD/package" "$OUT/evdi_1.15.1-197+osl1_amd64.deb"
echo "Built optional EVDI package; installation and module activation are separate steps."
