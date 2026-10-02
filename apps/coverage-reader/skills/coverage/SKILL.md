---
name: coverage
description: Assess crawl coverage when input contains discovered, observed, or remaining item counts.
license: MIT
---

Read [coverage rules](references/rules.md) and [result template](assets/result.json).
Preserve supplied counts and source identifiers. Distinguish tool observations from conclusions.
When observations or total scope are missing, return unknown completeness with an explanation.
Scripts are optional reference code; this runtime does not execute skill scripts.
