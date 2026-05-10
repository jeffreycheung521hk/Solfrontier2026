// Stage 2 — `GET /api/stage2/dashboard` (Next.js App Router).
//
// Read-only dashboard data route. Feeds the existing A3 dashboard's
// "Read-only API (Preview)" source so it stops 404'ing.
//
// HARD GUARANTEES upheld in this file:
//   - GET only. Non-GET methods return 405.
//   - No mutation. No signing. No transaction construction.
//   - No broadcast. No transaction submit.
//   - No live Solana RPC. No outbound HTTP / fetch / axios.
//   - No on-chain program code touched. No wallet code imported.
//   - Always returns a valid JSON payload accepted by the existing
//     A3 validator (`validateApiResponse` in
//     `frontend/src/lib/stage2-watch-api.ts`); never returns 500.
//
// DATA SOURCE for this slice:
//   Deterministic demo fallback. There is no in-process state-store
//   accessible from this route in the current codebase, and adding
//   an outbound fetch to a hypothetical Rust daemon URL was rejected
//   for security and simplicity. The payload is built by
//   `buildFallbackDashboardPayload(nowMs)` from
//   `@/lib/stage2-dashboard-server`, which is a pure deterministic
//   function with NO RPC, NO daemon access, NO database read.
//   Future slices can swap this for a real read-only state-store
//   adapter — the route's surface stays unchanged.
//
// CACHING:
//   `export const dynamic = "force-dynamic"` opts the route out of
//   any static generation. Response carries `cache-control: no-store,
//   no-cache, must-revalidate` + `pragma: no-cache` so any
//   intermediary or browser cache treats it as fresh-only.

import { NextResponse } from "next/server";

import { buildFallbackDashboardPayload } from "@/lib/stage2-dashboard-server";

// Force dynamic rendering. Required by the A4 spec — the route MUST
// never be statically generated at build time.
export const dynamic = "force-dynamic";

// Optional but defensive — Next sometimes treats route handlers as
// edge-runtime by default in some configs. We pin Node so the route
// runs in the same environment as the rest of the server code and
// has access to standard Web Crypto / Node APIs.
export const runtime = "nodejs";

const NO_STORE_HEADERS = {
  "content-type": "application/json",
  "cache-control": "no-store, no-cache, must-revalidate",
  "pragma": "no-cache",
} as const;

/// `GET /api/stage2/dashboard` — returns the dashboard JSON payload.
/// Always 200 in this slice (deterministic demo fallback never
/// fails). Future real-backend integrations should preserve the
/// "never 500; always renderable JSON" invariant.
export async function GET(): Promise<NextResponse> {
  // Build deterministic demo payload. Anchored to wall-clock so the
  // dashboard's "x seconds ago" labels look fresh on each refresh
  // click; the data SHAPE is invariant.
  const nowMs = Date.now();
  const payload = buildFallbackDashboardPayload(nowMs);
  return new NextResponse(JSON.stringify(payload), {
    status: 200,
    headers: NO_STORE_HEADERS,
  });
}

// ── Method guards ──────────────────────────────────────────────────
//
// Next.js App Router auto-returns 405 for HTTP methods not exported
// here, but we declare the four mutating verbs explicitly so the
// "this route is read-only" intent is grep-visible and the response
// body / Allow header are stable across Next versions.

function methodNotAllowed(method: string): NextResponse {
  return new NextResponse(
    JSON.stringify({
      error: `method ${method} not allowed; this route is read-only GET`,
    }),
    {
      status: 405,
      headers: {
        ...NO_STORE_HEADERS,
        allow: "GET",
      },
    },
  );
}

export async function POST(): Promise<NextResponse> {
  return methodNotAllowed("POST");
}
export async function PUT(): Promise<NextResponse> {
  return methodNotAllowed("PUT");
}
export async function DELETE(): Promise<NextResponse> {
  return methodNotAllowed("DELETE");
}
export async function PATCH(): Promise<NextResponse> {
  return methodNotAllowed("PATCH");
}
