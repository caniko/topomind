"""FreeCAD requires this loader; all workbench behavior is implemented in Rust."""

try:
    from . import SemanticMCP_native
except ImportError:  # FreeCAD may load InitGui.py as a top-level module.
    import SemanticMCP_native

SemanticMCP_native.install()
