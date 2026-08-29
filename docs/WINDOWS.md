# Windows

`ah` ships a Windows x64 binary (`ah-windows-x64.exe`). The TUI, hop, and search work. A few mux features are Unix-only today.

## What works

- `ah`, `ah claude` / `ah cursor` / …, hop, search, `ah resume`
- Tabs and workspaces in the live process
- Screen-matching status (idle / working / blocked / done / unknown)
- `ah worktree` if Git for Windows is on `PATH`
- `ah remote user@host` if OpenSSH is on `PATH`

## What does not

The background mux (`ah` detaching with `Ctrl+B q`, `ah server`, attach over `attach.sock`, pane CLI via `$AH_SOCK`) uses Unix domain sockets. Those paths are compiled out on native Windows.

Workarounds:

1. **WSL2** — install the Linux `ah` inside the distro. Detach, remote, and pane CLI work there.
2. Stay attached — run `ah` in the terminal and do not detach. Tabs still live for that process.

`install.sh` is a bash installer. On Windows use the [GitHub Release](https://github.com/hetpatel-11/agent-hop/releases/latest) exe, or `npm install -g agent-hop`.

npm shims (`.cmd`) are spawned through `cmd.exe /d /s /c` so `CreateProcessW` does not hit os error 193.
