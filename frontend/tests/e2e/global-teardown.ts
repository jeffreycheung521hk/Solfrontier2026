export default async function globalTeardown() {
  const encoded = process.env.__E2E_PIDS;
  if (!encoded) return;
  const entries = encoded.split(",").filter(Boolean);

  if (process.platform === "win32") {
    const { spawnSync } = await import("node:child_process");
    for (const entry of entries) {
      const pid = entry.split(":")[1];
      if (!pid) continue;
      spawnSync("taskkill", ["/F", "/T", "/PID", pid], { stdio: "ignore" });
    }
    // Belt-and-braces: also kill any stray clawd / next dev processes that
    // slipped out of the tree (common on Windows when ctrl-c interrupts).
    spawnSync("taskkill", ["/F", "/IM", "clawd.exe"], { stdio: "ignore" });
  } else {
    for (const entry of entries) {
      const pid = Number(entry.split(":")[1]);
      if (!pid) continue;
      try { process.kill(pid, "SIGTERM"); } catch { /* gone */ }
    }
  }
}
