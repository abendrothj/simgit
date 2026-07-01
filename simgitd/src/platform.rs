//! Platform abstraction layer — hides Unix/Windows differences behind helpers.
//!
//! Everything that currently calls `libc` directly or checks `cfg!(target_os)`
//! should route through this module instead.

/// Return the current user's numeric UID.
///
/// On Unix this is a real uid.  On Windows we return 0 because Windows uses
/// SIDs, not uids, and the value is only used for filesystem identity in VFS
/// attributes (FUSE/NFS).  Root ownership (0) is the safest default.
pub fn current_uid() -> u32 {
    #[cfg(unix)]
    {
        unsafe { libc::getuid() }
    }
    #[cfg(windows)]
    {
        0
    }
}

/// Return the current user's numeric GID.
pub fn current_gid() -> u32 {
    #[cfg(unix)]
    {
        unsafe { libc::getgid() }
    }
    #[cfg(windows)]
    {
        0
    }
}

/// Secure the daemon's control endpoint so only the current user can connect.
///
/// On Unix this sets `0600` on the socket.  On Windows this is a no-op
/// (named-pipe ACLs are set by the pipe creator, and TCP loopback is already
/// local-host only).
pub fn secure_rpc_endpoint(path: &std::path::Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let meta = std::fs::metadata(path)?;
        let mut perms = meta.permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(path, perms)?;
    }
    #[cfg(windows)]
    {
        let _ = path; // no-op: Windows named-pipe security is set at creation time
    }
    Ok(())
}

/// Spawn a background task that listens for OS-level shutdown signals.
///
/// On Unix this waits for SIGTERM.  On Windows we only have Ctrl+C (which is
/// handled separately by `tokio::signal::ctrl_c()`).
pub async fn wait_for_termination_signal() -> std::io::Result<()> {
    #[cfg(unix)]
    {
        let mut sigterm = tokio::signal::unix::signal(
            tokio::signal::unix::SignalKind::terminate(),
        )?;
        sigterm.recv().await;
        Ok(())
    }
    #[cfg(windows)]
    {
        // Windows has no SIGTERM — daemon shutdown is driven by Ctrl+C or
        // the Windows Service Control Manager.  This future never resolves;
        // shutdown on Windows is exclusively via ctrl_c().
        std::future::pending::<()>().await;
        Ok(())
    }
}
