"""Check a coverage report against counts explicitly supplied in the input.

This checker validates the report contract, not real page observation coverage.
"""
import json
import re
import sys


def verdict(decision, reason):
    print(json.dumps({"decision": decision, "reason": reason}))


request = json.load(sys.stdin)
try:
    output = json.loads(request["output"])
except (ValueError, TypeError):
    verdict("continue", "The response is not a JSON coverage report.")
    sys.exit(0)
if not isinstance(output, dict):
    verdict("continue", "The report must be a JSON object.")
    sys.exit(0)
fields = ("status", "discovered", "observed", "remaining", "reason")
if any(field not in output for field in fields):
    verdict("continue", "The report is missing required coverage fields.")
    sys.exit(0)
if not isinstance(output["reason"], str) or not output["reason"].strip():
    verdict("continue", "The report lacks an explanation of coverage.")
    sys.exit(0)
counts = dict(re.findall(r"\b(discovered|observed)=(\d+)\b", request["input"]))
if set(counts) != {"discovered", "observed"}:
    valid = output["status"] == "unknown" and all(output[key] is None for key in ("discovered", "observed", "remaining"))
    verdict("complete" if valid else "continue", "Unknown counts were preserved." if valid else "Input does not establish both counts; report unknown coverage without inventing counts.")
    sys.exit(0)
discovered, observed = int(counts["discovered"]), int(counts["observed"])
if observed > discovered:
    verdict("blocked", "Supplied observed count exceeds the discovered count; scope is inconsistent.")
    sys.exit(0)
expected = {"discovered": discovered, "observed": observed, "remaining": discovered-observed}
valid = all(type(output[key]) is int and output[key] == value for key, value in expected.items())
valid = valid and output["status"] == ("complete" if observed == discovered else "partial")
verdict("complete" if valid else "continue", "Report matches supplied counts; page observations are not independently verified." if valid else "Coverage fields or status contradict the supplied counts.")
