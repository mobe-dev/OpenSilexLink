#!/bin/bash
# SPDX-License-Identifier: MIT
# Pinned CI build tool; verify before installing.
set -euo pipefail
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
curl -fL --retry 3 https://github.com/goreleaser/nfpm/releases/download/v2.45.0/nfpm_2.45.0_amd64.deb -o "$work/nfpm.deb"
printf '%s  %s\n' 60cacba86c0e780115d13be53c44b7c2b5dd95d511c4d3e6f41bb449d6afe2cc "$work/nfpm.deb" | sha256sum -c -
sudo dpkg -i "$work/nfpm.deb"
