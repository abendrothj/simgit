#!/usr/bin/env python3

"""
Real-Agent Harness for Mock-to-Reality Calibration.

This harness runs deterministic or LLM-backed agents against a live simgit daemon.
It writes real files inside each session mount before calling session.commit, so the
report reflects actual conflict and latency behavior instead of synthetic bookkeeping.
"""

from __future__ import annotations

import argparse
import concurrent.futures
import glob
import json
import logging
import math
import os
import socket
import sys
import threading
import time
import urllib.error
import urllib.request
import uuid
from abc import ABC, abstractmethod
from collections import Counter
from dataclasses import asdict, dataclass, field
from datetime import datetime
from pathlib import Path
from typing import Any, Dict, List, Optional


logging.basicConfig(level=logging.INFO, format="%(asctime)s [%(levelname)s] %(message)s")
logger = logging.getLogger(__name__)


@dataclass
class FileEdit:
    """Track one filesystem edit applied inside a session mount."""

    file_path: str
    action: str
    content: str = ""
    size_bytes: Optional[int] = None
    content_sample: str = ""


@dataclass
class CommitAttempt:
    """Track a single commit attempt."""

    attempt_num: int
    timestamp_start: float
    timestamp_end: float = 0.0
    duration_ms: float = 0.0
    success: bool = False
    error_type: Optional[str] = None
    error_message: str = ""
    edits_count: int = 0


@dataclass
class AgentSession:
    """Track one agent's lifecycle and outcomes."""

    agent_id: str
    task_id: str
    session_id: Optional[str] = None
    mount_path: Optional[str] = None
    created_at: float = field(default_factory=time.time)
    completed_at: Optional[float] = None
    total_duration_s: float = 0.0
    success: bool = False
    failure_type: Optional[str] = None
    file_edits: List[FileEdit] = field(default_factory=list)
    commit_attempts: List[CommitAttempt] = field(default_factory=list)
    conflicts_encountered: int = 0
    conflicts_resolved: int = 0
    conflict_paths: List[str] = field(default_factory=list)
    model_used: Optional[str] = None
    model_tokens_used: int = 0
    tool_calls_made: int = 0
    retries_count: int = 0
    notes: List[str] = field(default_factory=list)

    def to_dict(self) -> Dict[str, Any]:
        return {
            "agent_id": self.agent_id,
            "task_id": self.task_id,
            "session_id": self.session_id,
            "mount_path": self.mount_path,
            "created_at": datetime.fromtimestamp(self.created_at).isoformat(),
            "completed_at": datetime.fromtimestamp(self.completed_at).isoformat()
            if self.completed_at
            else None,
            "total_duration_s": self.total_duration_s,
            "success": self.success,
            "failure_type": self.failure_type,
            "file_edits": [
                {
                    "file_path": edit.file_path,
                    "action": edit.action,
                    "size_bytes": edit.size_bytes,
                    "content_sample": edit.content_sample,
                }
                for edit in self.file_edits
            ],
            "commit_attempts": [asdict(attempt) for attempt in self.commit_attempts],
            "conflicts_encountered": self.conflicts_encountered,
            "conflicts_resolved": self.conflicts_resolved,
            "conflict_paths": self.conflict_paths,
            "model_used": self.model_used,
            "model_tokens_used": self.model_tokens_used,
            "tool_calls_made": self.tool_calls_made,
            "retries_count": self.retries_count,
            "notes": self.notes,
        }


