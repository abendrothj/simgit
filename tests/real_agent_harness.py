#!/usr/bin/env python3

"""
Real-Agent Harness for Mock-to-Reality Calibration

This harness runs real agents against simgit to compare behavior against mock swarm baselines.
Agents can be LLM-powered (OpenAI, Claude, local models) or deterministic (for CI validation).

Usage (placeholder for future LLM integration):
    python3 tests/real_agent_harness.py \\
        --agent-type llm \\
        --model gpt-4o \\
        --agents 5 \\
        --task-description "Fix Python linting errors" \\
        --report-out /tmp/real_agent_results.json

Current status: Skeleton scaffold with session management and metrics capture.
LLM backend integration deferred pending model selection and auth setup.
"""

import json
import socket
import time
import uuid
import os
import sys
import argparse
import logging
from dataclasses import dataclass, asdict, field
from typing import Optional, Dict, List, Any
from abc import ABC, abstractmethod
from datetime import datetime
from pathlib import Path

# Setup logging
logging.basicConfig(
    level=logging.INFO,
    format='%(asctime)s [%(levelname)s] %(message)s'
)
logger = logging.getLogger(__name__)


# ============================================================================
# Data Structures
# ============================================================================

@dataclass
class FileEdit:
    """Track a single file edit within a session."""
    file_path: str
    action: str  # "create", "modify", "delete"
    size_bytes: Optional[int] = None
    content_sample: str = ""  # First 100 chars for inspection


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
    
    def finalize(self):
        """Calculate duration and mark as complete."""
        self.duration_ms = (self.timestamp_end - self.timestamp_start) * 1000
        self.timestamp_end = time.time()


@dataclass
class AgentSession:
    """Track one agent's interaction with simgit."""
    agent_id: str
    task_id: str
    session_id: Optional[str] = None
    
    # Lifecycle tracking
    created_at: float = field(default_factory=time.time)
    completed_at: Optional[float] = None
    
    # Edit and commit tracking
    file_edits: List[FileEdit] = field(default_factory=list)
    commit_attempts: List[CommitAttempt] = field(default_factory=list)
    
    # Metrics
    total_duration_s: float = 0.0
    success: bool = False
    failure_type: Optional[str] = None
    
    # Conflict tracking
    conflicts_encountered: int = 0
    conflicts_resolved: int = 0
    conflict_paths: List[str] = field(default_factory=list)
    
    # Metadata for later analysis
    model_used: Optional[str] = None
    model_tokens_used: int = 0
    tool_calls_made: int = 0
    retries_count: int = 0
    
    def to_dict(self):
        return {
            'agent_id': self.agent_id,
            'task_id': self.task_id,
            'session_id': self.session_id,
            'created_at': datetime.fromtimestamp(self.created_at).isoformat(),
            'completed_at': datetime.fromtimestamp(self.completed_at).isoformat() if self.completed_at else None,
            'total_duration_s': self.total_duration_s,
            'success': self.success,
            'failure_type': self.failure_type,
            'file_edits': [asdict(e) for e in self.file_edits],
            'commit_attempts': [asdict(c) for c in self.commit_attempts],
            'conflicts_encountered': self.conflicts_encountered,
            'conflicts_resolved': self.conflicts_resolved,
            'model_used': self.model_used,
            'model_tokens_used': self.model_tokens_used,
            'tool_calls_made': self.tool_calls_made,
            'retries_count': self.retries_count,
        }


# ============================================================================
# RPC Client
# ============================================================================

