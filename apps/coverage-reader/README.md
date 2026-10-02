# Coverage reader

A local package example with one declared skill and no external MCP servers.

```sh
dust run ./apps/coverage-reader 'discovered=290 observed=100 distinct items'
dust pack ./apps/coverage-reader
dust run ./coverage-reader-0.1.0.dustpkg 'discovered=290 observed=100 distinct items'
```

Results depend on the selected model. The runtime tests use mock model responses and do not measure model coverage reasoning.