class SimgitRPCClient:
    """Low-level JSON-RPC client for simgit daemon."""

    def __init__(self, socket_path: str, timeout_s: float = 30.0):
        self.socket_path = socket_path
        self.timeout_s = timeout_s
        self.request_id = 0
        self._request_lock = threading.Lock()

    def _next_request_id(self) -> int:
        with self._request_lock:
            self.request_id += 1
            return self.request_id

    def _send_request(self, method: str, params: Dict[str, Any]) -> Dict[str, Any]:
        request = {
            "jsonrpc": "2.0",
            "id": self._next_request_id(),
            "method": method,
            "params": params,
        }

        try:
            client_socket = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            client_socket.settimeout(self.timeout_s)
            client_socket.connect(self.socket_path)

            payload = json.dumps(request) + "\n"
            client_socket.sendall(payload.encode())

            response_data = b""
            while b"\n" not in response_data:
                chunk = client_socket.recv(4096)
                if not chunk:
                    break
                response_data += chunk

            client_socket.close()

            if not response_data:
                return {"error": "No response from daemon"}

            return json.loads(response_data.splitlines()[0].decode())
        except Exception as exc:
            return {"error": str(exc)}

    def session_create(
        self,
        task_id: str,
        agent_label: str,
        base_commit: Optional[str] = None,
    ) -> tuple[bool, Dict[str, Any]]:
        result = self._send_request(
            "session.create",
            {
                "task_id": task_id,
                "agent_label": agent_label,
                "base_commit": base_commit,
                "peers": False,
            },
        )

        if "error" in result:
            return False, {"error": result["error"]}
        if "result" in result:
            return True, result["result"]
        return False, {"error": f"Unexpected response: {result}"}

    def session_commit(
        self,
        session_id: str,
        branch_name: Optional[str] = None,
        message: Optional[str] = None,
    ) -> tuple[bool, Dict[str, Any]]:
        payload = {"session_id": session_id}
        if branch_name:
            payload["branch_name"] = branch_name
        if message:
            payload["message"] = message

        result = self._send_request("session.commit", payload)
        if "error" in result:
            return False, {"error": result.get("error", "Unknown error")}
        if "result" in result:
            return True, result["result"]
        return False, {"error": f"Unexpected response: {result}"}

    def session_abort(self, session_id: str) -> tuple[bool, Dict[str, Any]]:
        result = self._send_request("session.abort", {"session_id": session_id})
        if "error" in result:
            return False, {"error": result.get("error", "Unknown error")}
        if "result" in result:
            return True, result["result"]
        return False, {"error": f"Unexpected response: {result}"}


class Agent(ABC):
    """Abstract agent that prepares edits and decides when to commit."""

    def __init__(self, agent_id: str, task_id: str):
        self.agent_id = agent_id
        self.task_id = task_id
        self.session: Optional[AgentSession] = None

    @abstractmethod
    def initialize(self, rpc_client: SimgitRPCClient) -> bool:
        pass

    @abstractmethod
    def get_next_action(self) -> Optional[List[FileEdit]]:
        pass

    @abstractmethod
    def should_commit(self) -> bool:
        pass

    @abstractmethod
    def explain_result(self, session: AgentSession) -> str:
        pass