class SimgitRPCClient:
    """Low-level JSON-RPC client for simgit daemon."""
    
    def __init__(self, socket_path: str, timeout_s: float = 30.0):
        self.socket_path = socket_path
        self.timeout_s = timeout_s
        self.request_id = 0
    
    def _send_request(self, method: str, params: Dict[str, Any]) -> Dict[str, Any]:
        """Send JSON-RPC request and receive response."""
        self.request_id += 1
        request = {
            "jsonrpc": "2.0",
            "id": self.request_id,
            "method": method,
            "params": params
        }
        
        try:
            sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            sock.settimeout(self.timeout_s)
            sock.connect(self.socket_path)
            
            payload = json.dumps(request) + '\n'
            sock.sendall(payload.encode())
            
            # Read response
            response_data = b""
            while True:
                try:
                    chunk = sock.recv(4096)
                    if not chunk:
                        break
                    response_data += chunk
                except socket.timeout:
                    break
            
            sock.close()
            
            if not response_data:
                return {"error": "No response from daemon"}
            
            response = json.loads(response_data.decode())
            return response
        
        except Exception as e:
            return {"error": str(e)}
    
    def session_create(self, task_id: str, agent_label: str, base_commit: Optional[str] = None) -> tuple[bool, str]:
        """Create a new session. Returns (success, session_id)."""
        result = self._send_request("session.create", {
            "task_id": task_id,
            "agent_label": agent_label,
            "base_commit": base_commit,
            "peers": False
        })
        
        if "error" in result:
            return False, result["error"]
        
        if "result" in result:
            return True, result["result"].get("session_id", "unknown")
        
        return False, f"Unexpected response: {result}"
    
    def session_flatten(self, session_id: str) -> tuple[bool, Dict[str, Any]]:
        """Commit a session (flatten to base). Returns (success, result_dict)."""
        result = self._send_request("session.flatten", {
            "session_id": session_id
        })
        
        if "error" in result:
            error_msg = result.get("error", "Unknown error")
            return False, {"error": error_msg}
        
        if "result" in result:
            return True, result["result"]
        
        return False, {"error": f"Unexpected response: {result}"}
    
    def session_diff(self, session_id: str) -> tuple[bool, Dict[str, Any]]:
        """Get diff for session. Returns (success, diff_dict)."""
        result = self._send_request("session.diff", {
            "session_id": session_id
        })
        
        if "error" in result:
            return False, {"error": result["error"]}
        
        if "result" in result:
            return True, result["result"]
        
        return False, {"error": f"Unexpected response: {result}"}


# ============================================================================
# Agent Interface
# ============================================================================

class Agent(ABC):
    """Abstract agent that can edit a repo via simgit."""
    
    def __init__(self, agent_id: str, task_id: str):
        self.agent_id = agent_id
        self.task_id = task_id
        self.session: Optional[AgentSession] = None
    
    @abstractmethod
    def initialize(self, rpc_client: SimgitRPCClient) -> bool:
        """Initialize the agent (e.g., auth with LLM, load model). Return True if success."""
        pass
    
    @abstractmethod
    def get_next_action(self) -> Optional[List[FileEdit]]:
        """
        Return the next set of file edits to make, or None if done.
        Should return a list of FileEdit objects.
        """
        pass
    
    @abstractmethod
    def should_commit(self) -> bool:
        """Return True if ready to commit current edits."""
        pass
    
    @abstractmethod
    def explain_result(self, session: AgentSession) -> str:
        """Explain outcome (for logging/analysis)."""
        pass


# ============================================================================
# Deterministic Test Agent (for immediate validation)
# ============================================================================

class DeterministicTestAgent(Agent):
    """
    Deterministic agent that creates a fixed file, modifies it, then commits.
    Used for immediate CI validation before LLM integration.
    """
    
    def __init__(self, agent_id: str, task_id: str, delay_between_edits_s: float = 0.1):
        super().__init__(agent_id, task_id)
        self.delay_between_edits_s = delay_between_edits_s
        self.action_count = 0
        self.phase = "create"  # "create", "modify", "commit"
    
    def initialize(self, rpc_client: SimgitRPCClient) -> bool:
        """No special initialization needed."""
        return True
    
    def get_next_action(self) -> Optional[List[FileEdit]]:
        """Generate deterministic edits."""
        time.sleep(self.delay_between_edits_s)
        
        if self.phase == "create":
            self.phase = "modify"
            return [FileEdit(
                file_path=f"deterministic_agent_{self.agent_id}_file.txt",
                action="create",
                size_bytes=100,
                content_sample="Deterministic test content"
            )]
        elif self.phase == "modify":
            self.phase = "commit"
            return [FileEdit(
                file_path=f"deterministic_agent_{self.agent_id}_file.txt",
                action="modify",
                size_bytes=200,
                content_sample="Modified deterministic test content"
            )]
        else:
            return None
    
    def should_commit(self) -> bool:
        """Commit after modify phase."""
        return self.phase == "commit"
    
    def explain_result(self, session: AgentSession) -> str:
        if session.success:
            return f"Agent {self.agent_id}: Successfully created and modified test file"
        else:
            return f"Agent {self.agent_id}: Failed to commit ({session.failure_type})"


