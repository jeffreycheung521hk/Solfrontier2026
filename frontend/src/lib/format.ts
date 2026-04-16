// Formatting helpers for the showcase UI.

const LAMPORTS_PER_SOL = 1_000_000_000;

export function formatSol(lamports: number): string {
  const sol = lamports / LAMPORTS_PER_SOL;
  if (sol >= 1) return `${sol.toLocaleString(undefined, { maximumFractionDigits: 4 })} SOL`;
  return `${lamports.toLocaleString()} lamports`;
}

export function formatToken(amount: number, decimals: number, symbol = ""): string {
  const scaled = amount / 10 ** decimals;
  const body = scaled.toLocaleString(undefined, { maximumFractionDigits: decimals });
  return symbol ? `${body} ${symbol}` : body;
}

export function shortPubkey(pk: string, keep = 4): string {
  if (pk.length <= keep * 2 + 1) return pk;
  return `${pk.slice(0, keep)}…${pk.slice(-keep)}`;
}

export function formatRelative(iso: string, now = new Date()): string {
  const then = new Date(iso).getTime();
  const diffSec = Math.round((now.getTime() - then) / 1000);
  if (Math.abs(diffSec) < 60) return `${diffSec}s ago`;
  const diffMin = Math.round(diffSec / 60);
  if (Math.abs(diffMin) < 60) return `${diffMin}m ago`;
  const diffHr = Math.round(diffMin / 60);
  return `${diffHr}h ago`;
}

export function formatCountdown(seconds: number): string {
  if (seconds <= 0) return "expired";
  const m = Math.floor(seconds / 60);
  const s = seconds % 60;
  return `${m}:${s.toString().padStart(2, "0")}`;
}

export function mintSymbol(mint?: string | null): string {
  if (!mint) return "SPL";
  if (mint === "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v") return "USDC";
  return "SPL";
}