class DeterministicTestAgent(Agent):
    """Deterministic agent for repeatable hotspot/disjoint calibration."""

    def __init__(
        self,
        agent_id: str,
        task_id: str,
        task_profile: str = "disjoint-files",
        shared_path: str = "bench/shared_hotspot.txt",
        delay_between_edits_s: float = 0.05,
    ):
        super().__init__(agent_id, task_id)
        self.task_profile = task_profile
        self.shared_path = shared_path
        self.delay_between_edits_s = delay_between_edits_s
        self.phase = "create"

    def initialize(self, rpc_client: SimgitRPCClient) -> bool:
        return True

    def _target_paths(self) -> List[str]:
        unique_path = f"bench/disjoint/{self.agent_id}.txt"
        if self.task_profile == "hotspot-file":
            return [self.shared_path]
        if self.task_profile == "sharded-hotspot":
            # Each agent writes its own shard of the shared directory.
            # No two agents touch the same path, so conflict_rate should drop to ~0
            # while still exercising the shared-directory hotspot code path.
            shard_path = f"bench/hotspot-shards/{self.agent_id}.txt"
            return [shard_path]
        if self.task_profile == "mixed":
            return [unique_path, self.shared_path]
        return [unique_path]

    def get_next_action(self) -> Optional[List[FileEdit]]:
        time.sleep(self.delay_between_edits_s)
        target_paths = self._target_paths()

        if self.phase == "create":
            self.phase = "modify"
            return [
                FileEdit(
                    file_path=path,
                    action="create",
                    content=(
                        f"agent={self.agent_id}\n"
                        f"task={self.task_id}\n"
                        f"phase=create\n"
                        f"ts={time.time_ns()}\n"
                    ),
                )
                for path in target_paths
            ]

        if self.phase == "modify":
            self.phase = "commit"
            return [
                FileEdit(
                    file_path=path,
                    action="modify",
                    content=(
                        f"agent={self.agent_id}\n"
                        f"task={self.task_id}\n"
                        f"phase=modify\n"
                        f"profile={self.task_profile}\n"
                        f"ts={time.time_ns()}\n"
                    ),
                )
                for path in target_paths
            ]

        return None

    def should_commit(self) -> bool:
        return self.phase == "commit"

    def explain_result(self, session: AgentSession) -> str:
        if session.success:
            return f"{self.agent_id}: committed {len(session.file_edits)} deterministic edits"
        return f"{self.agent_id}: failed ({session.failure_type})"


