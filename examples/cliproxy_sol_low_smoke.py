"""Exercise real Sol-low document reads and compaction, without a target audit.

Creates harmless text documents, reads them through a scoped stdio MCP, and checks
the returned markers against both disk contents and actual successful tool calls.
Credentials stay in the environment; reports and original transcripts are kept.
"""
import argparse
from collections import Counter
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import time
from urllib.parse import urlsplit, urlunsplit


MODEL = "gpt-6.1-sol(low)"
MCP = r'''
import hashlib
import json
import os
from pathlib import Path
import sys

root = Path(os.environ["DUST_SMOKE_DOCUMENTS"]).resolve(strict=True)
for line in sys.stdin:
    request = json.loads(line)
    if "id" not in request:
        continue
    method = request.get("method")
    if method == "initialize":
        result = {"protocolVersion": "2024-11-05", "capabilities": {"tools": {}},
                  "serverInfo": {"name": "smoke-documents", "version": "1.0.0"}}
    elif method == "tools/list":
        result = {"tools": [{"name": "read_document",
            "description": "Read a named smoke-test text document. Returns full text and SHA-256.",
            "inputSchema": {"type": "object", "additionalProperties": False,
                "properties": {"path": {"type": "string", "pattern": "^document-[0-9]+\\.txt$"}},
                "required": ["path"]}}]}
    elif method == "ping":
        result = {}
    elif method == "tools/call":
        params = request.get("params", {})
        try:
            name = params.get("arguments", {}).get("path", "")
            path = root / name
            if (params.get("name") != "read_document" or Path(name).name != name
                    or not name.startswith("document-") or not name.endswith(".txt")
                    or path.is_symlink() or path.resolve().parent != root):
                raise ValueError("not a smoke document")
            data = path.read_bytes()
            value = {"path": name, "sha256": hashlib.sha256(data).hexdigest(),
                     "text": data.decode("utf-8")}
            result = {"content": [{"type": "text", "text": json.dumps(value, ensure_ascii=False)}],
                      "isError": False}
        except (ValueError, OSError):
            result = {"content": [{"type": "text", "text": "Unknown smoke document"}], "isError": True}
    else:
        result = {}
    print(json.dumps({"jsonrpc": "2.0", "id": request["id"], "result": result}), flush=True)
'''


