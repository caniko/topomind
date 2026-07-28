import io
import pathlib
import sys
import tempfile
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "freecad-addon"))

from SemanticMCP.bridge.protocol import MAX_FRAME, Pairing, ProtocolError, read_frame  # noqa: E402
from SemanticMCP.bridge.server import FreeCADBridge  # noqa: E402


class App:
    Documents = {}
    ActiveDocument = None

    def Version(self):
        return (1, 1, 2)


class SecurityTests(unittest.TestCase):
    def test_oversized_and_truncated_frames_fail_closed(self):
        with self.assertRaises(ProtocolError):
            read_frame(io.BytesIO((MAX_FRAME + 1).to_bytes(4, "big")))
        with self.assertRaises(ProtocolError):
            read_frame(io.BytesIO((4).to_bytes(4, "big") + b"{}"))

    def test_secret_permissions_are_private(self):
        with tempfile.TemporaryDirectory() as directory:
            path = pathlib.Path(directory) / "secret"
            Pairing(path, b"x" * 32).write()
            self.assertEqual(path.stat().st_mode & 0o777, 0o600)

    def test_old_freecad_is_advertised_read_only(self):
        bridge = FreeCADBridge(app=App(), gui=None, runtime_root=tempfile.mkdtemp())
        hello = bridge.hello()
        self.assertEqual(hello["compatibility"], "read_only_below_design_baseline")
        self.assertNotIn("write_model_low_risk", hello["capabilities"])


if __name__ == "__main__":
    unittest.main()
