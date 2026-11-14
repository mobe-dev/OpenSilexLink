#!/bin/sh
set -eu
# Debian passes remove/upgrade; RPM passes the number of installed versions.
case "${1:-}" in
    remove|0)
        if [ -d /run/systemd/system ]; then
            if systemctl is-active --quiet opensilexlink.service; then
                /usr/bin/osl ipc prepare_shutdown >/dev/null || {
                    echo 'Unmount USB storage before removing OpenSilexLink.' >&2
                    exit 1
                }
            fi
            systemctl stop opensilexlink.service
            systemctl disable opensilexlink.service
        fi
        ;;
esac
