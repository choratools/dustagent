"""
DustAgent Micro-Kernel Core.
Executes an Application Manifest with Scoped MCP Tools and LLM Gateway.
"""

import json
import os
import sys
import urllib.error
import urllib.request
from typing import Any, Dict, List, Optional, Tuple

from dustagent.mcp import MicroMCPClient
from dustagent.patch import FuzzyPatcher


class DustCore:
    """Ultra-lightweight execution kernel for Agent as an Application."""

    def __init__(self, manifest_path: str, model: Optional[str] = None):
        with open(manifest_path, "r", encoding="utf-8") as f:
            self.manifest: Dict[str, Any] = json.load(f)

        self.model = model or self.manifest.get("default_model", "gpt-4o-mini")
        self.mcp_clients: Dict[str, MicroMCPClient] = {}
        self._init_scoped_mcp()

    def _init_scoped_mcp(self) -> None:
        """Starts ONLY the MCP servers declared in the application manifest."""
        scoped_servers = self.manifest.get("mcp_servers", {})
        for name, config in scoped_servers.items():
            cmd = config.get("command")
            args = config.get("args", [])
            env = config.get("env")
            try:
                client = MicroMCPClient(command=cmd, args=args, env=env)
                self.mcp_clients[name] = client
            except Exception as e:
                sys.stderr.write(f"[dustagent] Warning: Failed to launch MCP server '{name}': {e}\n")

    def _get_mcp_tools(self) -> List[Dict[str, Any]]:
        """Collects tools from the isolated MCP instances."""
        tools = []
        for srv_name, client in self.mcp_clients.items():
            try:
                for tool in client.list_tools():
                    # Prefix with server name to avoid collision
                    tool_def = {
                        "name": f"{srv_name}__{tool['name']}",
                        "description": tool.get("description", ""),
                        "parameters": tool.get("inputSchema", {"type": "object", "properties": {}})
                    }
                    tools.append(tool_def)
            except Exception as e:
                sys.stderr.write(f"[dustagent] Warning: Failed to list tools from '{srv_name}': {e}\n")
        return tools

    def _execute_tool(self, full_tool_name: str, arguments: Dict[str, Any]) -> str:
        """Dispatches tool execution to the appropriate MCP server."""
        if "__" not in full_tool_name:
            raise ValueError(f"Invalid scoped tool name: {full_tool_name}")
        srv_name, tool_name = full_tool_name.split("__", 1)
        client = self.mcp_clients.get(srv_name)
        if not client:
            raise ValueError(f"MCP server '{srv_name}' not running")
        res = client.call_tool(tool_name, arguments)
        return json.dumps(res)

    def _call_llm(self, messages: List[Dict[str, Any]], tools: Optional[List[Dict[str, Any]]] = None) -> Dict[str, Any]:
        """Minimalist zero-dependency LLM Gateway (OpenAI-compatible format)."""
        api_key = os.environ.get("OPENAI_API_KEY")
        base_url = os.environ.get("OPENAI_BASE_URL", "https://api.openai.com/v1").rstrip("/")
        
        if not api_key:
            raise EnvironmentError("OPENAI_API_KEY environment variable is required.")

        url = f"{base_url}/chat/completions"
        payload: Dict[str, Any] = {
            "model": self.model,
            "messages": messages,
            "temperature": 0.0
        }
        if tools:
            payload["tools"] = [{"type": "function", "function": t} for t in tools]

        data = json.dumps(payload).encode("utf-8")
        req = urllib.request.Request(
            url,
            data=data,
            headers={
                "Content-Type": "application/json",
                "Authorization": f"Bearer {api_key}"
            },
            method="POST"
        )

        try:
            with urllib.request.urlopen(req, timeout=60.0) as resp:
                resp_data = json.loads(resp.read().decode("utf-8"))
                return resp_data["choices"][0]["message"]
        except urllib.error.HTTPError as e:
            err_body = e.read().decode("utf-8")
            raise RuntimeError(f"LLM API Error ({e.code}): {err_body}")

    def execute(self, user_input: str) -> str:
        """Executes the specialized task in a pure single-shot or micro-loop pipeline."""
        system_prompt = self.manifest.get("system_prompt", "You are a helpful specialized assistant.")
        messages = [
            {"role": "system", "content": system_prompt},
            {"role": "user", "content": user_input}
        ]

        tools = self._get_mcp_tools()
        max_tool_turns = 3

        while max_tool_turns > 0:
            max_tool_turns -= 1
            msg = self._call_llm(messages, tools if tools else None)
            tool_calls = msg.get("tool_calls")

            if not tool_calls:
                # Final content reached
                return msg.get("content", "")

            messages.append(msg)
            for tc in tool_calls:
                call_id = tc["id"]
                fn_name = tc["function"]["name"]
                fn_args = json.loads(tc["function"].get("arguments", "{}"))
                try:
                    tool_output = self._execute_tool(fn_name, fn_args)
                except Exception as ex:
                    tool_output = json.dumps({"error": str(ex)})

                messages.append({
                    "role": "tool",
                    "tool_call_id": call_id,
                    "content": tool_output
                })

        return ""

    def close(self) -> None:
        """Cleans up all MCP child processes."""
        for client in self.mcp_clients.values():
            client.close()

    def __enter__(self):
        return self

    def __exit__(self, exc_type, exc_val, exc_tb):
        self.close()