class LLMAgent(Agent):
    """Minimal OpenAI-compatible agent that returns JSON edit plans."""

    def __init__(
        self,
        agent_id: str,
        task_id: str,
        model: str,
        task_description: str,
        task_profile: str,
        shared_path: str,
        api_key: Optional[str],
        base_url: str,
        timeout_s: float = 60.0,
        temperature: float = 0.2,
    ):
        super().__init__(agent_id, task_id)
        self.model = model
        self.task_description = task_description
        self.task_profile = task_profile
        self.shared_path = shared_path
        self.api_key = api_key
        self.base_url = base_url.rstrip("/")
        self.timeout_s = timeout_s
        self.temperature = temperature
        self.generated_plan = False
        self.commit_message = f"llm harness commit ({agent_id})"

    def initialize(self, rpc_client: SimgitRPCClient) -> bool:
        if not self.model:
            logger.error("LLM model is required when --agent-type llm is selected")
            return False
        if not self.api_key:
            logger.error(
                "LLM API key missing. Set SIMGIT_LLM_API_KEY or OPENAI_API_KEY before running."
            )
            return False
        if self.session is not None:
            self.session.model_used = self.model
        return True

    def _allowed_paths(self) -> List[str]:
        unique_path = f"bench/llm/{self.agent_id}.txt"
        if self.task_profile == "hotspot-file":
            return [self.shared_path]
        if self.task_profile == "sharded-hotspot":
            # Per-agent shard: same directory as hotspot but unique filename.
            shard_path = f"bench/hotspot-shards/{self.agent_id}.txt"
            return [shard_path]
        if self.task_profile == "mixed":
            return [unique_path, self.shared_path]
        return [unique_path]

    def _extract_json_object(self, content: str) -> Dict[str, Any]:
        stripped = content.strip()
        if stripped.startswith("```"):
            stripped = stripped.strip("`")
            if stripped.startswith("json"):
                stripped = stripped[4:].strip()
        start = stripped.find("{")
        end = stripped.rfind("}")
        if start == -1 or end == -1 or end <= start:
            raise ValueError(f"Model response did not contain a JSON object: {content!r}")
        return json.loads(stripped[start : end + 1])

    def _coerce_message_content(self, payload: Dict[str, Any]) -> str:
        choices = payload.get("choices") or []
        if not choices:
            raise ValueError(f"Model response had no choices: {payload}")
        message = choices[0].get("message") or {}
        content = message.get("content", "")
        if isinstance(content, list):
            text_parts = []
            for part in content:
                if isinstance(part, dict) and part.get("type") == "text":
                    text_parts.append(part.get("text", ""))
                else:
                    text_parts.append(str(part))
            return "\n".join(text_parts)
        return str(content)

    def _call_model(self) -> Dict[str, Any]:
        if self.session is None:
            raise RuntimeError("session must be bound before the model is called")

        system_prompt = (
            "You are a software agent participating in a concurrency stress test. "
            "Return exactly one JSON object with keys: commit_message, ready_to_commit, edits. "
            "Each edit must contain file_path, action, content. "
            "Only use paths from the allowed path list. "
            "Do not explain your answer."
        )
        user_prompt = {
            "task": self.task_description,
            "task_profile": self.task_profile,
            "agent_id": self.agent_id,
            "task_id": self.task_id,
            "allowed_paths": self._allowed_paths(),
            "required_behavior": [
                "Produce one or two file edits.",
                "Write plain text content only.",
                "Set ready_to_commit to true.",
                "Keep content under 30 lines total.",
            ],
        }

        request = urllib.request.Request(
            f"{self.base_url}/chat/completions",
            data=json.dumps(
                {
                    "model": self.model,
                    "temperature": self.temperature,
                    "messages": [
                        {"role": "system", "content": system_prompt},
                        {"role": "user", "content": json.dumps(user_prompt)},
                    ],
                }
            ).encode(),
            headers={
                "Authorization": f"Bearer {self.api_key}",
                "Content-Type": "application/json",
            },
            method="POST",
        )

        self.session.tool_calls_made += 1
        with urllib.request.urlopen(request, timeout=self.timeout_s) as response:
            payload = json.loads(response.read().decode())

        usage = payload.get("usage") or {}
        self.session.model_tokens_used += int(usage.get("total_tokens") or 0)
        content = self._coerce_message_content(payload)
        return self._extract_json_object(content)

    def get_next_action(self) -> Optional[List[FileEdit]]:
        if self.generated_plan:
            return None
        if self.session is None:
            raise RuntimeError("session must be bound before generating actions")

        self.generated_plan = True
        plan = self._call_model()
        self.commit_message = str(plan.get("commit_message") or self.commit_message)
        edits = []
        allowed_paths = set(self._allowed_paths())
        for raw_edit in plan.get("edits") or []:
            file_path = str(raw_edit["file_path"])
            if file_path not in allowed_paths:
                raise ValueError(f"Model selected disallowed path: {file_path}")
            edits.append(
                FileEdit(
                    file_path=file_path,
                    action=str(raw_edit.get("action") or "modify"),
                    content=str(raw_edit.get("content") or ""),
                )
            )

        if not edits:
            raise ValueError("Model returned zero edits")
        return edits

    def should_commit(self) -> bool:
        return self.generated_plan

    def explain_result(self, session: AgentSession) -> str:
        if session.success:
            return (
                f"{self.agent_id}: committed {len(session.file_edits)} LLM edits "
                f"with {session.model_tokens_used} tokens"
            )
        return f"{self.agent_id}: failed ({session.failure_type})"


def socket_accepting_connections(path: str) -> bool:
    if not os.path.exists(path):
        return False
    try:
        probe = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        probe.settimeout(0.5)
        probe.connect(path)
        probe.close()
        return True
    except OSError:
        return False


def resolve_socket_path(input_socket: str) -> str:
    if socket_accepting_connections(input_socket):
        return input_socket

    candidates: List[tuple[float, str]] = []
    for path in glob.glob("/tmp/simgit-slo-*/control.sock"):
        if socket_accepting_connections(path):
            candidates.append((os.path.getmtime(path), path))

    if candidates:
        candidates.sort(reverse=True)
        resolved = candidates[0][1]
        if input_socket != resolved:
            logger.info("Socket '%s' unavailable; resolved latest live socket: %s", input_socket, resolved)
        return resolved

    return input_socket


