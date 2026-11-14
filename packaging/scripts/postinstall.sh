#!/bin/sh
set -eu
if ! getent group opensilexlink >/dev/null; then
    groupadd --system opensilexlink
fi
if [ -d /run/systemd/system ]; then
    systemctl daemon-reload
    # Start on first install. Upgrades preserve an explicitly stopped service.
    if [ -e /var/lib/opensilexlink/.installed ]; then
        if systemctl is-active --quiet opensilexlink.service; then
            echo 'OpenSilexLink updated. Unmount USB storage before restarting opensilexlink.service.'
        fi
    else
        systemctl enable --now opensilexlink.service
    fi
fi
install -d -m 0700 /var/lib/opensilexlink
touch /var/lib/opensilexlink/.installed
