import * as net from "node:net";
import * as os from "node:os";
import * as path from "node:path";
import { decodeRpcError, DeadlineExceededError, DaemonNotFoundError, SimgitError, CommitPendingError } from "./errors.js";
import type {
  CommitTelemetry,
  DiffResult,
  LockInfo,
  PeerEvent,
  RpcRequest,
  RpcResponse,
  SessionCommitResult,
  SessionCommitStatus,
  SessionInfo,
  SessionStatus,
} from "./types.js";

// ── Helpers ───────────────────────────────────────────────────────────────────

let nextId = 1;
function getNextId(): number {
  return nextId++;
}

function defaultSocketPath(): string {
  const xdgRuntime = process.env["XDG_RUNTIME_DIR"];
  if (xdgRuntime) {
    return path.join(xdgRuntime, "simgit", "control.sock");
  }
  const uid = process.getuid?.() ?? 0;
  return `/tmp/simgit-${uid}/control.sock`;
}

function rpcTimeoutMs(): number {
  const val = process.env["SIMGIT_RPC_TIMEOUT_SECS"];
  const parsed = val ? parseInt(val, 10) : NaN;
  return (Number.isFinite(parsed) && parsed > 0 ? parsed : 30) * 1000;
}

function rpcRetryCount(): number {
  const val = process.env["SIMGIT_RPC_RETRIES"];
  const parsed = val ? parseInt(val, 10) : NaN;
  return Number.isFinite(parsed) ? parsed : 2;
}

function deadlineEpochMs(timeoutMs: number): number {
  return Date.now() + timeoutMs;
}

function isRetryableError(err: unknown): boolean {
  if (!(err instanceof Error)) return false;
  const code = (err as NodeJS.ErrnoException).code;
  return (
    code === "ECONNREFUSED" ||
    code === "ECONNRESET" ||
    code === "EPIPE" ||
    code === "ENOENT" ||
    code === "ETIMEDOUT"
  );
}

// ── Low-level socket transport ────────────────────────────────────────────────

function callOnce(
  socketPath: string,
  method: string,
  params: unknown,
  timeoutMs: number,
): Promise<unknown> {
  return new Promise((resolve, reject) => {
    const socket = net.createConnection(socketPath);
    let settled = false;
    let buffer = "";

    const timer = setTimeout(() => {
      if (settled) return;
      settled = true;
      socket.destroy();
      reject(
        new SimgitError(
          `RPC response timeout (${timeoutMs / 1000}s) for method ${method}`,
        ),
      );
    }, timeoutMs);

    socket.once("error", (err: NodeJS.ErrnoException) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      if (err.code === "ECONNREFUSED" || err.code === "ENOENT") {
        reject(new DaemonNotFoundError(socketPath));
      } else {
        reject(err);
      }
    });

    socket.once("connect", () => {
      const req: RpcRequest = {
        jsonrpc: "2.0",
        id: getNextId(),
        method,
        params,
      };
      socket.write(JSON.stringify(req) + "\n");
      socket.end(); // half-close write side, keep read side open
    });

    socket.on("data", (chunk: Buffer) => {
      buffer += chunk.toString("utf8");
      const newline = buffer.indexOf("\n");
      if (newline === -1) return;

      const line = buffer.slice(0, newline).trim();
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      socket.destroy();

      if (!line) {
        reject(new SimgitError("empty RPC response"));
        return;
      }

      let resp: RpcResponse;
      try {
        resp = JSON.parse(line) as RpcResponse;
      } catch {
        reject(new SimgitError(`malformed RPC response: ${line}`));
        return;
      }

      if (resp.error) {
        reject(decodeRpcError(resp.error));
      } else {
        resolve(resp.result ?? null);
      }
    });

    socket.once("end", () => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      // Try to parse whatever we buffered
      const line = buffer.trim();
      if (!line) {
        reject(new SimgitError("empty RPC response"));
        return;
      }
      let resp: RpcResponse;
      try {
        resp = JSON.parse(line) as RpcResponse;
      } catch {
        reject(new SimgitError(`malformed RPC response: ${line}`));
        return;
      }
      if (resp.error) {
        reject(decodeRpcError(resp.error));
      } else {
        resolve(resp.result ?? null);
      }
    });
  });
}