def detect_error_type(raw_error: Any) -> str:
    error_text = raw_error if isinstance(raw_error, str) else json.dumps(raw_error)
    normalized = error_text.lower()
    if "lock_conflict" in normalized or "borrow conflict" in normalized:
        return "lock_conflict"
    if "merge" in normalized:
        return "merge_conflict"
    if "timeout" in normalized:
        return "timeout"
    if "not found" in normalized:
        return "session_not_found"
    return "other"


def apply_file_edits(mount_path: str, edits: List[FileEdit]) -> List[FileEdit]:
    applied = []
    for edit in edits:
        target_path = Path(mount_path) / edit.file_path
        target_path.parent.mkdir(parents=True, exist_ok=True)

        # LLM outputs may use synonyms; normalize them before applying edits.
        action = edit.action.strip().lower()
        alias_map = {
            "update": "modify",
            "edit": "modify",
            "write": "modify",
            "overwrite": "modify",
            "append": "modify",
            "remove": "delete",
        }
        action = alias_map.get(action, action)

        if action == "delete":
            if target_path.exists():
                target_path.unlink()
            applied.append(FileEdit(file_path=edit.file_path, action="delete", size_bytes=0, content_sample=""))
            continue

        if action not in {"create", "modify"}:
            raise ValueError(f"Unsupported edit action: {edit.action}")

        target_path.write_text(edit.content, encoding="utf-8")
        applied.append(
            FileEdit(
                file_path=edit.file_path,
                action=action,
                size_bytes=len(edit.content.encode("utf-8")),
                content_sample=edit.content[:100],
            )
        )

    return applied


def percentile(values: List[float], ratio: float) -> float:
    if not values:
        return 0.0
    if len(values) == 1:
        return float(values[0])
    ordered = sorted(values)
    rank = (len(ordered) - 1) * ratio
    lower = math.floor(rank)
    upper = math.ceil(rank)
    if lower == upper:
        return float(ordered[lower])
    lower_value = ordered[lower]
    upper_value = ordered[upper]
    return float(lower_value + (upper_value - lower_value) * (rank - lower))


