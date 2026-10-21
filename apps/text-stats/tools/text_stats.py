"""App-owned tool: JSON stdin -> JSON stdout; diagnostics belong on stderr."""

import json
import sys

request = json.load(sys.stdin)
text = request["text"]
json.dump(
    {
        "characters": len(text),
        "words": len(text.split()),
        "lines": len(text.splitlines()),
    },
    sys.stdout,
    ensure_ascii=False,
)
sys.stdout.write("\n")
