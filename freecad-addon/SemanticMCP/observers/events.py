"""Small event envelopes and main-thread coalescing."""

from __future__ import annotations

import time
from dataclasses import dataclass
from typing import Any, Callable


@dataclass(frozen=True)
class EventEnvelope:
    event_class: str
    object_name: str | None
    sequence: int
    monotonic_ms: int


class EventCoalescer:
    def __init__(self, debounce_ms: int = 100) -> None:
        self.debounce_ms = debounce_ms
        self._sequence = 0
        self._pending: dict[str, EventEnvelope] = {}

    def push(self, event_class: str, object_name: str | None = None) -> EventEnvelope:
        self._sequence += 1
        event = EventEnvelope(event_class, object_name, self._sequence, int(time.monotonic() * 1000))
        self._pending[event_class] = event
        return event

    def flush(self, now_ms: int | None = None, *, force: bool = False) -> list[EventEnvelope]:
        now_ms = now_ms if now_ms is not None else int(time.monotonic() * 1000)
        ready = [event for event in self._pending.values() if force or now_ms - event.monotonic_ms >= self.debounce_ms or event.event_class == "transaction"]
        for event in ready:
            self._pending.pop(event.event_class, None)
        return sorted(ready, key=lambda event: event.sequence)


class ObserverRegistry:
    """Installs only documented callback-shaped observers when APIs exist."""

    def __init__(self, on_event: Callable[[EventEnvelope], None], coalescer: EventCoalescer | None = None) -> None:
        self.on_event = on_event
        self.coalescer = coalescer or EventCoalescer()
        self._installed: list[Any] = []

    def emit(self, event_class: str, object_name: str | None = None) -> None:
        self.coalescer.push(event_class, object_name)
        for event in self.coalescer.flush(force=event_class == "transaction"):
            self.on_event(event)

    def flush(self, *, force: bool = False) -> None:
        for event in self.coalescer.flush(force=force):
            self.on_event(event)

    def install(self, app: Any, gui: Any = None) -> None:
        document_observer = _DocumentObserver(self)
        add_observer = getattr(app, "addDocumentObserver", None)
        if add_observer:
            add_observer(document_observer)
            self._installed.append(document_observer)
        selection = getattr(gui, "Selection", None) if gui is not None else None
        if selection is not None and hasattr(selection, "addObserver"):
            selection_observer = _SelectionObserver(self)
            selection.addObserver(selection_observer)
            self._installed.append(selection_observer)

    def remove(self, app: Any, gui: Any = None) -> None:
        for observer in self._installed:
            remove = getattr(app, "removeDocumentObserver", None)
            if remove:
                try:
                    remove(observer)
                except Exception:
                    pass
            selection = getattr(gui, "Selection", None) if gui is not None else None
            if selection is not None and hasattr(selection, "removeObserver"):
                try:
                    selection.removeObserver(observer)
                except Exception:
                    pass
        self._installed.clear()


class _DocumentObserver:
    def __init__(self, registry: ObserverRegistry) -> None:
        self.registry = registry

    def slotCreatedObject(self, document: Any, obj: Any) -> None:
        self.registry.emit("geometry", getattr(obj, "Name", None))

    def slotDeletedObject(self, document: Any, name: str) -> None:
        self.registry.emit("geometry", name)

    def slotChangedObject(self, document: Any, obj: Any, property_name: str) -> None:
        event_class = "metadata" if property_name in {"Label", "Label2"} else "parametric"
        self.registry.emit(event_class, getattr(obj, "Name", None))

    def slotRecomputedDocument(self, document: Any) -> None:
        self.registry.emit("geometry")

    def transactionOpened(self, document: Any) -> None:
        self.registry.emit("transaction")

    def transactionCommitted(self, document: Any) -> None:
        self.registry.emit("transaction")

    def transactionAborted(self, document: Any) -> None:
        self.registry.emit("transaction")


class _SelectionObserver:
    def __init__(self, registry: ObserverRegistry) -> None:
        self.registry = registry

    def addSelection(self, document: Any, object_name: str, subelement: str, position: Any) -> None:
        self.registry.emit("focus", object_name)

    def clearSelection(self, document: Any) -> None:
        self.registry.emit("focus")

    def setPreselection(self, document: Any, object_name: str, subelement: str) -> None:
        self.registry.emit("focus", object_name)

    def removeSelection(self, document: Any, object_name: str, subelement: str) -> None:
        self.registry.emit("focus", object_name)