class RealAgentHarness:
    """Run multiple agents concurrently and summarize the dominant weakness."""

    def __init__(
        self,
        socket_path: str,
        num_agents: int,
        report_out: str,
        max_commit_retries: int,
        commit_retry_backoff_ms: int,
        use_commit_barrier: bool,
        max_workers: Optional[int] = None,
    ):
        self.socket_path = socket_path
        self.num_agents = num_agents
        self.report_out = report_out
        self.max_commit_retries = max_commit_retries
        self.commit_retry_backoff_ms = commit_retry_backoff_ms
        self.use_commit_barrier = use_commit_barrier
        self.max_workers = max_workers or num_agents
        self.rpc_client = SimgitRPCClient(socket_path)
        self.sessions: List[AgentSession] = []

    def run(self, agent_factory) -> bool:
        logger.info("Starting real-agent harness with %s agents", self.num_agents)
        logger.info("Daemon socket: %s", self.socket_path)

        barrier = (
            threading.Barrier(self.num_agents)
            if self.use_commit_barrier and self.num_agents > 1
            else None
        )
        future_to_agent: Dict[concurrent.futures.Future[AgentSession], str] = {}

        with concurrent.futures.ThreadPoolExecutor(max_workers=self.max_workers) as executor:
            for _ in range(self.num_agents):
                agent_id = f"agent-{uuid.uuid4().hex[:8]}"
                future = executor.submit(self._run_single_agent, agent_factory, agent_id, barrier)
                future_to_agent[future] = agent_id

            for future in concurrent.futures.as_completed(future_to_agent):
                agent_id = future_to_agent[future]
                try:
                    session = future.result()
                except Exception as exc:
                    session = AgentSession(agent_id=agent_id, task_id=f"task-{uuid.uuid4().hex[:8]}")
                    session.failure_type = "worker_exception"
                    session.notes.append(str(exc))
                    session.completed_at = time.time()
                    session.total_duration_s = session.completed_at - session.created_at
                    if barrier is not None:
                        try:
                            barrier.abort()
                        except threading.BrokenBarrierError:
                            pass
                self.sessions.append(session)

        self._write_report()
        return bool(self.sessions) and all(session.success for session in self.sessions)

    def _run_single_agent(
        self,
        agent_factory,
        agent_id: str,
        barrier: Optional[threading.Barrier],
    ) -> AgentSession:
        task_id = f"task-{uuid.uuid4().hex[:8]}"
        session = AgentSession(agent_id=agent_id, task_id=task_id)
        agent: Agent = agent_factory(agent_id, task_id)
        agent.session = session

        if not agent.initialize(self.rpc_client):
            session.failure_type = "agent_initialize_failed"
            session.completed_at = time.time()
            session.total_duration_s = session.completed_at - session.created_at
            return session

        created, result = self.rpc_client.session_create(task_id, agent_id)
        if not created:
            session.failure_type = "session_create_failed"
            session.notes.append(str(result.get("error", "unknown session.create failure")))
            session.completed_at = time.time()
            session.total_duration_s = session.completed_at - session.created_at
            return session

        session.session_id = result.get("session_id")
        session.mount_path = result.get("mount_path")
        logger.info("Created session for %s: %s", agent_id, session.session_id)

        if not session.mount_path:
            session.failure_type = "missing_mount_path"
            session.completed_at = time.time()
            session.total_duration_s = session.completed_at - session.created_at
            return session

        try:
            while not agent.should_commit():
                edits = agent.get_next_action()
                if edits is None:
                    break
                session.file_edits.extend(apply_file_edits(session.mount_path, edits))

            if not agent.should_commit():
                session.failure_type = "no_commit_ready"
                return session

            if barrier is not None:
                try:
                    barrier.wait(timeout=30)
                except threading.BrokenBarrierError:
                    session.failure_type = "commit_barrier_broken"
                    return session

            commit_message = getattr(agent, "commit_message", None)
            for attempt_num in range(1, self.max_commit_retries + 1):
                attempt = CommitAttempt(
                    attempt_num=attempt_num,
                    timestamp_start=time.time(),
                    edits_count=len(session.file_edits),
                )
                succeeded, commit_result = self.rpc_client.session_commit(
                    session.session_id,
                    branch_name=f"simgit/real-agent/{session.agent_id}",
                    message=commit_message or f"real-agent harness commit ({session.agent_id})",
                )
                attempt.timestamp_end = time.time()
                attempt.duration_ms = (attempt.timestamp_end - attempt.timestamp_start) * 1000

                if succeeded:
                    attempt.success = True
                    session.commit_attempts.append(attempt)
                    session.success = True
                    if attempt_num > 1:
                        session.conflicts_resolved += 1
                    break

                raw_error = commit_result.get("error", "unknown")
                attempt.error_type = detect_error_type(raw_error)
                attempt.error_message = raw_error if isinstance(raw_error, str) else json.dumps(raw_error)
                session.commit_attempts.append(attempt)
                session.retries_count += 1

                if attempt.error_type in {"lock_conflict", "merge_conflict"}:
                    session.conflicts_encountered += 1
                    for edit in session.file_edits:
                        session.conflict_paths.append(edit.file_path)
                    if attempt_num < self.max_commit_retries:
                        time.sleep((self.commit_retry_backoff_ms * attempt_num) / 1000.0)
                        continue

                session.failure_type = attempt.error_type
                break

            if not session.success and not session.failure_type:
                session.failure_type = "commit_retries_exhausted"
        except urllib.error.HTTPError as exc:
            session.failure_type = "llm_http_error"
            session.notes.append(exc.read().decode(errors="ignore"))
        except urllib.error.URLError as exc:
            session.failure_type = "llm_network_error"
            session.notes.append(str(exc))
        except Exception as exc:
            session.failure_type = "agent_runtime_error"
            session.notes.append(str(exc))
        finally:
            if not session.success and session.session_id:
                self.rpc_client.session_abort(session.session_id)
            session.completed_at = time.time()
            session.total_duration_s = session.completed_at - session.created_at
            logger.info("%s: %s", session.agent_id, agent.explain_result(session))

        return session

    def _weakness_summary(self) -> Dict[str, Any]:
        failure_counts = Counter(session.failure_type or "success" for session in self.sessions)
        attempt_durations = [
            attempt.duration_ms
            for session in self.sessions
            for attempt in session.commit_attempts
        ]
        hot_paths = Counter(
            edit.file_path
            for session in self.sessions
            for edit in session.file_edits
        )
        total_attempts = len(attempt_durations)
        total_conflicts = sum(session.conflicts_encountered for session in self.sessions)
        success_count = sum(1 for session in self.sessions if session.success)
        non_conflict_failures = sum(
            count
            for failure_type, count in failure_counts.items()
            if failure_type not in {"success", "lock_conflict", "merge_conflict"}
        )
        conflict_rate = (total_conflicts / total_attempts) if total_attempts else 0.0

        recommendation = "collect_more_data"
        # Prioritize overlap-driven recommendations when conflict pressure is clearly dominant.
        if conflict_rate >= 0.30 and hot_paths:
            recommendation = "edit_isolation_or_ast_locking"
        elif percentile(attempt_durations, 0.95) >= 5000 and conflict_rate < 0.20:
            recommendation = "scheduler_or_backpressure"
        elif conflict_rate >= 0.15:
            recommendation = "task_routing_or_overlap_policy"
        elif non_conflict_failures >= max(2, math.ceil(len(self.sessions) * 0.4)):
            recommendation = "harness_or_protocol_hardening"

        dominant_failure_type = failure_counts.most_common(1)[0][0] if failure_counts else None
        return {
            "dominant_failure_type": dominant_failure_type,
            "failure_counts": dict(failure_counts),
            "conflict_rate": conflict_rate,
            "retry_rate_per_success": (
                sum(session.retries_count for session in self.sessions) / success_count
                if success_count
                else 0.0
            ),
            "commit_latency_ms": {
                "p50": percentile(attempt_durations, 0.50),
                "p95": percentile(attempt_durations, 0.95),
                "p99": percentile(attempt_durations, 0.99),
            },
            "top_paths": hot_paths.most_common(10),
            "recommendation": recommendation,
        }

    def _write_report(self) -> None:
        report = {
            "timestamp": datetime.now().isoformat(),
            "total_agents": self.num_agents,
            "completed_agents": len(self.sessions),
            "successful_agents": sum(1 for session in self.sessions if session.success),
            "sessions": [session.to_dict() for session in self.sessions],
            "summary": {
                "success_rate": (
                    sum(1 for session in self.sessions if session.success) / len(self.sessions)
                    if self.sessions
                    else 0.0
                ),
                "avg_duration_s": (
                    sum(session.total_duration_s for session in self.sessions) / len(self.sessions)
                    if self.sessions
                    else 0.0
                ),
                "total_edits": sum(len(session.file_edits) for session in self.sessions),
                "total_commit_attempts": sum(len(session.commit_attempts) for session in self.sessions),
                "total_conflicts": sum(session.conflicts_encountered for session in self.sessions),
            },
            "weakness_summary": self._weakness_summary(),
        }

        with open(self.report_out, "w", encoding="utf-8") as handle:
            json.dump(report, handle, indent=2)

        logger.info("Report written to %s", self.report_out)
        logger.info("Summary: %s", report["summary"])
        logger.info("Weakness summary: %s", report["weakness_summary"])


