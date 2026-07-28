# FreeCAD fixture sources

The checked-in JSON fixture is the portable bridge boundary used when FreeCAD
is unavailable. A real FreeCAD source model for this fixture is intentionally
not committed as a binary. It consists of a Part Design body, a fully
constrained sketch with one 6 mm diameter constraint, and a through pocket.

Record a source model with the addon extractor using the same document name
and compare its bridge DTO against `../bridge-dto/simple_document.json` after
normalizing FreeCAD/OCCT version metadata and timestamps.
