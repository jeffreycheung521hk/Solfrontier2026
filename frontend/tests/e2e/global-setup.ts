import { spawn, ChildProcess } from "node:child_process";
import { existsSync, rmSync, mkdirSync } from "node:fs";
import { join } from "node:path";

// Repo root = one level up from frontend/.
const frontendDir = join(__dirname, "..", "..");
const repoRoot = join(frontendDir, "..");
const daemonBin = join(repoRoot, "target", "debug", "clawd.exe");
const configFile = join(repoRoot, "config", "default.toml");
const dbDir = join(repoRoot, "data");

type Spawned = { name: string; pid: number; child: ChildProcess };
const spawned: Spawned[] = [];

function recordSpawned(s: Spawned) {
  spawned.push(s);
  // Persist PID list *immediately* so teardown can clean up even if setup
  // crashes partway through (e.g. one dev server failed to come up).
  process.env.__E2E_PIDS = spawned.map((s) => `${s.name}:${s.pid}`).join(",");
}

async function waitForUrl(url: string, timeoutMs: number, label: string): Promise<void> {
  const start = Date.now();
  let lastErr: unknown = null;
  while (Date.now() - start < timeoutMs) {
    try {
      // Abort after 3 s so a single stuck compile doesn't monopolise the loop.
      const ctrl = new AbortController();
      const t = setTimeout(() => ctrl.abort(), 3_000);
      try {
        const r = await fetch(url, { signal: ctrl.signal });
        clearTimeout(t);
        if (r.status < 500) return; // Next can 200 / 404 once booted
      } finally {
        clearTimeout(t);
      }
    } catch (e) {
      lastErr = e;
    }
    await new Promise((r) => setTimeout(r, 500));
  }
  throw new Error(`[${label}] did not become reachable at ${url} within ${timeoutMs}ms — last error: ${String(lastErr)}`);
}

async function seedOnce(token: string): Promise<void> {
  const r = await fetch("http://127.0.0.1:7070/debug/seed-demo", {
    method: "POST",
    headers: { authorization: `Bearer ${token}` },
  });
  if (!r.ok) {
    const body = await r.text();
    throw new Error(`seed-demo failed: ${r.status} ${body}`);
  }
}

function spawnDaemon(): Spawned {
  if (!existsSync(daemonBin)) {
    throw new Error(
      `daemon binary not found at ${daemonBin}. Run \`cargo build --bin clawd\` first.`,
    );
  }
  if (!existsSync(dbDir)) mkdirSync(dbDir, { recursive: true });
  for (const name of ["claw.db", "claw.db-shm", "claw.db-wal"]) {
    const p = join(dbDir, name);
    if (existsSync(p)) rmSync(p, { force: true });
  }
  const child = spawn(daemonBin, ["--config", configFile], {
    cwd: repoRoot, // config has keypair_path = "data/devnet.json" relative to repo root
    env: {
      ...process.env,
      CLAW_API_TOKEN: "showcase-demo-token",
      CLAW_ENABLE_DEMO_SEED: "1",
      RUST_LOG: "warn",
    },
    stdio: ["ignore", "pipe", "pipe"],
    windowsHide: true,
  });
  return { name: "clawd", pid: child.pid!, child };
}

async function runNextBuild(port: number, env: Record<string, string>): Promise<void> {
  const distDir = `.next-e2e-${port}`;
  // Skip the (slow) build if the dist dir is already populated and newer
  // than any src file. Saves ~2 minutes per iteration during debug loops.
  // Force rebuild with CLAW_E2E_FORCE_BUILD=1.
  if (process.env.CLAW_E2E_FORCE_BUILD !== "1"
      && existsSync(join(frontendDir, distDir, "BUILD_ID"))) {
    console.log(`[e2e] reusing cached build at ${distDir} (set CLAW_E2E_FORCE_BUILD=1 to force rebuild)`);
    return;
  }
  const { spawnSync } = await import("node:child_process");
  const r = spawnSync(
    process.platform === "win32" ? "npx.cmd" : "npx",
    ["next", "build"],
    {
      cwd: frontendDir,
      env: { ...process.env, ...env, CLAW_E2E_DIST_DIR: distDir },
      stdio: "inherit",
      shell: process.platform === "win32",
    },
  );
  if (r.status !== 0) {
    throw new Error(`next build failed for port ${port} (exit ${r.status})`);
  }
}

