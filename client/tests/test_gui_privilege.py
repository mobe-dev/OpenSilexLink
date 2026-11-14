"""Exercise authentication boundaries without asking for a real password."""
import os
import subprocess
from types import SimpleNamespace

os.environ.setdefault("QT_QPA_PLATFORM", "offscreen")
import pytest

pytest.importorskip("PySide6.QtWidgets")
from PySide6.QtWidgets import QApplication, QMessageBox, QInputDialog, QLineEdit
from opensilexlink import ipc
from opensilexlink.gui import privilege


@pytest.fixture
def app():
    return QApplication.instance() or QApplication([])


@pytest.mark.parametrize("error,expected", [
    (None, False), (PermissionError("denied"), True),
    (FileNotFoundError("stopped"), False), (ConnectionRefusedError(), False)])
def test_only_permission_denied_requests_sudo(monkeypatch, error, expected):
    monkeypatch.setattr(os, "geteuid", lambda: 1000)
    class Probe:
        def __enter__(self): return self
        def __exit__(self, *args): pass
        def connect(self, address):
            assert address == ipc.SYSTEM_SOCKET
            if error: raise error
    monkeypatch.setattr(privilege.socket, "socket", lambda *a: Probe())
    assert privilege.needs_privilege(ipc.SYSTEM_SOCKET) is expected
    assert not privilege.needs_privilege("/tmp/development.sock")
    assert not privilege.needs_privilege(("127.0.0.1", 9000))
    monkeypatch.setattr(os, "geteuid", lambda: 0)
    assert not privilege.needs_privilege(ipc.SYSTEM_SOCKET)


def test_sudo_uses_fixed_installed_program_and_clean_environment():
    command, env = privilege.sudo_command({
        "DISPLAY": ":0", "WAYLAND_DISPLAY": "wayland-0", "HOME": "/home/test",
        "PYTHONPATH": "/tmp/evil", "LD_PRELOAD": "/tmp/evil.so",
        "QT_PLUGIN_PATH": "/tmp/plugins", "SILEX_SOCKET": "/tmp/evil.sock",
        "SILEX_IPC": "/tmp/evil.sock", "SUDO_ASKPASS": "/tmp/evil"})
    assert command[:5] == ["/usr/bin/sudo", "-A", "--", "/usr/bin/env", "-i"]
    assert command[-2:] == ["/usr/bin/osl-gui", "--privileged"]
    assert "HOME=/root" in command and "DISPLAY=:0" in command
    assert env["HOME"] == "/home/test"
    assert env["SUDO_ASKPASS"] == privilege.ASKPASS
    assert not any("evil" in argument for argument in command)
    assert not any("evil" in value for value in env.values())
    assert "QT_PLUGIN_PATH" not in env


def choose_button(monkeypatch, accept):
    monkeypatch.setattr(QMessageBox, "exec", lambda self: 0)
    def clicked(self):
        return next(button for button in self.buttons()
                    if (self.buttonRole(button) == QMessageBox.ButtonRole.AcceptRole) == accept)
    monkeypatch.setattr(QMessageBox, "clickedButton", clicked)


def test_cancel_does_not_invoke_sudo(app, monkeypatch):
    choose_button(monkeypatch, False)
    monkeypatch.setattr(subprocess, "run", lambda *a, **k: pytest.fail("cancel must not run sudo"))
    assert privilege.offer_privileged_launch() == 0


@pytest.mark.parametrize("code", [0, 1])
def test_explicit_administrator_choice_and_auth_failure(app, monkeypatch, code):
    choose_button(monkeypatch, True)
    calls, warnings = [], []
    def run(command, **kwargs):
        calls.append((command, kwargs))
        return SimpleNamespace(returncode=code)
    monkeypatch.setattr(subprocess, "run", run)
    monkeypatch.setattr(QMessageBox, "warning", lambda *args: warnings.append(args))
    assert privilege.offer_privileged_launch() == code
    assert len(calls) == 1
    assert calls[0][1]["cwd"] == "/"
    assert calls[0][1]["stdin"] == subprocess.DEVNULL
    assert bool(warnings) == bool(code)


@pytest.mark.parametrize("accepted", [True, False])
def test_askpass_password_echo_and_cancel(app, monkeypatch, capsys, accepted):
    monkeypatch.setattr(privilege, "QApplication", lambda *a: app)
    def password(*args):
        assert args[-1] == QLineEdit.EchoMode.Password
        return "test-secret", accepted
    monkeypatch.setattr(QInputDialog, "getText", password)
    assert privilege.askpass_main() == (0 if accepted else 1)
    output = capsys.readouterr()
    assert output.out == ("test-secret\n" if accepted else "")
    assert output.err == ""


def test_root_window_ignores_socket_override_and_never_launches_apps(app, monkeypatch):
    from opensilexlink.gui.app import MainWindow
    monkeypatch.setattr(os, "geteuid", lambda: 0)
    monkeypatch.setattr(MainWindow, "_start", lambda self: None)
    monkeypatch.setattr(MainWindow, "_build_tray", lambda self: None)
    monkeypatch.setattr(subprocess, "Popen", lambda *a, **k: pytest.fail("root must not launch configured apps"))
    window = MainWindow(address="/tmp/evil.sock")
    window._health_timer.stop()
    try:
        assert window._address == ipc.SYSTEM_SOCKET
        assert "Administrator mode" in window.windowTitle()
        window._apply_settings_cache({"hide_on_close": True})
        assert not window._hide_on_close
        window._launch_app({"mac": "test", "dev_id": "test", "app": "evil"})
        window._handle_app_launch([])
    finally:
        window._quitting = True
        window.close()
