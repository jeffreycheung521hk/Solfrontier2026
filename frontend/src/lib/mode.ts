// Showcase vs live mode selection.
//
// Set NEXT_PUBLIC_MODE=live and NEXT_PUBLIC_GATEWAY_URL to talk to a running
// gateway. Default is "showcase" so the UI renders fixtures and nothing is
// wired to a real daemon — safe for sponsor demos on flaky networks.

export type Mode = "showcase" | "live";

export const MODE: Mode =
  (process.env.NEXT_PUBLIC_MODE as Mode | undefined) ?? "showcase";

export const GATEWAY_URL: string =
  process.env.NEXT_PUBLIC_GATEWAY_URL ?? "http://127.0.0.1:8787";

/// Bearer token used by live mode. Must match the daemon's `--api-token`
/// (or the `CLAWSOLANA_API_TOKEN` env var the daemon reads). Empty in
/// showcase mode.
export const GATEWAY_TOKEN: string =
  process.env.NEXT_PUBLIC_GATEWAY_TOKEN ?? "";

export const IS_SHOWCASE = MODE === "showcase";
export const IS_LIVE = MODE === "live";
