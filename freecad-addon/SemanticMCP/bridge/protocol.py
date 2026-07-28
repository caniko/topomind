"""Schema-versioned length-prefixed local IPC and HMAC pairing."""

from __future__ import annotations

import hashlib
import hmac
import json
import os
import secrets
import struct
from pathlib import Path
from typing import BinaryIO, Any

IPC_VERSION = "ipc/1.0"
DTO_VERSION = "bridge-dto/1.0"
MAX_FRAME = 4 * 1024 * 1024


class ProtocolError(ValueError):
    """Malformed or unauthenticated protocol data."""


def canonical_json(value: Any) -> bytes:
    return json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False).encode("utf-8")


def sha256_json(value: Any) -> str:
    return "sha256:" + hashlib.sha256(canonical_json(value)).hexdigest()


def read_frame(stream: BinaryIO) -> dict[str, Any]:
    header = stream.read(4)
    if len(header) != 4:
        raise EOFError
    size = struct.unpack(">I", header)[0]
    if size > MAX_FRAME:
        raise ProtocolError(f"frame exceeds {MAX_FRAME} bytes")
    payload = stream.read(size)
    if len(payload) != size:
        raise ProtocolError("truncated frame")
    value = json.loads(payload.decode("utf-8"))
    if not isinstance(value, dict):
        raise ProtocolError("frame payload must be an object")
    return value


def write_frame(stream: BinaryIO, value: dict[str, Any]) -> None:
    payload = canonical_json(value)
    if len(payload) > MAX_FRAME:
        raise ProtocolError(f"frame exceeds {MAX_FRAME} bytes")
    stream.write(struct.pack(">I", len(payload)))
    stream.write(payload)
    stream.flush()


class Pairing:
    """One-time local pairing secret with strict file permissions."""

    def __init__(self, secret_path: str | os.PathLike[str], secret: bytes | None = None) -> None:
        self.path = Path(secret_path)
        self.secret = secret or secrets.token_bytes(32)

    def write(self) -> None:
        self.path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
        temporary = self.path.with_name(f".{self.path.name}.{os.getpid()}.tmp")
        flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL
        fd = os.open(temporary, flags, 0o600)
        try:
            with os.fdopen(fd, "wb") as handle:
                handle.write(self.secret)
                handle.flush()
                os.fsync(handle.fileno())
            os.replace(temporary, self.path)
            os.chmod(self.path, 0o600)
        finally:
            if temporary.exists():
                temporary.unlink()

    def proof(self, nonce: str) -> str:
        return hmac.new(self.secret, nonce.encode("utf-8"), hashlib.sha256).hexdigest()

    def verify(self, nonce: str, proof: str) -> bool:
        return hmac.compare_digest(self.proof(nonce), proof)


def load_secret(path: str | os.PathLike[str]) -> bytes:
    secret = Path(path).read_bytes()
    if len(secret) != 32:
        raise ProtocolError("pairing secret must be exactly 32 bytes")
    return secret
