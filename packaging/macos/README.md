# macOS packaging

Install the addon archive under FreeCAD's user `Mod` directory and launch the
sidecar from the MCP host. Unix-domain IPC is preferred; the private
loopback-TCP fallback is available when the host cannot use a Unix socket.
