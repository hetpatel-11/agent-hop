# Agent detection manifests

Copy any of these into `~/.agent-hop/detect/` to overlay the built-in screen matchers. User files win.

```toml
agent = "cursor"
version = 1

[[rule]]
status = "working"
contains = "esc to interrupt"

[[rule]]
status = "done"
contains = "conversation ended"
```

See `docs/PLUGINS.md` for the full schema.
