# Packaging

The FreeCAD addon archive is deterministic and contains the `SemanticMCP`
package at the location expected by FreeCAD's user `Mod` directory:

```sh
python scripts/package_addon.py --output dist/topomind-freecad-addon.zip
```

The Rust sidecar is built by Cargo or the Nix flake. Linux desktop metadata is
in `packaging/linux`; Windows and macOS use the same stdio sidecar and private
rendezvous record, with platform launchers supplied by the host application.
No package opens a public TCP listener or stores a pairing secret in the MCP
stream.
