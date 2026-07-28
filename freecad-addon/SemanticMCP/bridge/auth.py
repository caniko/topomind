"""Authentication and process-local rendezvous metadata."""

from __future__ import annotations

import json
import os
import time
import uuid
from pathlib import Path
from typing import Any

from .protocol import Pairing


class Rendezvous:
    def __init__(self, root: str | os.PathLike[str] | None = None) -> None:
        runtime = os.environ.get("XDG_RUNTIME_DIR")
        self.root = Path(root or runtime or "/tmp") / "topomind"
        self.root.mkdir(mode=0o700, parents=True, exist_ok=True)
        os.chmod(self.root, 0o700)
        self.session = f"s_{uuid.uuid4().hex}"
        self.epoch = f"e_{uuid.uuid4().hex}"
        self.socket = self.root / f"bridge-{self.session}.sock"
        self.secret_path = self.root / f"bridge-{self.session}.secret"
        self.record_path = self.root / f"bridge-{self.session}.json"
        self.pairing = Pairing(self.secret_path)

    def publish(self, hello: dict[str, Any], documents: list[str], endpoint: str | None = None) -> dict[str, Any]:
        self.pairing.write()
        record = {
            "session": self.session,
            "session_epoch": self.epoch,
            "endpoint": endpoint or str(self.socket),
            "secret_path": str(self.secret_path),
            "pid": os.getpid(),
            "started_at_ms": int(time.time() * 1000),
            "hello": hello,
            "documents": documents,
        }
        temporary = self.record_path.with_suffix(".tmp")
        with temporary.open("w", encoding="utf-8") as handle:
            os.chmod(temporary, 0o600)
            json.dump(record, handle, sort_keys=True, separators=(",", ":"))
            handle.flush()
            os.fsync(handle.fileno())
        os.replace(temporary, self.record_path)
        os.chmod(self.record_path, 0o600)
        return record

    def close(self) -> None:
        for path in (self.socket, self.record_path, self.secret_path):
            try:
                path.unlink()
            except FileNotFoundError:
                pass
