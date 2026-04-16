/**
 * Public types for the simgit TypeScript SDK.
 * Mirrors simgit-sdk/src/types.rs — all shapes are JSON-RPC 2.0 compatible.
 */

// ── Session ───────────────────────────────────────────────────────────────────

/** Lifecycle state of a session. */
export type SessionStatus = "ACTIVE" | "COMMITTED" | "ABORTED" | "STALE";

/** Metadata for a single agent session. */
export interface SessionInfo {
  session_id: string;
  task_id: string;
  agent_label: string | null;
  base_commit: string;
  created_at: string; // ISO 8601
  status: SessionStatus;
  /** Mount path where the session's CoW overlay is accessible. */
  mount_path: string;
  branch_name: string | null;
  peers_enabled: boolean;
}

/** Per-commit telemetry emitted by session.commit. All durations in ms. */
export interface CommitTelemetry {
  total_duration_ms: number;
  capture_self_queue_wait_ms: number;
  capture_self_ms: number;
  capture_peers_queue_wait_ms: number;
  capture_peers_execution_ms: number;
  capture_peers_ms: number;
  scheduler_queue_wait_ms: number;
  conflict_scan_ms: number;
  flatten_queue_wait_ms: number;
  flatten_ms: number;
  flatten_write_tree_ms: number;
  flatten_apply_manifest_ms: number;
  flatten_ref_update_ms: number;
  flatten_commit_object_ms: number;
  retry_count: number;
}

/** Return shape for session.commit. */
export interface SessionCommitResult extends SessionInfo {
  telemetry: CommitTelemetry;
}

/** Durable state for an idempotent commit request. */
export type CommitRequestState = "PENDING" | "SUCCESS" | "FAILED" | "NOT_FOUND";

/** Lookup payload for commit.status. */
export interface SessionCommitStatus {
  session_id: string;
  request_id: string;
  state: CommitRequestState;
  result?: SessionCommitResult;
  error?: RpcError;
}

// ── Conflicts ─────────────────────────────────────────────────────────────────

/** Detailed operation overlap for a single conflicting path. */
export interface MergePathConflict {
  path: string;
  ours_ops: string[];
  peer_ops: string[];
}

/** Conflict details for one peer session that blocked our commit. */
export interface MergeConflictPeer {
  session_id: string;
  task_id: string;
  paths: string[];
  path_conflicts: MergePathConflict[];
}

/** Structured payload returned for pre-commit overlap conflicts. */
export interface MergeConflictDetail {
  session_id: string;
  kind: string;
  conflicts: MergeConflictPeer[];
}

// ── Locks ─────────────────────────────────────────────────────────────────────

/** Information about a lock on a filesystem path. */
export interface LockInfo {
  path: string;
  writer_session: string | null;
  reader_sessions: string[];
  acquired_at: string; // ISO 8601
  ttl_seconds: number | null;
}

// ── Events ────────────────────────────────────────────────────────────────────

/** Event emitted by the daemon event broker. */
export interface PeerEvent {
  source_session: string;
  kind: string;
  payload: unknown;
  emitted_at: string; // ISO 8601
}

// ── Diff ──────────────────────────────────────────────────────────────────────

/** Result of diffing a session's delta layer against HEAD. */
export interface DiffResult {
  session_id: string;
  unified_diff: string;
  changed_paths: string[];
}

// ── RPC wire types ────────────────────────────────────────────────────────────

export interface RpcRequest {
  jsonrpc: "2.0";
  id: number;
  method: string;
  params: unknown;
}

export interface RpcResponse {
  jsonrpc: "2.0";
  id: number;
  result?: unknown;
  error?: RpcError;
}

export interface RpcError {
  code: number;
  message: string;
  data?: unknown;
}

// ── RPC error codes ───────────────────────────────────────────────────────────

export const ERR_BORROW_CONFLICT   = -32001;
export const ERR_SESSION_NOT_FOUND = -32002;
export const ERR_MERGE_CONFLICT    = -32003;
export const ERR_QUOTA_EXCEEDED    = -32004;
export const ERR_INVALID_PATH      = -32005;
export const ERR_DEADLINE_EXCEEDED = -32006;
export const ERR_COMMIT_PENDING    = -32007;