async function callWithRetry(
  socketPath: string,
  method: string,
  params: unknown,
  timeoutMs: number,
): Promise<unknown> {
  const attempts = rpcRetryCount() + 1;
  let lastErr: unknown;

  for (let attempt = 1; attempt <= attempts; attempt++) {
    try {
      return await callOnce(socketPath, method, params, timeoutMs);
    } catch (err) {
      lastErr = err;
      if (attempt < attempts && isRetryableError(err)) {
        await new Promise((r) => setTimeout(r, 100 * attempt));
        continue;
      }
      throw err;
    }
  }

  throw lastErr;
}

// ── Client ────────────────────────────────────────────────────────────────────

export interface ClientOptions {
  /** Path to the daemon Unix socket. Defaults to SIMGIT_SOCKET env var or platform default. */
  socketPath?: string;
}

/**
 * Stateless simgit client. Each method opens a fresh socket connection.
 *
 * @example
 * ```ts
 * import { Client } from "simgit";
 *
 * const client = new Client();
 * const session = await client.sessionCreate("my-task");
 * // write files under session.mount_path ...
 * const result = await client.sessionCommit(session.session_id, {
 *   branchName: "feat/my-task",
 *   message: "automated change",
 * });
 * console.log("committed in", result.telemetry.total_duration_ms, "ms");
 * ```
 */
export class Client {
  private readonly socketPath: string;

  constructor(options: ClientOptions = {}) {
    this.socketPath =
      options.socketPath ??
      process.env["SIMGIT_SOCKET"] ??
      defaultSocketPath();
  }

  private async call<T>(method: string, params: unknown, timeoutMs?: number): Promise<T> {
    return callWithRetry(
      this.socketPath,
      method,
      params,
      timeoutMs ?? rpcTimeoutMs(),
    ) as Promise<T>;
  }

  // ── Session methods ─────────────────────────────────────────────────────────

  /**
   * Create a new agent session. Returns the session info and its mount path.
   * The agent should write all files exclusively under `mount_path`.
   */
  async sessionCreate(
    taskId: string,
    options: {
      agentLabel?: string;
      baseCommit?: string;
      peers?: boolean;
    } = {},
  ): Promise<SessionInfo> {
    return this.call<SessionInfo>("session.create", {
      task_id: taskId,
      agent_label: options.agentLabel ?? null,
      base_commit: options.baseCommit ?? null,
      peers: options.peers ?? false,
    });
  }

  /**
   * Flatten the session's delta layer into a git branch and release all locks.
   *
   * Handles idempotent retry automatically: if the transport drops after the
   * daemon accepts the request, the SDK will poll `commit.status` using the
   * same `request_id` before giving up.
   */
  async sessionCommit(
    sessionId: string,
    options: {
      branchName?: string;
      message?: string;
      timeoutMs?: number;
    } = {},
  ): Promise<SessionCommitResult> {
    const timeoutMs = options.timeoutMs ?? rpcTimeoutMs();
    const deadlineMs = deadlineEpochMs(timeoutMs);
    const requestId = crypto.randomUUID();
    const attempts = rpcRetryCount() + 1;

    const params = {
      session_id: sessionId,
      branch_name: options.branchName ?? null,
      message: options.message ?? null,
      deadline_epoch_ms: deadlineMs,
      request_id: requestId,
    };

    for (let attempt = 1; attempt <= attempts; attempt++) {
      try {
        return await callOnce(this.socketPath, "session.commit", params, timeoutMs) as SessionCommitResult;
      } catch (err) {
        if (err instanceof CommitPendingError || isRetryableError(err)) {
          // Transport may have dropped after daemon accepted — poll for status
          const resolved = await this.pollCommitStatus(sessionId, requestId, timeoutMs);
          if (resolved !== null) return resolved;
          if (attempt === attempts) throw err;
          await new Promise((r) => setTimeout(r, 100 * attempt));
          continue;
        }
        throw err;
      }
    }

    throw new SimgitError("commit failed after retries");
  }

