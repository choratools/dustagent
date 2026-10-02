# Coverage reader

A local package example with one declared skill and no external MCP servers.

```sh
dust run ./apps/coverage-reader 'discovered=290 observed=100 distinct items'
dust pack ./apps/coverage-reader
dust run ./coverage-reader-0.1.0.dustpkg 'discovered=290 observed=100 distinct items'
```

Results depend on the selected model. The runtime tests use mock model responses and do not measure model coverage reasoning.

The package includes a feedback checker. It requests correction when the JSON report contradicts supplied counts, blocks inconsistent input, and does not independently establish real page observations. Python 3 is required. The declared `${DUST_APP_ROOT}` checker path follows archive extraction locations. Working-state tools are enabled for execution-local notes.
