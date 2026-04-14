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

export const IS_SHOWCASE = MODE === "showcase";
export const IS_LIVE = MODE === "live";
