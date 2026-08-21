#!/usr/bin/env node
// Thin shim: resolves the platform-specific native binary (installed as an
// optionalDependency package) and execs it. Same pattern esbuild/swc/
// Turborepo/Biome use to ship a native binary through npm -- keeps
// `npm install -g agent-hop` / `npx agent-hop` working unchanged for
// existing users while the actual engine is a compiled Rust binary, not JS.
const { spawnSync } = require("node:child_process");
const { existsSync } = require("node:fs");

const PLATFORM_PACKAGES = {
  "darwin-arm64": "agent-hop-darwin-arm64",
  "darwin-x64": "agent-hop-darwin-x64",
  "linux-x64": "agent-hop-linux-x64",
  "linux-arm64": "agent-hop-linux-arm64",
  "win32-x64": "agent-hop-win32-x64",
};

const key = `${process.platform}-${process.arch}`;
const pkgName = PLATFORM_PACKAGES[key];

if (!pkgName) {
  console.error(
    `agent-hop: no prebuilt binary for platform "${key}". ` +
      `Supported: ${Object.keys(PLATFORM_PACKAGES).join(", ")}. ` +
      `Open an issue at https://github.com/hetpatel-11/agent-hop/issues if you need this platform.`
  );
  process.exit(1);
}

const binName = process.platform === "win32" ? "ah.exe" : "ah";
let binPath;
try {
  binPath = require.resolve(`${pkgName}/bin/${binName}`);
} catch {
  console.error(
    `agent-hop: expected optionalDependency "${pkgName}" was not installed. ` +
      `This usually means npm skipped it for your platform/arch, or install was interrupted -- ` +
      `try reinstalling with "npm install -g agent-hop".`
  );
  process.exit(1);
}

if (!existsSync(binPath)) {
  console.error(`agent-hop: resolved binary path does not exist: ${binPath}`);
  process.exit(1);
}

const result = spawnSync(binPath, process.argv.slice(2), { stdio: "inherit" });
if (result.error) {
  console.error(`agent-hop: failed to launch native binary: ${result.error.message}`);
  process.exit(1);
}
process.exit(result.status ?? 1);
