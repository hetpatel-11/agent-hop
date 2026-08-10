import assert from "node:assert/strict";
import test from "node:test";
import { buildCodexAssistantImagePayloads, buildCodexMessageContent, sanitizeCodexFunctionName } from "../src/adapters/codex.js";

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

const image = { mimeType: "image/png", base64: "c2FuaXRpemVkLXRlc3QtZGF0YQ==" };

test("only writes input_image blocks into user messages", () => {
  assert.deepEqual(buildCodexMessageContent("user", "look", [image]), [
    { type: "input_text", text: "look" },
    { type: "input_image", image_url: "data:image/png;base64,c2FuaXRpemVkLXRlc3QtZGF0YQ==" },
  ]);
  assert.deepEqual(buildCodexMessageContent("assistant", "result", [image]), [{ type: "output_text", text: "result" }]);
});

test("preserves assistant images as Codex function-call outputs", () => {
  assert.deepEqual(buildCodexAssistantImagePayloads([image], "call_test"), [
    { type: "function_call", name: "imported_image", arguments: "{}", call_id: "call_test" },
    {
      type: "function_call_output",
      call_id: "call_test",
      output: [{ type: "input_image", image_url: "data:image/png;base64,c2FuaXRpemVkLXRlc3QtZGF0YQ==" }],
    },
  ]);
});
