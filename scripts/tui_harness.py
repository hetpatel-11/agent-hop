#!/usr/bin/env python3
"""Real tmux-based test harness for the agent-hop switcher.

Runs `ah` inside a dedicated, isolated tmux server (its own socket, never
touching any other tmux session) and drives it with real keystrokes,
capturing precise rendered screen state via `tmux capture-pane` -- the
same technique Ben Swerdlow used to build pixel-accurate terminal UI
replicas for brainless.swerdlow.dev.

This is meaningfully more faithful than a bare scripted pty: tmux is a
real, production terminal multiplexer that does actual VT parsing and
answers terminal queries the way a real terminal does, which is exactly
the gap a scripted pty + the `pyte` Python library couldn't close.

Usage: see the `if __name__ == "__main__"` block at the bottom for
example invocations, or import and call directly from another script.
"""
import subprocess
import time
import os
import sys

SOCKET = "/tmp/ah-harness.sock"
SESSION = "ahtest"
BINARY = "/Users/hetpatel/handoff/target/release/ah"


def _tmux(*args, check=False):
    return subprocess.run(["tmux", "-S", SOCKET, *args], capture_output=True, text=True, check=check)


def kill():
    _tmux("kill-server")


def start(agent_args, cwd, cols=100, rows=30):
    """agent_args: list, e.g. ["claude"] or ["codex"]."""
    kill()
    cmd = f"cd {cwd} && {BINARY} {' '.join(agent_args)}"
    _tmux("new-session", "-d", "-s", SESSION, "-x", str(cols), "-y", str(rows), cmd)
    time.sleep(0.3)


def send_keys(keys):
    """Send a tmux key-name string, e.g. 'Enter', 'C-r' (tmux's own key
    encoding, NOT necessarily what we want to test for our own trigger
    detection -- use send_raw_hex for that)."""
    _tmux("send-keys", "-t", SESSION, keys)


def send_literal(text):
    _tmux("send-keys", "-t", SESSION, "-l", text)


def send_raw_hex(hex_bytes: bytes):
    """Send exact raw bytes (e.g. a specific Kitty CSI-u encoding) via
    tmux's hex-literal send-keys mode, bypassing tmux's own key-name
    translation entirely."""
    hex_str = hex_bytes.hex()
    pairs = [hex_str[i:i + 2] for i in range(0, len(hex_str), 2)]
    _tmux("send-keys", "-t", SESSION, "-H", *pairs)


def resize(cols, rows):
    _tmux("resize-window", "-t", SESSION, "-x", str(cols), "-y", str(rows))


def pane_size():
    """The size the program actually sees -- NOT tmux's own window size.
    tmux reserves one row for its own status line by default, so
    window_height and pane_height differ; comparing against window_height
    gives a false "off by one" failure that isn't a real bug, just the
    wrong reference value. Confirmed directly: for a 30-row window with
    the status line on, pane_height is 29."""
    result = _tmux("display-message", "-t", SESSION, "-p", "#{pane_width} #{pane_height}")
    w, ht = result.stdout.strip().split()
    return int(w), int(ht)


def capture(escape_codes=False):
    args = ["capture-pane", "-t", SESSION, "-p"]
    if escape_codes:
        args.append("-e")
    result = _tmux(*args)
    return result.stdout


def toggle_bar_lines(text=None):
    text = text if text is not None else capture()
    return [(i, l.rstrip()) for i, l in enumerate(text.splitlines()) if "Alt+" in l and "switch agent" in l]


def assert_single_toggle_bar(label, expected_row=None):
    lines = toggle_bar_lines()
    ok = len(lines) == 1
    if expected_row is not None and ok:
        ok = lines[0][0] == expected_row
    status = "OK" if ok else "FAIL"
    print(f"[{status}] {label}: {len(lines)} toggle line(s) {lines}")
    return ok


def wait_for(predicate, timeout=15.0, interval=0.5):
    deadline = time.time() + timeout
    while time.time() < deadline:
        if predicate():
            return True
        time.sleep(interval)
    return False


def contains(text_fragment):
    return text_fragment in capture()


if __name__ == "__main__":
    # Smoke test: launch claude, accept trust, confirm exactly one toggle line.
    cwd = "/tmp/ah-harness-smoketest"
    os.makedirs(cwd, exist_ok=True)
    start(["claude"], cwd)
    wait_for(lambda: "trust" in capture().lower(), timeout=10)
    send_keys("Enter")
    wait_for(lambda: len(toggle_bar_lines()) >= 1, timeout=10)
    time.sleep(1.0)
    ok = assert_single_toggle_bar("smoke test: claude startup")
    kill()
    sys.exit(0 if ok else 1)
