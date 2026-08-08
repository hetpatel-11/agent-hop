import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const CACHE_DIR = join(homedir(), ".agent-hop");
const CACHE_PATH = join(CACHE_DIR, "update-check.json");
const CHECK_INTERVAL_MS = 24 * 60 * 60 * 1000; // once a day -- no reason to hit the registry on every launch
const FETCH_TIMEOUT_MS = 1500; // never let a slow/offline network delay startup meaningfully

interface Cache {
  latest: string;
  checkedAt: number;
}

function currentVersion(): string {
  // dist/update-check.js -> ../package.json
  const here = dirname(fileURLToPath(import.meta.url));
  const pkg = JSON.parse(readFileSync(join(here, "..", "package.json"), "utf8"));
  return pkg.version as string;
}

function readCache(): Cache | null {
  try {
    return JSON.parse(readFileSync(CACHE_PATH, "utf8"));
  } catch {
    return null;
  }
}

function writeCache(cache: Cache): void {
  try {
    mkdirSync(CACHE_DIR, { recursive: true });
    writeFileSync(CACHE_PATH, JSON.stringify(cache));
  } catch {
    // best-effort cache -- a failed write just means we re-check next launch, not fatal
  }
}

async function fetchLatestVersion(): Promise<string | null> {
  try {
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), FETCH_TIMEOUT_MS);
    const res = await fetch("https://registry.npmjs.org/agent-hop/latest", { signal: controller.signal });
    clearTimeout(timer);
    if (!res.ok) return null;
    const data = (await res.json()) as { version?: string };
    return data.version ?? null;
  } catch {
    return null; // offline, timeout, registry hiccup -- never let this break a launch
  }
}

/** Simple semver-ish compare, good enough for "is latest newer than current". */
function isNewer(latest: string, current: string): boolean {
  const a = latest.split(".").map(Number);
  const b = current.split(".").map(Number);
  for (let i = 0; i < Math.max(a.length, b.length); i++) {
    const x = a[i] ?? 0;
    const y = b[i] ?? 0;
    if (x !== y) return x > y;
  }
  return false;
}

export interface UpdateInfo {
  current: string;
  latest: string;
  updateAvailable: boolean;
}

/** Cached, network-optional version check. Never throws, never blocks longer
 * than FETCH_TIMEOUT_MS, and only hits the registry once per CHECK_INTERVAL_MS. */
export async function checkForUpdate(): Promise<UpdateInfo> {
  const current = currentVersion();
  const cache = readCache();
  const cacheFresh = cache && Date.now() - cache.checkedAt < CHECK_INTERVAL_MS;

  let latest = cache?.latest ?? current;
  if (!cacheFresh) {
    const fetched = await fetchLatestVersion();
    if (fetched) {
      latest = fetched;
      writeCache({ latest: fetched, checkedAt: Date.now() });
    } else if (cache) {
      latest = cache.latest; // keep stale cache rather than nothing
    }
  }

  return { current, latest, updateAvailable: isNewer(latest, current) };
}
