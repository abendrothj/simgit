export { Client, Session, createSession } from "./client.js";
export type { ClientOptions } from "./client.js";

export {
  SimgitError,
  DaemonNotFoundError,
  BorrowConflictError,
  MergeConflictError,
  SessionNotFoundError,
  QuotaExceededError,
  DeadlineExceededError,
  CommitPendingError,
  RpcCallError,
} from "./errors.js";

export type {
  SessionStatus,
  SessionInfo,
  CommitTelemetry,
  SessionCommitResult,
  CommitRequestState,
  SessionCommitStatus,
  MergePathConflict,
  MergeConflictPeer,
  MergeConflictDetail,
  LockInfo,
  PeerEvent,
  DiffResult,
} from "./types.js";
