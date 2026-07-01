# simgit FSKit bridge protocol

This document describes the TCP bridge protocol between the simgit FSKit
extension (`FSKitExt.appex`, written in Swift) and the simgitd daemon
(written in Rust).

## Transport

- **Address:** `127.0.0.1:<port>` — the port is allocated by simgitd at
  startup and communicated to the extension via the `-o port=<N>` mount
  option (passed through `FSLoadOptions.parameters`).
- **Framing:** length-prefixed messages. Each message is a 4-byte
  big-endian length followed by that many bytes of payload.
- **Payload format:** JSON-RPC 2.0 (same protocol as the daemon's Unix
  socket control interface), with an extended set of VFS methods.
- **Lifetime:** one connection per mount.  The extension connects on
  first FSKit operation and keeps the TCP socket open until unmount.

## Methods (proposed, not yet implemented)

| Method | FSKit op | Params | Result |
|---|---|---|---|
| `fskit.lookup` | `itemLookup` | `{parent_id, name}` | `{id, attr}` |
| `fskit.getattr` | `itemGetAttributes` | `{id}` | `{attr}` |
| `fskit.read` | `itemRead` | `{id, offset, length}` | `{data}` |
| `fskit.write` | `itemWrite` | `{id, offset, data}` | `{written, attr}` |
| `fskit.readdir` | `itemListDirectory` | `{id}` | `{entries[]}` |
| `fskit.create` | `itemLookup` (create) | `{parent_id, name, mode}` | `{id, attr}` |
| `fskit.remove` | `itemDelete` | `{parent_id, name}` | `{}` |
| `fskit.rename` | `itemRename` | `{src_id, src_name, dst_id, dst_name}` | `{}` |

All methods can return a JSON-RPC error with the `VfsOpError` enum
mapped to FSKit-appropriate NSError codes.

## Implementation plan

1. Add an `fskit_bridge.rs` module in simgitd that binds a `TcpListener`
   on `127.0.0.1:0` and serves the JSON-RPC methods above, delegating
   each to `SessionVfsOps` (already defined and shared by FUSE/NFS).
2. Remove the Phase 0 passthrough stubs from `FSKitPlugin.swift` and
   replace them with TCP calls to the daemon.
3. Update the daemon's config to expose the FSKit bridge port to mount
   options.
4. Build, sign, and test with real file I/O through the bridge.

## Before you can build/ship

See the top-of-file comment in `Bridge/FSKitPlugin.swift` for the
required Xcode project setup, entitlements, Apple Developer Program
requirements, and System Settings enablement steps.
