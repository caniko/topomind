import io
import pathlib
import sys
import tempfile
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "freecad-addon"))

from SemanticMCP.bridge.protocol import Pairing, read_frame, write_frame  # noqa: E402
from SemanticMCP.bridge.server import FreeCADBridge  # noqa: E402
from SemanticMCP.extractors.snapshot import FreeCADExtractor, RevisionBook  # noqa: E402
from SemanticMCP.observers.events import EventCoalescer  # noqa: E402
from SemanticMCP.operations.typed import TypedExecutor  # noqa: E402


class Vector:
    def __init__(self, x, y, z):
        self.x, self.y, self.z = x, y, z


class BoundBox:
    XMin, YMin, ZMin = 0.0, 0.0, 0.0
    XMax, YMax, ZMax = 10.0, 10.0, 10.0


class Cylinder:
    Radius = 3.0
    Axis = Vector(0, 0, 1)
    Center = Vector(0, 0, 0)


class Edge:
    Length = 10.0

    def isSame(self, other):
        return self is other


class Face:
    Surface = Cylinder()
    Orientation = "Reversed"
    Area = 100.0

    def __init__(self, edge):
        self.Edges = [edge]
        self.BoundBox = BoundBox()


class Shape:
    ShapeType = "Solid"
    BoundBox = BoundBox()
    Volume = 1000.0
    Area = 600.0
    Edges = [Edge()]
    Faces = [Face(Edges[0])]
    Wires = []
    Shells = []
    Solids = [object()]
    CompSolids = []
    Vertexes = []

    def isNull(self):
        return False

    def isValid(self):
        return True


class ViewObject:
    Visibility = True


class Object:
    Name = "Body"
    Label = "Body"
    TypeId = "PartDesign::Feature"
    PropertiesList = ["Length"]
    Length = 10.0
    Shape = Shape()
    ViewObject = ViewObject()
    OutList = []
    InList = []
    State = []


class Document:
    Name = "Demo"
    Label = "Demo"

    def __init__(self):
        self.Objects = [Object()]
        self._transaction = False
        self.recomputed = False

    def recompute(self):
        self.recomputed = True

    def openTransaction(self, _name):
        self._transaction = True

    def abortTransaction(self):
        self._transaction = False

    def commitTransaction(self):
        self._transaction = False

    def getObject(self, name):
        return next((obj for obj in self.Objects if obj.Name == name), None)


class App:
    def __init__(self):
        self.ActiveDocument = Document()
        self.Documents = {self.ActiveDocument.Name: self.ActiveDocument}

    def Version(self):
        return (1, 1, 3)


class Selection:
    def clearSelection(self):
        pass

    def addSelection(self, *_args):
        pass


class Gui:
    Selection = Selection()


class BridgeTests(unittest.TestCase):
    def test_frames_and_pairing(self):
        data = io.BytesIO()
        write_frame(data, {"message": "hello"})
        data.seek(0)
        self.assertEqual(read_frame(data)["message"], "hello")
        with tempfile.TemporaryDirectory() as directory:
            pairing = Pairing(pathlib.Path(directory) / "secret", b"x" * 32)
            pairing.write()
            self.assertTrue(pairing.verify("nonce", pairing.proof("nonce")))
            self.assertFalse(pairing.verify("nonce", "bad"))

    def test_extractor_preserves_exact_cylinder_evidence(self):
        app = App()
        snapshot = FreeCADExtractor(app).snapshot("s", RevisionBook("e"))
        faces = [entity for entity in snapshot["entities"] if entity["kind"] == "cad.face"]
        self.assertEqual(len(faces), 1)
        self.assertEqual(faces[0]["properties"]["geometry"]["surface_type"], "cylinder")
        self.assertEqual(faces[0]["properties"]["geometry"]["radius"], 3.0)

    def test_typed_property_write_is_allowlisted(self):
        app = App()
        executor = TypedExecutor(app)
        target = "fc://session/s/document/Demo/object/Body@g"
        result = executor.apply({"document": "Demo", "operations": [{"op": "set_property", "target": target, "property": "Length", "value": 12.0}], "validate": ["recompute"]}, commit=False)
        self.assertEqual(result.changed, [target])
        with self.assertRaises(Exception):
            executor.apply({"document": "Demo", "operations": [{"op": "set_property", "target": target, "property": "__class__", "value": 12.0}]}, commit=False)

    def test_event_coalescer_keeps_last_event_by_class(self):
        coalescer = EventCoalescer(debounce_ms=1)
        coalescer.push("view", "one")
        last = coalescer.push("view", "two")
        events = coalescer.flush(last.monotonic_ms + 1)
        self.assertEqual(events[0].object_name, "two")

    def test_bridge_authentication_does_not_open_socket_on_import(self):
        with tempfile.TemporaryDirectory() as directory:
            bridge = FreeCADBridge(app=App(), gui=Gui(), runtime_root=directory)
            bridge.rendezvous.pairing.write()
            nonce = "nonce"
            request = {"message": "bridge.authenticate", "request_id": "1", "payload": {"nonce": nonce, "proof": bridge.rendezvous.pairing.proof(nonce)}}
            response, authenticated = bridge.handle_request(request, False)
            self.assertTrue(authenticated)
            self.assertEqual(response["status"], "ok")


if __name__ == "__main__":
    unittest.main()
