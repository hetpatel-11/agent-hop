#!/usr/bin/env python3
"""Sustained, realistic stress test using tui_harness -- tries to
reproduce the reported "toggle bar corrupts/stacks after a while" bug by
doing many of the actions a real session would do, checking integrity
after every single step rather than just at the end."""
import sys
import os
import time

sys.path.insert(0, os.path.dirname(__file__))
import tui_harness as h

cwd = "/tmp/ah-stress-test"
os.makedirs(cwd, exist_ok=True)

failures = []


def check(label, expected_row=None):
    if not h.assert_single_toggle_bar(label, expected_row):
        failures.append(label)


h.start(["codex"], cwd, cols=100, rows=30)
h.wait_for(lambda: h.contains("Ask Codex"), timeout=15)
time.sleep(1.0)
check("startup")

messages = [
    "say the word apple and nothing else",
    "say the word banana and nothing else",
    "say the word cherry and nothing else",
]
for i, msg in enumerate(messages):
    h.send_literal(msg)
    h.send_keys("Enter")
    h.wait_for(lambda m=msg: m.split()[2] in h.capture(), timeout=20)
    time.sleep(1.5)
    check(f"after message {i + 1} ('{msg.split()[2]}')")

# Resize mid-idle
h.resize(120, 35)
time.sleep(1.0)
_, pane_h = h.pane_size()
check("after resize to 120x35", expected_row=pane_h - 1)

h.send_literal("say the word date and nothing else")
h.send_keys("Enter")
h.wait_for(lambda: "date" in h.capture(), timeout=20)
time.sleep(1.5)
check("after message post-resize")

# Hop to claude and back, twice
for i in range(2):
    h.send_raw_hex(b"\x1b[57420;3u")  # Alt+Down (Kitty CSI-u) -> next agent
    h.wait_for(lambda: h.contains("Alt+"), timeout=15)
    time.sleep(2.0)
    check(f"after hop {i + 1} forward")

    h.send_raw_hex(b"\x1b[1;3A")  # Alt+Up (legacy CSI) -> prev agent
    h.wait_for(lambda: h.contains("Alt+"), timeout=15)
    time.sleep(2.0)
    check(f"after hop {i + 1} back")

# Resize again while an agent is idle at the prompt, then type
h.resize(90, 28)
time.sleep(1.0)
_, pane_h = h.pane_size()
check("after second resize to 90x28", expected_row=pane_h - 1)
h.send_literal("say the word elderberry and nothing else")
h.send_keys("Enter")
h.wait_for(lambda: "elderberry" in h.capture(), timeout=20)
time.sleep(1.5)
check("after final message")

print()
print(f"Total checks failed: {len(failures)}")
for f in failures:
    print(f"  FAILED: {f}")

if failures:
    print()
    print("--- final screen state ---")
    print(h.capture())

h.kill()
sys.exit(1 if failures else 0)
