import type { NextConfig } from "next";

// Allow each dev server to use its own build dir when running side-by-side
// (Playwright E2E spins up live and showcase instances in parallel).
const distDir = process.env.CLAW_E2E_DIST_DIR;

const nextConfig: NextConfig = {
  ...(distDir ? { distDir } : {}),
};

export default nextConfig;
