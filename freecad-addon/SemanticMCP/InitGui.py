"""Minimal FreeCAD GUI workbench registration."""

try:
    import FreeCADGui  # type: ignore
except ImportError:  # pragma: no cover - exercised only inside FreeCAD
    FreeCADGui = None


if FreeCADGui is not None:  # pragma: no cover - exercised only inside FreeCAD
    class SemanticMCPWorkbench:
        MenuText = "Topomind Semantic MCP"
        ToolTip = "Revisioned semantic CAD context and safe agentic editing"
        Icon = ""

        def Initialize(self):
            from .ui.status import command_list, register_commands

            register_commands(FreeCADGui)
            self.appendMenu("Topomind", command_list())

        def GetClassName(self):
            return "Gui::PythonWorkbench"

    FreeCADGui.addWorkbench(SemanticMCPWorkbench())