function spawnNextStart(port: number, env: Record<string, string>): Spawned {
  // Production `next start` uses far less memory than dev / Turbopack and
  // lets us run two Next instances side-by-side reliably. Build must have
  // already happened via runNextBuild().
  const distDir = `.next-e2e-${port}`;
  env = { ...env, CLAW_E2E_DIST_DIR: distDir };
  const cmd = process.platform === "win32"
    ? `npx.cmd next start --port ${port}`
    : `npx next start --port ${port}`;
  const child = spawn(cmd, {
    cwd: frontendDir,
    env: { ...process.env, ...env },
    stdio: ["ignore", "pipe", "pipe"],
    shell: true,
    windowsHide: true,
  });
  const tag = `[next@${port}]`;
  child.stdout?.on("data", (d) => process.stdout.write(`${tag} ${d}`));
  child.stderr?.on("data", (d) => process.stderr.write(`${tag} ${d}`));
  return { name: `next@${port}`, pid: child.pid!, child };
}

async function freePortIfBound(port: number): Promise<void> {
  if (process.platform !== "win32") return;
  const { spawnSync } = await import("node:child_process");
  const r = spawnSync("netstat", ["-ano"], { encoding: "utf8" });
  if (r.status !== 0 || !r.stdout) return;
  for (const line of r.stdout.split(/\r?\n/)) {
    const m = line.match(/LISTENING\s+(\d+)$/);
    if (m && line.includes(`0.0.0.0:${port} `)) {
      const pid = m[1];
      console.log(`[e2e] killing leftover PID ${pid} on :${port}`);
      spawnSync("taskkill", ["/F", "/T", "/PID", pid], { stdio: "ignore" });
    }
  }
}

export default async function globalSetup() {
  // 0. Defensive cleanup — leftover Next / clawd from an interrupted run
  //    will keep ports bound.
  for (const port of [7070, 3000, 3001]) await freePortIfBound(port);
  await new Promise((r) => setTimeout(r, 500));

  // 1. Daemon first.
  const daemon = spawnDaemon();
  recordSpawned(daemon);
  await waitForUrl("http://127.0.0.1:7070/health", 60_000, "daemon");

  // 2. Seed into the clean daemon.
  await seedOnce("showcase-demo-token");

  // 3. Build + start two Next servers in production mode. Production start
  //    uses ~10× less memory than Turbopack dev, which matters when running
  //    two instances side-by-side.
  const liveEnv = {
    NEXT_PUBLIC_MODE: "live",
    NEXT_PUBLIC_GATEWAY_URL: "http://127.0.0.1:7070",
    NEXT_PUBLIC_GATEWAY_TOKEN: "showcase-demo-token",
  };
  const showcaseEnv = { NEXT_PUBLIC_MODE: "showcase" };

  console.log("[e2e] building Next.js (live)…");
  await runNextBuild(3000, liveEnv);
  console.log("[e2e] building Next.js (showcase)…");
  await runNextBuild(3001, showcaseEnv);

  const live = spawnNextStart(3000, liveEnv);
  recordSpawned(live);
  const showcase = spawnNextStart(3001, showcaseEnv);
  recordSpawned(showcase);

  // Probe favicon — cheap static asset, doesn't trigger full Turbopack compile.
  await waitForUrl("http://127.0.0.1:3000/favicon.ico", 120_000, "next-live");
  await waitForUrl("http://127.0.0.1:3001/favicon.ico", 120_000, "next-showcase");

  // Warm up each page route on both servers so the first real test doesn't
  // race Turbopack's lazy compilation.
  async function warm(base: string, paths: string[]) {
    for (const p of paths) {
      try { await fetch(`${base}${p}`); } catch { /* best-effort */ }
    }
  }
  const commonPaths = ["/", "/audit", "/policy"];
  await warm("http://127.0.0.1:3000", commonPaths);
  await warm("http://127.0.0.1:3001", commonPaths);

  // PIDs were staged into __E2E_PIDS by recordSpawned() as we went.
}
