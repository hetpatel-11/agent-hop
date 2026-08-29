# Plugins

Drop a folder under `~/.agent-hop/plugins/<name>/` with `plugin.toml`.

```toml
name = "mine"

# Extra Ctrl+B chord. Built-in letters (n, p, %, ", …) always win.
[[bind]]
chord = "t"
action = "split-vertical"   # split-vertical | split-horizontal | next-pane | zoom

[[bind]]
chord = "e"
shell = "notify-send agent-hop plugin-fired"
```

List what is loaded:

```bash
ah plugin list
```

## Detection files

A plugin may also ship `detect.toml` (same schema as `~/.agent-hop/detect/*.toml`). Those rules run before the built-in Rust matchers.

```toml
agent = "cursor"
version = 1

[[rule]]
status = "working"
contains = "Generating"

[[rule]]
status = "blocked"
contains = "Allow this"

[[rule]]
status = "done"
contains = "conversation ended"
```

Statuses: `idle`, `working`, `blocked`, `done`, `unknown`.

Herdr-style remote manifests are also read from `~/.local/state/agent-hop/agent-detection/remote/` if present. Override the search path with `AH_DETECT_DIR`.
