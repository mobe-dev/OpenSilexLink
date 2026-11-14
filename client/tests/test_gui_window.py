"""Desktop behavior with real Qt widgets, using the offscreen platform."""
import os

os.environ.setdefault("QT_QPA_PLATFORM", "offscreen")

import pytest

pytest.importorskip("PySide6.QtWidgets")
from PySide6.QtWidgets import QApplication
from opensilexlink import ipc
from opensilexlink.gui.app import MainWindow


@pytest.fixture
def window(monkeypatch):
    app = QApplication.instance() or QApplication([])
    monkeypatch.setattr(MainWindow, "_start", lambda self: None)
    monkeypatch.setattr(MainWindow, "_build_tray", lambda self: None)
    widget = MainWindow(address=("127.0.0.1", 1))
    widget._health_timer.stop()
    widget._on_op_done("daemon", True, "Connected")
    yield widget
    widget._quitting = True
    widget.close()
    app.processEvents()


def devices():
    return [{"ip": "192.0.2.1", "mac": "00:11:22:33:44:55", "model": "DS-600",
             "hostname": "test-server", "devices": [
                 {"dev_id": "usb5-002:0", "key": "0781:5580:SanDisk Extreme", "description": "SanDisk Extreme", "status": "connected",
                  "id_vendor": 0x0781, "id_product": 0x5580, "speed": "SuperSpeed"},
                 {"dev_id": "usb1-002:0", "key": "04b4:f901:Bluetooth", "description": "Bluetooth", "status": "available",
                  "id_vendor": 0x04b4, "id_product": 0xf901}]}]


def test_live_device_summary_selection_and_connection_controls(window):
    window._render(devices())
    assert "2 USB devices" in window.summary.text()
    assert "1 connected" in window.summary.text()
    assert window._select_device_in_tree("00:11:22:33:44:55", "usb5-002:0")
    assert window.btn_disconnect.isEnabled()
    assert not window.btn_connect.isEnabled()
    assert "0781:5580" in window.details.text()
    assert window.btn_web.isEnabled()
    window._select_device_in_tree("00:11:22:33:44:55", "usb1-002:0")
    assert window.btn_connect.isEnabled()
    assert not window.btn_disconnect.isEnabled()


def test_protected_usb_port_explains_why_connect_is_disabled(window):
    servers = devices()
    servers[0]["devices"][1]["password_required"] = True
    window._render(servers)
    window._select_device_in_tree("00:11:22:33:44:55", "usb1-002:0")
    assert not window.btn_connect.isEnabled()
    assert "password required" in window.details.text()


def test_lost_service_disables_controls_and_empty_list_explains_next_step(window, monkeypatch):
    window._render(devices())
    window._select_device_in_tree("00:11:22:33:44:55", "usb5-002:0")
    window._on_daemon_lost()
    assert not window.btn_disconnect.isEnabled()
    assert "offline" in window.service_status.text()
    window._render([])
    assert "Add a server" in window.summary.text()


def test_installed_service_failure_does_not_spawn_competing_daemon(window, monkeypatch):
    window._address = ipc.SYSTEM_SOCKET
    class FailedClient:
        def connect(self):
            raise PermissionError("Permission denied")
    monkeypatch.setattr(window, "_make_client", FailedClient)
    def fail_spawn(*args, **kwargs):
        pytest.fail("system service must not spawn a private daemon")
    monkeypatch.setattr("subprocess.Popen", fail_spawn)
    assert not window._ensure_daemon()
    assert "opensilexlink group" in window._daemon_error


def test_server_row_opens_settings_and_updates_do_not_enable_busy_buttons(window):
    window._render(devices())
    window.tree.setCurrentItem(window.tree.topLevelItem(0))
    assert window.btn_web.isEnabled()
    assert not window.btn_connect.isEnabled()
    window._select_device_in_tree("00:11:22:33:44:55", "usb5-002:0")
    window._operation_pending = True
    window._render(devices())
    assert not window.btn_disconnect.isEnabled()


def test_connection_preferences_are_visible_and_match_selected_device(window):
    servers = devices()
    servers[0]["devices"][0].update(auto_connect=True, reconnect_on_error=False)
    window._system_service = True
    window._render(servers)
    window._select_device_in_tree("00:11:22:33:44:55", "usb5-002:0")
    assert window.cb_restart.isEnabled() and window.cb_restart.isChecked()
    assert window.cb_loss.isEnabled() and not window.cb_loss.isChecked()
    servers[0]["devices"][0]["status"] = "reconnecting"
    window._render(servers)
    assert window.btn_disconnect.isEnabled()
    assert not window.btn_connect.isEnabled()


