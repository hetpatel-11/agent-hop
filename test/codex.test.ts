import assert from "node:assert/strict";
import test from "node:test";
import { sanitizeCodexFunctionName } from "../src/adapters/codex.js";

test("preserves valid Codex function names", () => {
  assert.equal(sanitizeCodexFunctionName("run_terminal_command"), "run_terminal_command");
  assert.equal(sanitizeCodexFunctionName("tool-name_2"), "tool-name_2");
  assert.equal(sanitizeCodexFunctionName("__tool__"), "__tool__");
});

test("normalizes cross-agent tool labels for the Codex API", () => {
  assert.equal(sanitizeCodexFunctionName("Web search:"), "Web_search");
  assert.equal(sanitizeCodexFunctionName(" mcp.server/tool name "), "mcp_server_tool_name");
  assert.equal(sanitizeCodexFunctionName(":"), "unknown_tool");
});

test("always returns a valid Codex function name", () => {
  for (const name of ["Web search:", " mcp.server/tool name ", ":", "", "\u641c\u7d22\u5de5\u5177"]) {
    assert.match(sanitizeCodexFunctionName(name), /^[a-zA-Z0-9_-]+$/);
  }
});