# ============================================================================
# Harness Orchestrator
# ============================================================================

class RealAgentHarness:
    """Orchestrate multiple real agents against a simgit daemon."""
    
    def __init__(self, socket_path: str, num_agents: int = 5, report_out: str = "/tmp/real_agent_report.json"):
        self.socket_path = socket_path
        self.num_agents = num_agents
        self.report_out = report_out
        self.rpc_client = SimgitRPCClient(socket_path)
        self.sessions: List[AgentSession] = []
        self.agents: Dict[str, Agent] = {}
    
    def run(self, agent_factory=None) -> bool:
        """
        Run the harness with given agent factory.
        If agent_factory is None, uses DeterministicTestAgent for validation.
        """
        if agent_factory is None:
            agent_factory = DeterministicTestAgent
        
        logger.info(f"Starting real-agent harness with {self.num_agents} agents")
        logger.info(f"Daemon socket: {self.socket_path}")
        
        # Create agents
        for i in range(self.num_agents):
            agent_id = f"agent-{uuid.uuid4().hex[:8]}"
            task_id = f"task-{uuid.uuid4().hex[:8]}"
            
            agent = agent_factory(agent_id, task_id)
            
            if not agent.initialize(self.rpc_client):
                logger.error(f"Failed to initialize {agent_id}")
                continue
            
            session = AgentSession(agent_id=agent_id, task_id=task_id)
            self.agents[agent_id] = agent
            agent.session = session
            
            # Create simgit session
            success, session_id = self.rpc_client.session_create(task_id, agent_id)
            if not success:
                logger.error(f"Failed to create simgit session for {agent_id}: {session_id}")
                session.failure_type = "session_create_failed"
                self.sessions.append(session)
                continue
            
            session.session_id = session_id
            logger.info(f"Created session for {agent_id}: {session_id}")
            
            # Run agent workflow
            self._run_agent(agent, session)
            
            self.sessions.append(session)
        
        # Write report
        self._write_report()
        
        return True
    
    def _run_agent(self, agent: Agent, session: AgentSession) -> None:
        """Run one agent's workflow."""
        max_iterations = 10
        iteration = 0
        
        while iteration < max_iterations:
            iteration += 1
            
            # Get next action
            edits = agent.get_next_action()
            if edits is None:
                logger.info(f"{session.agent_id}: Agent indicates completion")
                break
            
            logger.info(f"{session.agent_id}: Making {len(edits)} file edits (iteration {iteration})")
            session.file_edits.extend(edits)
            
            # Check if ready to commit
            if agent.should_commit():
                logger.info(f"{session.agent_id}: Committing...")
                attempt = CommitAttempt(
                    attempt_num=len(session.commit_attempts) + 1,
                    timestamp_start=time.time(),
                    edits_count=len(edits)
                )
                
                success, result = self.rpc_client.session_flatten(session.session_id)
                attempt.timestamp_end = time.time()
                attempt.duration_ms = (attempt.timestamp_end - attempt.timestamp_start) * 1000
                
                if success:
                    attempt.success = True
                    session.success = True
                    logger.info(f"{session.agent_id}: Commit successful (p95={attempt.duration_ms:.0f}ms)")
                    break
                else:
                    # Extract error type
                    error = result.get("error", "unknown")
                    if "lock_conflict" in error.lower():
                        attempt.error_type = "lock_conflict"
                    elif "merge" in error.lower():
                        attempt.error_type = "merge_conflict"
                    else:
                        attempt.error_type = "other"
                    
                    attempt.error_message = error
                    session.commit_attempts.append(attempt)
                    session.retries_count += 1
                    
                    logger.warning(f"{session.agent_id}: Commit failed ({attempt.error_type}): {error}")
                    
                    # If contention, continue with more edits
                    if attempt.error_type == "lock_conflict":
                        session.conflicts_encountered += 1
                        continue
                    else:
                        session.failure_type = attempt.error_type
                        break
                
                session.commit_attempts.append(attempt)
        
        if not session.success and not session.failure_type:
            session.failure_type = "max_iterations"
        
        session.completed_at = time.time()
        session.total_duration_s = session.completed_at - session.created_at
        
        logger.info(f"{session.agent_id}: Workflow complete - {agent.explain_result(session)}")
    
    def _write_report(self) -> None:
        """Write JSON report with all session results."""
        report = {
            "timestamp": datetime.now().isoformat(),
            "total_agents": self.num_agents,
            "completed_agents": len(self.sessions),
            "successful_agents": sum(1 for s in self.sessions if s.success),
            "sessions": [s.to_dict() for s in self.sessions],
            "summary": {
                "success_rate": sum(1 for s in self.sessions if s.success) / len(self.sessions) if self.sessions else 0,
                "avg_duration_s": sum(s.total_duration_s for s in self.sessions) / len(self.sessions) if self.sessions else 0,
                "total_edits": sum(len(s.file_edits) for s in self.sessions),
                "total_commits": sum(len(s.commit_attempts) for s in self.sessions),
                "total_conflicts": sum(s.conflicts_encountered for s in self.sessions),
            }
        }
        
        with open(self.report_out, 'w') as f:
            json.dump(report, f, indent=2)
        
        logger.info(f"Report written to {self.report_out}")
        logger.info(f"Summary: {report['summary']}")


