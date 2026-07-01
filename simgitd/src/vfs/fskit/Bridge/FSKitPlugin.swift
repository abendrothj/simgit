import FSKit

/// An FSKit file-system extension that bridges macOS VFS operations over a
/// local TCP socket to a Rust backend (the simgitd daemon).
///
/// This is the Tier 3 backend for simgit on macOS 15.4+ — the intended
/// long-term replacement for the embedded NFSv3 server when a native
/// Apple-blessed VFS surface is desired.
///
/// ## Architecture
///
///     macOS VFS layer (FSKit host)
///       │ XPC
///       ▼
///     FSKitExt.appex (this file, compiled by Xcode)
///       │ TCP localhost → length-delimited protobuf or JSON-RPC
///       ▼
///     simgitd (Rust process, already running)
///       │ SessionVfsOps
///       ▼
///     BorrowRegistry + DeltaStore + git_resolver
///
/// ## Before you can build/ship this
///
/// ### 1. Xcode project setup
///
/// This file belongs in an Xcode project with these targets:
///
///   - **Host app** (macOS, `com.simgit.fskit-host`):
///     A minimal UI that exists only to embed and enable the extension.
///     macOS launches the extension only when the host app has been
///     opened at least once.  The host app can be a simple menu-bar app.
///
///   - **FSKit Extension** (App Extension, `com.simgit.fskit-ext`):
///     Contains this file + an `Info.plist` declaring:
///       EXAppExtensionAttributes:
///         FSShortName: "simgit"
///       NSExtension:
///         NSExtensionPrincipalClass: "$(PRODUCT_MODULE_NAME).FSKitPlugin"
///
/// ### 2. Entitlements (plist, applied to the appex target)
///
///     <key>com.apple.developer.fskit.fsmodule</key>
///     <true/>
///
/// ### 3. Apple Developer Program
///
/// The fskit.fsmodule entitlement **cannot be signed by free/personal
/// teams**.  A paid Apple Developer account is required to sign the
/// extension for development or distribution.  Without it, the extension
/// will appear in System Settings but refuse to activate.
///
/// ### 4. Enable in System Settings
///
/// After building and running the host app once, go to:
///
///   System Settings → General → Login Items & Extensions
///     → File System Extensions → "simgit" → Enable
///
/// ### 5. Mount
///
/// Once enabled, the extension is mounted via the standard `mount(8)`
/// command:
///
///     mkdir -p /tmp/simgit-fskit-session
///     mount -t simgit -o sid=<session-uuid> none /tmp/simgit-fskit-session
///
/// The `-o sid=...` option is forwarded to the extension and used to
/// announce which simgit session this mount belongs to.

@main
final class FSKitPlugin: FSUnaryFileSystemExtension {

    override func createFileSystem() -> (any FSFileSystem)? {
        if #available(macOS 15.4, *) {
            return SimgitFS()
        } else {
            return nil
        }
    }
}

// ---------------------------------------------------------------------------
// The FSKit file system implementation
// ---------------------------------------------------------------------------

@available(macOS 15.4, *)
final class SimgitFS: FSUnaryFileSystem, FSUnaryFileSystemOperations {

    // ── Temporary passthrough-to-self for Phase 0 scaffolding ──────────────
    //
    // Until the Rust TCP bridge is fully wired, `rootDirectoryURL` points
    // at a plain directory (e.g. under /tmp/) and every FSKit call is
    // forwarded to the on-disk filesystem.  This is the same "Phase 0 stub"
    // pattern the original plain-directory NFS backend used, but running
    // through the native FSKit IPC path rather than raw directories.
    //
    // To enable the bridge, replace the passthrough methods below with
    // TCP calls to the daemon's FSKit endpoint.

    private var root: URL? = nil
    private var sid: String = "" // session-id for daemon announce

    // MARK: - FSUnaryFileSystemOperations

    func loadResource(
        _ resource: any FSResource,
        options: FSLoadOptions,
        reply: @escaping (FSVolume?, (any Error)?) -> Void
    ) async throws {
        // Extract session-id from mount options.
        if let opts = options.parameters?["sid"] as? String {
            self.sid = opts
        }

        // Phase 0 stub: use a plain directory as the backing store.
        let base = URL(fileURLWithPath: "/tmp", isDirectory: true)
        let dir = base.appendingPathComponent("simgit-fskit-\(sid)", isDirectory: true)
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)

