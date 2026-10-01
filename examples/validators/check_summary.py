#!/usr/bin/env python3
"""Check the summary output contract; this does not verify factual accuracy."""
import json
import sys

try:
    record = json.load(sys.stdin)
    result = json.loads(record["output"])
    passed = (
        isinstance(result, dict)
        and set(result) == {"summary"}
        and isinstance(result["summary"], str)
        and bool(result["summary"].strip())
        and len(result["summary"]) <= 200
    )
    reason = "Checked exact summary key, nonempty string and 200-character bound"
except (ValueError, KeyError, TypeError):
    passed = False
    reason = "Output does not satisfy the JSON summary contract"
print(json.dumps({"passed": passed, "reason": reason}))
