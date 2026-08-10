import assert from "node:assert/strict";
import test from "node:test";
import { resolveExecutable } from "../src/executable.js";

test("resolves an installed executable", () => {
  assert.ok(resolveExecutable("node"));
});

test("returns null for a missing executable", () => {
  assert.equal(resolveExecutable("agent-hop-definitely-not-installed"), null);
});
