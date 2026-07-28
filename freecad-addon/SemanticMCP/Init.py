"""FreeCAD addon entrypoint; the workbench is optional until started by the user."""

from .bridge.server import FreeCADBridge


def create_bridge(**kwargs):
    """Create a bridge explicitly; importing the addon never opens a socket."""

    return FreeCADBridge(**kwargs)


def stop_bridge():
    from .ui.status import stop_bridge as stop

    stop()