def main() -> None:
    parser = argparse.ArgumentParser(
        description="Real-agent harness for mock-to-reality calibration",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""\
Examples:
  python3 tests/real_agent_harness.py \
    --agents 5 --task-profile hotspot-file --commit-barrier \
    --report-out /tmp/real_hotspot.json

    SIMGIT_LLM_API_KEY=... python3 tests/real_agent_harness.py \
        --agent-type llm --model gpt-4.1 --agents 5 \
    --task-profile mixed --task "Refactor the benchmark text files clearly" \
    --report-out /tmp/real_llm.json
""",
    )
    parser.add_argument("--agents", type=int, default=5, help="Number of agents to run")
    parser.add_argument(
        "--socket",
        default=os.environ.get("SIMGIT_SOCKET", "/tmp/simgit-slo-latest/control.sock"),
        help="Path to simgit RPC socket",
    )
    parser.add_argument("--report-out", default="/tmp/real_agent_report.json", help="JSON report path")
    parser.add_argument(
        "--agent-type",
        default="deterministic",
        choices=["deterministic", "llm"],
        help="Agent type to run",
    )
    parser.add_argument("--model", help="Model name for LLM mode")
    parser.add_argument(
        "--task",
        default="Create and refine benchmark text edits for a concurrency stress run.",
        help="Task description passed to the LLM",
    )
    parser.add_argument(
        "--task-profile",
        default="hotspot-file",
        choices=["hotspot-file", "disjoint-files", "mixed", "sharded-hotspot"],
        help="Overlap profile for generated edits (sharded-hotspot: per-agent shard files under bench/hotspot-shards/)",
    )
    parser.add_argument(
        "--shared-path",
        default="bench/shared_hotspot.txt",
        help="Shared file path used in hotspot or mixed mode",
    )
    parser.add_argument(
        "--commit-barrier",
        action="store_true",
        help="Wait for all agents to finish writing before any commit attempt starts",
    )
    parser.add_argument("--workers", type=int, help="Worker threads for the harness")
    parser.add_argument(
        "--max-commit-retries",
        type=int,
        default=3,
        help="Maximum commit retries per agent",
    )
    parser.add_argument(
        "--commit-retry-backoff-ms",
        type=int,
        default=250,
        help="Linear backoff per retry attempt in milliseconds",
    )
    parser.add_argument(
        "--llm-base-url",
        default=os.environ.get("SIMGIT_LLM_BASE_URL")
        or os.environ.get("OPENAI_BASE_URL")
        or "https://api.openai.com/v1",
        help="OpenAI-compatible base URL for chat completions",
    )
    parser.add_argument(
        "--llm-timeout-secs",
        type=float,
        default=float(os.environ.get("SIMGIT_LLM_TIMEOUT_SECS", "60")),
        help="HTTP timeout for LLM requests",
    )
    args = parser.parse_args()

    args.socket = resolve_socket_path(args.socket)
    if not socket_accepting_connections(args.socket):
        logger.error("Daemon socket is unavailable or not accepting connections: %s", args.socket)
        logger.error("Start simgitd first or run tests/nightly-slo-gate.sh to create a fresh daemon socket")
        sys.exit(2)

    harness = RealAgentHarness(
        socket_path=args.socket,
        num_agents=args.agents,
        report_out=args.report_out,
        max_commit_retries=args.max_commit_retries,
        commit_retry_backoff_ms=args.commit_retry_backoff_ms,
        use_commit_barrier=args.commit_barrier,
        max_workers=args.workers,
    )

    if args.agent_type == "deterministic":
        success = harness.run(
            lambda agent_id, task_id: DeterministicTestAgent(
                agent_id,
                task_id,
                task_profile=args.task_profile,
                shared_path=args.shared_path,
            )
        )
    else:
        api_key = os.environ.get("SIMGIT_LLM_API_KEY") or os.environ.get("OPENAI_API_KEY")
        success = harness.run(
            lambda agent_id, task_id: LLMAgent(
                agent_id,
                task_id,
                model=args.model or "",
                task_description=args.task,
                task_profile=args.task_profile,
                shared_path=args.shared_path,
                api_key=api_key,
                base_url=args.llm_base_url,
                timeout_s=args.llm_timeout_secs,
            )
        )

    sys.exit(0 if success else 1)


if __name__ == "__main__":
    main()