def configure_provider():
    env = dict(os.environ)
    env["OPENAI_BASE_URL"] = env.get("CLIPROXYAPI_BASE_URL", "http://127.0.0.1:8317/v1")
    key = env.get("CLIPROXYAPI_API_KEY") or env.get("OPENAI_API_KEY")
    if not key:
        config = Path(env.get("CLIPROXYAPI_CONFIG", "/home/jtjisgod/cliproxyapi/config.yaml"))
        if config.is_file():
            import yaml
            keys = (yaml.safe_load(config.read_text()) or {}).get("api-keys", [])
            key = keys[0] if keys else None
    if not isinstance(key, str) or not key:
        raise ValueError("Set CLIPROXYAPI_API_KEY or CLIPROXYAPI_CONFIG")
    env["OPENAI_API_KEY"] = key
    return env


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--dust", default=shutil.which("dust"))
    parser.add_argument("--output", type=Path)
    parser.add_argument("--documents", type=int, default=12)
    parser.add_argument("--chars", type=int, default=10000)
    parser.add_argument("--window", type=int, default=32768)
    compaction = parser.add_mutually_exclusive_group()
    compaction.add_argument("--require-compaction", action="store_true", default=True)
    compaction.add_argument("--allow-no-compaction", action="store_false", dest="require_compaction",
                            help="Only verify reads/output; do not claim compaction was exercised")
    options = parser.parse_args()
    if not options.dust or not 1 <= options.documents <= 20 or not 512 <= options.chars <= 12000:
        parser.error("Require dust, 1..20 documents, and 512..12000 characters")
    stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    output = (options.output or Path(".dustagent/smoke") / stamp).resolve()
    output.mkdir(parents=True, exist_ok=False)
    documents = output / "documents"
    documents.mkdir()
    expected = []
    # Quotes, newlines, slashes and UTF-8 exercise serialized-context estimation.
    padding = '회의 메모: "blue notebook"; local path notes/archive.\n'
    for number in range(1, options.documents + 1):
        name = f"document-{number}.txt"
        marker = hashlib.sha256(f"smoke-{number}".encode()).hexdigest()[:16]
        heading = f"Document {number}\nMARKER={marker}\n"
        body = heading + (padding * (options.chars // len(padding) + 1))[:options.chars]
        (documents / name).write_text(body)
        expected.append({"path": name, "marker": marker})
    manifest = {
        "package": {"name": "sol-low-document-smoke", "version": "1.0.0"},
        "default_model": MODEL, "max_turns": 64, "timeout_ms": 3600000,
        "tool_timeout_ms": 60000, "output_format": "raw_json",
        "compaction": {"context_window_tokens": options.window},
        "system_prompt": (
            "Extract MARKER from the supplied harmless text documents. Read each document "
            "exactly once with documents__read_document. You may batch reads. Preserve the "
            "extracted path/marker pairs when context is summarized. Return only JSON with "
            "one key documents containing objects with path and marker. Do not use clock, "
            "sleep, environment or UUID tools; this task needs only document reads."
        ),
        "mcp_servers": {"documents": {"command": "python3", "args": ["-c", MCP],
                                      "env": {"DUST_SMOKE_DOCUMENTS": str(documents)}}},
    }
    package = output / "package"
    package.mkdir()
    (package / "app.json").write_text(json.dumps(manifest, ensure_ascii=False, indent=2))
    binary = str(Path(options.dust).resolve())
    archive = output / "document-smoke.dustpkg"
    subprocess.run([binary, "pack", str(package), "-o", str(archive)],
                   check=True, capture_output=True, text=True, timeout=30)
    task = {"documents": [item["path"] for item in expected],
            "instruction": "Read all listed documents. Extract their MARKER values and return the requested JSON."}
    (output / "task.json").write_text(json.dumps(task, indent=2))
    started = time.monotonic()
    env = configure_provider()
    env["DUST_HISTORY_HOME"] = str(output / "history")
    process = subprocess.run([binary, "run", str(archive), "--json", "--report",
                              str(output / "dust-report.json"), "-m", MODEL, json.dumps(task)],
                             env=env, cwd=output, capture_output=True,
                             text=True, timeout=3610)
    (output / "stderr.log").write_text(process.stderr)
    report_path = output / "dust-report.json"
    if not report_path.is_file():
        raise RuntimeError(f"Dust exited {process.returncode} without report; inspect {output}")
    report = json.loads(report_path.read_text())
    reads = [call for call in report.get("tool_calls", [])
             if call["name"] == "documents__read_document" and call["status"] == "succeeded"]
    failures = []
    if process.returncode:
        failures.append(f"Dust exited with status {process.returncode}")
    expected_paths = {item["path"] for item in expected}
    counts = Counter(call["arguments"]["path"] for call in reads)
    if counts != Counter(expected_paths):
        failures.append("Document reads missing or repeated")
    for call in reads:
        envelope = json.loads(call["output"])
        if envelope.get("isError") or call.get("truncated"):
            failures.append("Document response errored or truncated")
            continue
        record = json.loads(envelope["content"][0]["text"])
        actual = (documents / record["path"]).read_bytes()
        if record["sha256"] != hashlib.sha256(actual).hexdigest() or record["text"] != actual.decode():
            failures.append("Read content does not match disk")
    if report["stop_reason"] != "completed":
        failures.append("Execution did not complete")
    try:
        result = json.loads(report.get("output") or "null")
        if not isinstance(result, dict) or sorted(result.get("documents", []), key=lambda x: x["path"]) != sorted(expected, key=lambda x: x["path"]):
            failures.append("Returned markers differ from documents")
    except (ValueError, TypeError, KeyError):
        result = None
        failures.append("Invalid result JSON")
    transcript = report.get("transcript")
    if not transcript or not Path(transcript["path"]).is_file():
        failures.append("Original transcript missing")
    if options.require_compaction and not report.get("compactions"):
        failures.append("No installed compaction was exercised")
    endpoint = urlsplit(env["OPENAI_BASE_URL"])
    public_endpoint = urlunsplit((endpoint.scheme, endpoint.netloc.rsplit("@", 1)[-1],
                                 endpoint.path, "", ""))
    summary = {
        "status": "passed" if not failures else "failed", "requested_model": MODEL,
        "configured_provider": "CLIProxyAPI-compatible endpoint", "configured_base_url": public_endpoint,
        "routing_evidence": "CLI model argument and configured endpoint; upstream backend not independently attested",
        "scope": "harmless document extraction; no target analysis",
        "compaction_required": options.require_compaction,
        "context_window_tokens": options.window, "document_count": options.documents,
        "process_returncode": process.returncode,
        "document_bytes": sum(path.stat().st_size for path in documents.iterdir()),
        "stop_reason": report["stop_reason"], "turns_used": report["turns_used"],
        "elapsed_seconds": round(time.monotonic() - started, 1), "successful_reads": len(reads),
        "tool_call_counts": dict(Counter(call["name"] for call in report.get("tool_calls", []))),
        "compactions": report.get("compactions", []),
        "compaction_attempts": report.get("compaction_attempts", []),
        "terminal_error": report.get("error"), "failures": failures,
        "result": result, "transcript": transcript,
        "archive_sha256": hashlib.sha256(archive.read_bytes()).hexdigest(),
        "binary_sha256": hashlib.sha256(Path(binary).read_bytes()).hexdigest(),
    }
    (output / "summary.json").write_text(json.dumps(summary, ensure_ascii=False, indent=2) + "\n")
    print(json.dumps(summary, ensure_ascii=False, indent=2))
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
