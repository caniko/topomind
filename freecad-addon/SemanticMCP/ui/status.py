"""Explicit FreeCAD commands for bridge lifecycle and pairing status."""

from __future__ import annotations

import json
from pathlib import Path
from typing import Any

_bridge: Any = None
_thread: Any = None


def start_bridge() -> str:
    global _bridge, _thread
    if _bridge is not None and _bridge.started:
        return str(_bridge.rendezvous.record_path)
    import FreeCAD  # type: ignore

    try:
        import FreeCADGui  # type: ignore
    except ImportError:
        FreeCADGui = None
    from ..bridge.server import FreeCADBridge

    _bridge = FreeCADBridge(app=FreeCAD, gui=FreeCADGui)
    _thread = _bridge.start_background()
    return str(_bridge.rendezvous.record_path)


def stop_bridge() -> None:
    global _bridge, _thread
    if _bridge is not None:
        _bridge.stop()
    if _thread is not None:
        _thread.join(timeout=2.0)
    _bridge = None
    _thread = None


def pairing_status() -> dict[str, Any]:
    if _bridge is None:
        return {"state": "stopped"}
    record = Path(_bridge.rendezvous.record_path)
    if not record.exists():
        return {"state": "starting", "record": str(record)}
    return {"state": "ready", **json.loads(record.read_text(encoding="utf-8"))}


def show_pairing() -> dict[str, Any]:
    status = pairing_status()
    try:
        import FreeCADGui  # type: ignore
        from PySide import QtWidgets  # type: ignore

        QtWidgets.QMessageBox.information(FreeCADGui.getMainWindow(), "Topomind pairing", json.dumps(status, indent=2, sort_keys=True))
    except ImportError:
        pass
    return status


def command_list():
    return ["Topomind_StartBridge", "Topomind_StopBridge", "Topomind_ShowPairing"]


def register_commands(gui: Any) -> None:
    class Start:
        def Activated(self):
            start_bridge()

        def GetResources(self):
            return {"MenuText": "Start bridge", "ToolTip": "Start the authenticated local Topomind bridge"}

    class Stop:
        def Activated(self):
            stop_bridge()

        def GetResources(self):
            return {"MenuText": "Stop bridge", "ToolTip": "Stop the Topomind bridge"}

    class Show:
        def Activated(self):
            show_pairing()

        def GetResources(self):
            return {"MenuText": "Show pairing status", "ToolTip": "Show local Topomind pairing status"}

    gui.addCommand("Topomind_StartBridge", Start())
    gui.addCommand("Topomind_StopBridge", Stop())
    gui.addCommand("Topomind_ShowPairing", Show())