# ============================================================================
# Main
# ============================================================================

def main():
    parser = argparse.ArgumentParser(
        description="Real-agent harness for mock-to-reality calibration",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""\
Examples:
  # Deterministic test (current)
  python3 tests/real_agent_harness.py --agents 5 --report-out /tmp/real_test.json
  
  # LLM-powered (future)
  python3 tests/real_agent_harness.py \\
    --agent-type llm --model gpt-4o \\
    --agents 5 --task "Fix lint errors" \\
    --report-out /tmp/real_llm_results.json
"""
    )
    
    parser.add_argument('--agents', type=int, default=5,
                        help='Number of agents to run (default: 5)')
    parser.add_argument('--socket', default=os.environ.get('SIMGIT_SOCKET', '/tmp/simgit-slo-latest/control.sock'),
                        help='Path to simgit RPC socket')
    parser.add_argument('--report-out', default='/tmp/real_agent_report.json',
                        help='Path to write JSON report')
    parser.add_argument('--agent-type', default='deterministic', choices=['deterministic', 'llm'],
                        help='Type of agent to use (default: deterministic for CI validation)')
    parser.add_argument('--model', help='Model to use (for LLM agent, future feature)')
    parser.add_argument('--task', help='Task description (for LLM agent, future feature)')
    
    args = parser.parse_args()
    
    # Check socket exists
    if not os.path.exists(args.socket):
        logger.error(f"Daemon socket not found: {args.socket}")
        logger.error("Make sure daemon is running before starting harness")
        sys.exit(2)
    
    # Create harness
    harness = RealAgentHarness(
        socket_path=args.socket,
        num_agents=args.agents,
        report_out=args.report_out
    )
    
    # Run with appropriate agent type
    if args.agent_type == 'deterministic':
        success = harness.run(agent_factory=DeterministicTestAgent)
    elif args.agent_type == 'llm':
        logger.error("LLM agent not yet implemented")
        logger.error("Please implement LLMAgent class and update agent_factory")
        sys.exit(1)
    
    sys.exit(0 if success else 1)


if __name__ == '__main__':
    main()