        self.root = dir
        reply(nil, nil)
    }

    func didFinishLoading() {
        // Ready for mount requests.
    }

    // ═══ Phase 0 passthrough stubs ════════════════════════════════════════
    //
    // Each of these methods would, in the real Tier 3 implementation,
    // forward the request over TCP to the simgitd daemon and translate
    // the daemon's response back into the FSKit result.
    //
    // The TCP protocol is described in `fskit-bridge-protocol.md`.
    // TODO: implement the bridge (see that doc).

    func itemGetAttributes(
        _ item: FSItem,
        requestedAttributes: Set<FSItemAttribute>,
        reply: @escaping (FSItemAttributes?, (any Error)?) -> Void
    ) async throws {
        // Stub: forward to real filesystem.
        guard let root = self.root else {
            reply(nil, NSError(domain: "com.simgit.fskit", code: 1))
            return
        }
        let path = root.appendingPathComponent(item.name)
        if let attrs = itemAttributes(path: path) {
            reply(attrs, nil)
        } else {
            reply(nil, NSError(domain: NSPOSIXErrorDomain, code: Int(ENOENT)))
        }
    }

    func itemLookup(
        _ parent: FSItem,
        name: String,
        reply: @escaping (FSItem?, (any Error)?) -> Void
    ) async throws {
        guard let root = self.root else {
            reply(nil, NSError(domain: "com.simgit.fskit", code: 1))
            return
        }
        let path = root.appendingPathComponent(name)
        if FileManager.default.fileExists(atPath: path.path) {
            reply(FSItem.item(name: name), nil)
        } else {
            reply(nil, NSError(domain: NSPOSIXErrorDomain, code: Int(ENOENT)))
        }
    }

    func itemRead(
        _ item: FSItem,
        offset: Int64,
        length: Int,
        reply: @escaping (Data?, (any Error)?) -> Void
    ) async throws {
        guard let root = self.root else {
            reply(nil, NSError(domain: "com.simgit.fskit", code: 1))
            return
        }
        let path = root.appendingPathComponent(item.name)
        do {
            let fh = try FileHandle(forReadingFrom: path)
            defer { try? fh.close() }
            try fh.seek(toOffset: UInt64(offset))
            let data = try fh.read(upToCount: length) ?? Data()
            reply(data, nil)
        } catch {
            reply(nil, error)
        }
    }

    func itemGetFileHandle(
        _ item: FSItem,
        forWriting: Bool,
        reply: @escaping (FSFileHandle?, (any Error)?) -> Void
    ) async throws {
        guard let root = self.root else {
            reply(nil, NSError(domain: "com.simgit.fskit", code: 1))
            return
        }
        let path = root.appendingPathComponent(item.name)
        if forWriting {
            // Write-through to real file; daemon picks up changes at commit.
            let fh = try? FileHandle(forWritingTo: path)
            reply(fh, nil)
        } else {
            let fh = try? FileHandle(forReadingFrom: path)
            reply(fh, nil)
        }
    }

    func itemWrite(
        _ handle: FSFileHandle,
        offset: Int64,
        data: Data,
        reply: @escaping (Int?, (any Error)?) -> Void
    ) async throws {
        do {
            try handle.seek(toOffset: UInt64(offset))
            try handle.write(contentsOf: data)
            reply(data.count, nil)
        } catch {
            reply(nil, error)
        }
    }

    func itemFlush(_ handle: FSFileHandle) async throws {
        try? handle.synchronize()
    }

    func itemListDirectory(
        _ item: FSItem,
        reply: @escaping ([FSDirectoryEntry]?, (any Error)?) -> Void
    ) async throws {
        guard let root = self.root else {
            reply(nil, NSError(domain: "com.simgit.fskit", code: 1))
            return
        }
        let path = root.appendingPathComponent(item.name)
        do {
            let names = try FileManager.default.contentsOfDirectory(atPath: path.path)
            let entries = names.map { FSDirectoryEntry(name: $0) }
            reply(entries, nil)
        } catch {
            reply(nil, error)
        }
    }

    // MARK: - Helpers

    private func itemAttributes(path: URL) -> FSItemAttributes? {
        guard let attrs = try? FileManager.default.attributesOfItem(atPath: path.path) else {
            return nil
        }
        let result = FSItemAttributes()
        result.fileSize = attrs[.size] as? Int64 ?? 0
        // FSItemAttributes uses String keys; fill in additional fields as needed.
        return result
    }
}
