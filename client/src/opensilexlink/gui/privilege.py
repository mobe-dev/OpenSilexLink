"""Explicit sudo fallback for the installed desktop client."""
from __future__ import annotations

import os
import socket
import subprocess

from PySide6.QtWidgets import QApplication, QInputDialog, QLineEdit, QMessageBox

from .. import ipc
from ..i18n import _

ASKPASS = "/usr/libexec/opensilexlink-askpass"
GUI = "/usr/bin/osl-gui"
_DISPLAY_ENV = ("DISPLAY", "WAYLAND_DISPLAY", "XDG_RUNTIME_DIR",
                "XAUTHORITY", "DBUS_SESSION_BUS_ADDRESS", "LANG", "LC_ALL")


def needs_privilege(address) -> bool:
    """Check real socket permissions, including ACLs and active session groups.

    A stopped service is a service error, not a reason to request a password.
    Development sockets never cause an administrator relaunch.
    """
    if os.geteuid() == 0 or address != ipc.SYSTEM_SOCKET:
        return False
    try:
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as probe:
            probe.connect(address)
    except PermissionError:
        return True
    except OSError:
        pass
    return False


def sudo_command(environ) -> tuple[list[str], dict[str, str]]:
    """Pass display access only; root ignores Python, loader and IPC overrides."""
    display = {key: environ[key] for key in _DISPLAY_ENV if environ.get(key)}
    base = {"PATH": "/usr/sbin:/usr/bin:/sbin:/bin", "HOME": "/root"}
    command = ["/usr/bin/sudo", "-A", "--", "/usr/bin/env", "-i"]
    command += [f"{key}={value}" for key, value in {**base, **display}.items()]
    command += [GUI, "--privileged"]
    # The helper runs as the requesting user. Keep its normal home directory.
    helper_env = {**base, **display, "HOME": environ.get("HOME", "/"),
                  "SUDO_ASKPASS": ASKPASS}
    return command, helper_env


def offer_privileged_launch(parent=None) -> int:
    box = QMessageBox(parent)
    box.setWindowTitle(_("OpenSilexLink — access required"))
    box.setIcon(QMessageBox.Icon.Question)
    box.setText(_("Your current session cannot access the OpenSilexLink service."))
    box.setInformativeText(_(
        "Join the opensilexlink group and log in again for normal access, "
        "or run this window as administrator using your sudo password. "
        "Automatic application launches are disabled in administrator mode."))
    run = box.addButton(_("Run as administrator"), QMessageBox.ButtonRole.AcceptRole)
    cancel = box.addButton(QMessageBox.StandardButton.Cancel)
    box.setDefaultButton(cancel)
    box.exec()
    if box.clickedButton() != run:
        return 0
    command, environ = sudo_command(os.environ)
    try:
        result = subprocess.run(command, env=environ, cwd="/", stdin=subprocess.DEVNULL,
                                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        if result.returncode == 0:
            return 0
    except OSError:
        pass
    QMessageBox.warning(parent, _("OpenSilexLink — administrator launch failed"), _(
        "Sudo authentication was cancelled or failed, or the administrator window "
        "could not start. Your account must be allowed to use sudo. "
        "You can also join the opensilexlink group and log in again."))
    return 1


def askpass_main() -> int:
    """sudo's helper: password travels only over its private stdout pipe."""
    app = QApplication([])
    app.setApplicationName("OpenSilexLink")
    password, accepted = QInputDialog.getText(
        None, _("OpenSilexLink — sudo authentication"),
        _("Enter your Linux account password to run OpenSilexLink as administrator:"),
        QLineEdit.EchoMode.Password)
    if not accepted:
        return 1
    print(password, flush=True)
    return 0