  /**
   * Poll commit.status until the request reaches a terminal state.
   * Returns null if the request is not found or times out.
   */
  async pollCommitStatus(
    sessionId: string,
    requestId: string,
    pollWindowMs: number,
  ): Promise<SessionCommitResult | null> {
    const deadline = Date.now() + pollWindowMs;

    while (Date.now() < deadline) {
      const status = await this.sessionCommitStatus(sessionId, requestId);
      switch (status.state) {
        case "SUCCESS":
          if (!status.result) throw new SimgitError("commit status SUCCESS missing result payload");
          return status.result;
        case "FAILED":
          if (!status.error) throw new SimgitError("commit status FAILED missing error payload");
          throw decodeRpcError(status.error);
        case "NOT_FOUND":
          return null;
        case "PENDING":
          await new Promise((r) => setTimeout(r, 100));
          break;
      }
    }

    return null;
  }

  /** Look up the durable state of a commit request by request_id. */
  async sessionCommitStatus(
    sessionId: string,
    requestId: string,
  ): Promise<SessionCommitStatus> {
    return this.call<SessionCommitStatus>("commit.status", {
      session_id: sessionId,
      request_id: requestId,
    });
  }

  /** Discard the session's delta layer and release all locks. */
  async sessionAbort(sessionId: string): Promise<void> {
    await this.call<null>("session.abort", { session_id: sessionId });
  }

  /** List sessions, optionally filtered by status. */
  async sessionList(status?: SessionStatus): Promise<SessionInfo[]> {
    return this.call<SessionInfo[]>("session.list", { status: status ?? null });
  }

  /** Return the unified diff of an in-flight session against HEAD. */
  async sessionDiff(sessionId: string): Promise<DiffResult> {
    return this.call<DiffResult>("session.diff", { session_id: sessionId });
  }

  // ── Lock methods ────────────────────────────────────────────────────────────

  /** List current locks, optionally filtered by path prefix. */
  async lockList(pathPrefix?: string): Promise<LockInfo[]> {
    return this.call<LockInfo[]>("lock.list", { path: pathPrefix ?? null });
  }

  /**
   * Long-poll until the write lock on `path` is free (or timeout elapses).
   * Returns true if the lock was acquired, false if timeout elapsed.
   */
  async lockWait(filePath: string, timeoutMs?: number): Promise<boolean> {
    const result = await this.call<{ acquired: boolean }>("lock.wait", {
      path: filePath,
      timeout_ms: timeoutMs ?? null,
    });
    return result.acquired;
  }

  // ── Event methods ───────────────────────────────────────────────────────────

  /** List recent events, optionally filtered by source session. */
  async eventList(options: { sessionId?: string; limit?: number } = {}): Promise<PeerEvent[]> {
    return this.call<PeerEvent[]>("event.list", {
      session_id: options.sessionId ?? null,
      limit: options.limit ?? null,
    });
  }

  /**
   * Wait for the next event from a session (or the global stream).
   * Returns null on timeout.
   */
  async eventSubscribe(options: {
    sessionId?: string;
    timeoutMs?: number;
  } = {}): Promise<PeerEvent | null> {
    const result = await this.call<{ timeout?: boolean; event?: PeerEvent }>(
      "event.subscribe",
      {
        session_id: options.sessionId ?? null,
        timeout_ms: options.timeoutMs ?? null,
      },
    );
    if (result.timeout || !result.event) return null;
    return result.event;
  }
}

// ── Convenience: Session handle ───────────────────────────────────────────────

/**
 * A bound session handle returned by `Client.session()`.
 * Carries the session_id so callers don't pass it on every method.
 *
 * @example
 * ```ts
 * const session = await client.session("my-task");
 * // write files under session.info.mount_path ...
 * const result = await session.commit({ branchName: "feat/my-task" });
 * ```
 */
export class Session {
  constructor(
    private readonly client: Client,
    public readonly info: SessionInfo,
  ) {}

  get id(): string {
    return this.info.session_id;
  }

  get mountPath(): string {
    return this.info.mount_path;
  }

  async diff(): Promise<DiffResult> {
    return this.client.sessionDiff(this.id);
  }

  async commit(options: {
    branchName?: string;
    message?: string;
    timeoutMs?: number;
  } = {}): Promise<SessionCommitResult> {
    return this.client.sessionCommit(this.id, options);
  }

  async abort(): Promise<void> {
    return this.client.sessionAbort(this.id);
  }
}

/** Create a session and return a bound Session handle. */
export async function createSession(
  client: Client,
  taskId: string,
  options: {
    agentLabel?: string;
    baseCommit?: string;
    peers?: boolean;
  } = {},
): Promise<Session> {
  const info = await client.sessionCreate(taskId, options);
  return new Session(client, info);
}
