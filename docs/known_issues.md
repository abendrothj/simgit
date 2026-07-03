# Design notes / resolved issues

## Delete & rename semantics across backends (FUSE/WinFSP vs macOS NFS)

**Status:** resolved. Both backend families now commit a real git deletion for
`rm`/rename-away, via two different mount-level mechanisms dictated by each
platform's client caching behavior.

All backends must satisfy two things at once:

1. **Correct commit** — `rm x` (or renaming `x` away) must commit as a git
   deletion, not as an empty file.
2. **Create-after-delete** — recreating a path right after deleting it must
   work, because `git checkout` overwrites files with `open(O_CREAT|O_TRUNC)`.

### FUSE / WinFSP (shared `SessionFs`)

Records a **real deletion** (`DeltaStore::mark_deleted`): the path is removed
from `writes` and added to `deletes`, so `LOOKUP` returns `ENOENT` and the file
disappears at the mount. The Linux kernel does not negatively cache FUSE
lookups (no `negative_timeout` is set), so create-after-delete still reaches the
server. Rename also remaps the reused inode to the moved content — the kernel
reuses the *source* inode number for the destination name, so without the remap
reads through it would hit the deleted source path. Covered by the Linux FUSE
mount-integration test and the `session_fs` unit tests.

### macOS NFS

A real deletion is **not** usable here: removing the path makes `LOOKUP` return
`NFS3ERR_NOENT`, which the macOS NFS client negatively caches **even under
`actimeo=0`** (confirmed on a live mount) — the client then never sends the
`CREATE` RPC for a following `open(O_CREAT|O_TRUNC)`, breaking `git checkout`.

So the NFS backend keeps the path **LOOKUP-able as a zero-length blob** (in
`writes`) but records a **tombstone** (`DeltaManifest::tombstones`,
`DeltaStore::mark_tombstone`). `flatten` treats a tombstoned path as a deletion,
and a subsequent write (recreate) clears the tombstone via `write_blob`. Net
result: the commit is correct (deletion, or new content if recreated) while the
live mount never trips the negative cache.

Trade-off: at the *mount* level on macOS, `rm x; ls` still shows `x` as an empty
file until commit. This is a cosmetic divergence from FUSE; the committed tree
is identical on both platforms. Covered by `flatten` unit test
`gix_flatten_treats_tombstone_as_deletion` and verified end-to-end on a live NFS
mount (delete, delete+recreate, and commit).
