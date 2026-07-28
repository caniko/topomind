"""Core adapter registry; adapters enrich reads but cannot weaken policy."""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any, Callable


@dataclass(frozen=True)
class Adapter:
    name: str
    object_type_patterns: tuple[str, ...]
    extractor_version: str
    transaction_safety: str
    operations: tuple[str, ...] = ()
    validators: tuple[str, ...] = ()

    def matches(self, type_id: str) -> bool:
        return any(type_id == pattern or type_id.startswith(pattern.rstrip("*")) for pattern in self.object_type_patterns)


@dataclass
class AdapterRegistry:
    adapters: list[Adapter] = field(default_factory=list)

    def register(self, adapter: Adapter) -> None:
        if any(existing.name == adapter.name for existing in self.adapters):
            raise ValueError(f"adapter already registered: {adapter.name}")
        self.adapters.append(adapter)

    def for_type(self, type_id: str) -> Adapter | None:
        return next((adapter for adapter in self.adapters if adapter.matches(type_id)), None)

    def capabilities(self) -> list[str]:
        return sorted({operation for adapter in self.adapters for operation in adapter.operations})


def core_registry() -> AdapterRegistry:
    registry = AdapterRegistry()
    registry.register(Adapter("core.document", ("App::Document",), "1.0", "verified", validators=("recompute",)))
    registry.register(Adapter("part", ("Part::", "PartDesign::Feature"), "1.0", "verified", operations=("create_primitive", "boolean"), validators=("shape_validity",)))
    registry.register(Adapter("partdesign", ("PartDesign::",), "1.0", "verified", operations=("set_property", "set_expression"), validators=("recompute", "shape_validity")))
    registry.register(Adapter("sketcher", ("Sketcher::SketchObject",), "1.0", "verified", operations=("sketch_set_datum",), validators=("sketch_solver",)))
    return registry
