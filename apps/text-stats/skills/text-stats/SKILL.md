---
name: text-stats
description: Count user-supplied text with this app's declared text_stats tool.
---

Call `app__text_stats` with `{"text": "the exact user-supplied text"}`.
Return the tool's `characters`, `words`, and `lines` counts as one JSON object.
Do not add task instructions to the text being measured or estimate counts yourself.
If the tool fails, report the failure rather than inventing counts.

The interpreter is `python3`; it must already be installed on the execution host.
