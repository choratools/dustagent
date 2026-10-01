"""
Zero-dependency stdio Model Context Protocol (MCP) Client.
Implements JSON-RPC 2.0 over standard I/O streams.
"""

import json
import os
import subprocess
import sys
from typing import Any, Dict, List, Optional


class MicroMCPClient:
    """Lightweight MCP client executing over subprocess stdio without external SDK dependencies."""

    def __init__(self, command: str, args: Optional[List[str]] = None, env: Optional[Dict[str, str]] = None):
        cmd = [command] + (args or [])
        merged_env = os.environ.copy()
        if env:
            merged_env.update(env)

        self.proc = subprocess.Popen(
            cmd,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
            bufsize=1,
            env=merged_env
        )
        self._req_id = 0
        self._initialize()

    def _send_rpc(self, method: str, params: Optional[Dict[str, Any]] = None) -> Any:
        self._req_id += 1
        req = {
            "jsonrpc": "2.0",
            "id": self._req_id,
            "method": method
        }
        if params is not None:
            req["params"] = params

        payload = json.dumps(req) + "\n"
        self.proc.stdin.write(payload)
        self.proc.stdin.flush()

        response_line = self.proc.stdout.readline()
        if not response_line:
            raise RuntimeError(f"MCP server terminated unexpectedly while executing {method}")

        resp = json.loads(response_line)
        if "error" in resp:
            raise RuntimeError(f"MCP RPC Error ({method}): {resp['error']}")
        return resp.get("result")

    def _send_notification(self, method: str, params: Optional[Dict[str, Any]] = None) -> None:
        req = {
            "jsonrpc": "2.0",
            "method": method
        }
        if params is not None:
            req["params"] = params
        payload = json.dumps(req) + "\n"
        self.proc.stdin.write(payload)
        self.proc.stdin.flush()

    def _initialize(self) -> None:
        self._send_rpc("initialize", {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {
                "name": "dustagent",
                "version": "0.1.0"
            }
        })
        self._send_notification("notifications/initialized")

    def list_tools(self) -> List[Dict[str, Any]]:
        result = self._send_rpc("tools/list") or {}
        return result.get("tools", [])

    def call_tool(self, name: str, arguments: Dict[str, Any]) -> Any:
        result = self._send_rpc("tools/call", {
            "name": name,
            "arguments": arguments
        })
        return result

    def close(self) -> None:
        if self.proc and self.proc.poll() is None:
            try:
                self.proc.terminate()
                self.proc.wait(timeout=1.0)
            except Exception:
                self.proc.kill()

    def __enter__(self):
        return self

    def __exit__(self, exc_type, exc_val, exc_tb):
        self.close()
