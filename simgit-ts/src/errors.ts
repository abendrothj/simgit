import type { MergeConflictDetail, RpcError } from "./types.js";
import {
  ERR_BORROW_CONFLICT,
  ERR_COMMIT_PENDING,
  ERR_DEADLINE_EXCEEDED,
  ERR_INVALID_PATH,
  ERR_MERGE_CONFLICT,
  ERR_QUOTA_EXCEEDED,
  ERR_SESSION_NOT_FOUND,
} from "./types.js";

export class SimgitError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "SimgitError";
  }
}

export class DaemonNotFoundError extends SimgitError {
  constructor(public readonly socketPath: string) {
    super(`simgit daemon not found at ${socketPath} — is simgitd running?`);
    this.name = "DaemonNotFoundError";
  }
}

export class BorrowConflictError extends SimgitError {
  constructor(
    public readonly path: string,
    public readonly holderSessionId: string,
    public readonly holderTaskId: string,
  ) {
    super(
      `path '${path}' is exclusively held by session ${holderSessionId} (task: ${holderTaskId})`,
    );
    this.name = "BorrowConflictError";
  }
}

export class MergeConflictError extends SimgitError {
  constructor(public readonly detail: MergeConflictDetail) {
    const paths = detail.conflicts.flatMap((c) => c.paths).join(", ");
    const peers = detail.conflicts
      .map((c) => `${c.session_id} (${c.task_id})`)
      .join(", ");
    super(
      `merge conflict on paths [${paths}] against active session(s): ${peers}`,
    );
    this.name = "MergeConflictError";
  }
}

export class SessionNotFoundError extends SimgitError {
  constructor(message: string) {
    super(message);
    this.name = "SessionNotFoundError";
  }
}

export class QuotaExceededError extends SimgitError {
  constructor(message: string) {
    super(message);
    this.name = "QuotaExceededError";
  }
}

export class DeadlineExceededError extends SimgitError {
  constructor(message: string) {
    super(message);
    this.name = "DeadlineExceededError";
  }
}

export class CommitPendingError extends SimgitError {
  constructor(message: string) {
    super(message);
    this.name = "CommitPendingError";
  }
}

export class RpcCallError extends SimgitError {
  constructor(
    public readonly code: number,
    message: string,
  ) {
    super(`RPC error ${code}: ${message}`);
    this.name = "RpcCallError";
  }
}

export function decodeRpcError(err: RpcError): SimgitError {
  switch (err.code) {
    case ERR_BORROW_CONFLICT: {
      const data = err.data as {
        path: string;
        holder: { session_id: string; task_id: string };
      };
      return new BorrowConflictError(
        data.path,
        data.holder.session_id,
        data.holder.task_id,
      );
    }
    case ERR_MERGE_CONFLICT:
      return new MergeConflictError(err.data as MergeConflictDetail);
    case ERR_SESSION_NOT_FOUND:
      return new SessionNotFoundError(err.message);
    case ERR_QUOTA_EXCEEDED:
      return new QuotaExceededError(err.message);
    case ERR_DEADLINE_EXCEEDED:
      return new DeadlineExceededError(err.message);
    case ERR_COMMIT_PENDING:
      return new CommitPendingError(err.message);
    case ERR_INVALID_PATH:
      return new SimgitError(`invalid path: ${err.message}`);
    default:
      return new RpcCallError(err.code, err.message);
  }
}