def test_device_properties_expose_startup_and_loss_recovery_separately(window):
    from opensilexlink.gui.options import DevicePropertiesDialog
    server = devices()[0]
    dialog = DevicePropertiesDialog(server["devices"][0], server,
        {"auto_connect":True, "reconnect_on_error":False}, window)
    assert 'before login' in dialog.cb_auto.text()
    assert 'unexpected disconnect' in dialog.cb_reconnect.text()
    assert dialog.result_rule()['auto_connect'] is True
    assert dialog.result_rule()['reconnect_on_error'] is False
    dialog.close()


def test_main_window_saves_both_connection_preferences_through_ipc(window):
    import time
    servers = devices()
    saved = []
    class Client:
        def set_device_rule(self, patch):
            saved.append(patch)
            servers[0]['devices'][0].update({k:v for k,v in patch.items() if k not in ('mac','key')})
            window._bridge.servers.emit(servers)
    window.client = Client()
    window._system_service = True
    window._render(servers)
    window._select_device_in_tree('00:11:22:33:44:55', 'usb5-002:0')
    for checkbox in (window.cb_restart, window.cb_loss):
        checkbox.click()
        deadline = time.monotonic() + 2
        while window._policy_pending and time.monotonic() < deadline:
            QApplication.processEvents()
            time.sleep(0.005)
        assert not window._policy_pending
    assert saved == [
        {'mac':'00:11:22:33:44:55', 'key':'0781:5580:SanDisk Extreme', 'auto_connect':True},
        {'mac':'00:11:22:33:44:55', 'key':'0781:5580:SanDisk Extreme', 'reconnect_on_error':False}]
    window.client = None


def test_bind_all_is_saved_for_selected_server_without_render_side_effects(window):
    import time
    servers = devices()
    saved = []
    class Client:
        def set_bind_all(self, mac, enabled):
            saved.append((mac, enabled))
            servers[0]['bind_all'] = enabled
            for device in servers[0]['devices']:
                device['auto_connect'] = enabled
            window._bridge.servers.emit(servers)
        def close(self):
            pass
    window.client = Client()
    window._system_service = True
    window._render(servers)
    window.tree.setCurrentItem(window.tree.topLevelItem(0))
    assert window.cb_bind_all.isEnabled()
    assert not window.cb_bind_all.isChecked()
    window.cb_bind_all.click()
    deadline = time.monotonic() + 2
    while window._policy_pending and time.monotonic() < deadline:
        QApplication.processEvents()
        time.sleep(0.005)
    assert not window._policy_pending
    assert window.cb_bind_all.isChecked()
    window._select_device_in_tree('00:11:22:33:44:55', 'usb5-002:0')
    assert window.cb_bind_all.isChecked()
    assert window.cb_restart.isChecked() and not window.cb_restart.isEnabled()
    assert window.cb_loss.isEnabled()
    window._render(servers)
    assert saved == [('00:11:22:33:44:55', True)]
    window._on_daemon_lost()
    assert not window.cb_bind_all.isEnabled()
    window.client = None


def test_bind_all_preserves_underlying_device_startup_rule_in_properties(window):
    from opensilexlink.gui.options import DevicePropertiesDialog
    server = devices()[0]
    server['bind_all'] = True
    dialog = DevicePropertiesDialog(server['devices'][0], server,
        {'auto_connect': False}, window, system_service=True)
    assert dialog.cb_auto.isChecked() and not dialog.cb_auto.isEnabled()
    assert dialog.result_rule()['auto_connect'] is False
    dialog.close()


def test_server_bind_setting_roundtrips_and_does_not_enable_other_servers():
    from opensilexlink.settings import Settings
    s = Settings.from_dict({'bind_all_servers': ['00:11:22:33:44:55']})
    assert s.should_auto_connect('00:11:22:33:44:55', 'new dock device')
    assert not s.should_auto_connect('00:11:22:33:44:66', 'new dock device')
    assert Settings.from_dict(s.to_dict()).bind_all_servers == s.bind_all_servers
