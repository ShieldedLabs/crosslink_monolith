# Tools

Developer tools for the monolith. Each one documents its own usage in its header or `--help`.

| Tool | What it does |
| --- | --- |
| `await_scan.py` | Finds async code that never awaits: `async fn`s in trait impls, which clippy's `unused_async` skips, and `async { }` blocks that are ready as soon as they are polled. Run `python3 tools/await_scan.py [ROOT] [--tests]`. |
