#!/bin/sh
set -eu
if [ -d /run/systemd/system ]; then
    systemctl daemon-reload
fi
# Preserve settings and group membership on removal for a later reinstall.
