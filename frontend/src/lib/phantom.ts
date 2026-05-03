// Phase 6 Day 2 — Phantom wallet wrapper.
//
// Translates the connect / sign patterns from `bridge/index.html` and
// `docs/proofs/phantom_a1_sign_v0.html` into TypeScript with strict
// type guards. The browser-side surface is intentionally minimal:
//
//  - getPhantomProvider()           — runtime detection (Phantom v2 first, v1 fallback)
//  - isPhantomAvailable()           — boolean for UI gating
//  - connectPhantom() / disconnect  — public-key handshake
//  - signSolendTransaction(b64)     — legacy `Transaction` path used by the Solend rail
//  - signMessage(text)              — for challenge-response wallet bind (future use)
//
// Safety contract:
//   - No private-key handling. Phantom owns key material.
//   - No raw secret logging. The only thing we ever pass to the daemon
//     is `signed_tx_b64` (a serialised, signed Solana transaction —
//     public on-chain bytes once submitted).
//   - No auto-signing. Every signTransaction call is gated on a
//     deliberate user action upstream (a button click in
//     `wallet-connect.tsx` / `signing-flow.tsx`).
//   - No `signAndSendTransaction`. Per Phase 4C / 5G's pattern, the
//     daemon owns submit + verification; the browser owns signing only.

import { Transaction } from "@solana/web3.js";

// ── Provider detection ──────────────────────────────────────────────────────

/// Minimal shape of the Phantom provider we rely on. We do not import
/// the Phantom SDK directly — production Phantom is loaded as a
/// browser extension and exposes itself on `window.phantom.solana`
/// (v2) or `window.solana` (legacy) without an import.
export interface PhantomProvider {
  isPhantom?: boolean;
  publicKey?: { toBase58(): string };
  /** Connect — Phantom shows a popup; resolves with the user's pubkey. */
  connect(opts?: { onlyIfTrusted?: boolean }): Promise<{
    publicKey: { toBase58(): string };
  }>;
  /** Disconnect — Phantom v2 supports; v1 is a no-op fallback. */
  disconnect?(): Promise<void>;
  /** Sign a Solana transaction. Phantom signs only the slot(s) whose
   *  pubkey it owns; other slots remain untouched. */
  signTransaction(tx: Transaction): Promise<Transaction>;
  /** Sign an arbitrary byte buffer — used only for the wallet-bind
   *  challenge nonce in the future. Returns ed25519 signature bytes. */
  signMessage(
    message: Uint8Array,
    encoding?: "utf8",
  ): Promise<{ signature: Uint8Array }>;
  /** Listen for account-change / disconnect events. */
  on?(event: "disconnect" | "accountChanged", handler: () => void): void;
  removeListener?(
    event: "disconnect" | "accountChanged",
    handler: () => void,
  ): void;
}

/// Loose typing of the global Phantom injection surface. Phantom v2
/// nests under `window.phantom.solana`; v1 puts it directly at
/// `window.solana`. Either form is acceptable.
interface WindowWithPhantom {
  phantom?: { solana?: PhantomProvider };
  solana?: PhantomProvider;
}

/// Runtime accessor — returns the Phantom provider if Phantom is
/// installed and recognised, `null` otherwise. **SSR-safe**: returns
/// `null` during server render (`window` is undefined), so callers can
/// invoke this in component bodies without crashing the build.
export function getPhantomProvider(): PhantomProvider | null {
  if (typeof window === "undefined") return null;
  const w = window as unknown as WindowWithPhantom;
  const provider = w.phantom?.solana ?? w.solana ?? null;
  if (!provider) return null;
  if (provider.isPhantom !== true) return null;
  return provider;
}

/// Boolean guard for UI rendering — `true` only when a recognised
/// Phantom provider is present at runtime.
export function isPhantomAvailable(): boolean {
  return getPhantomProvider() !== null;
}

// ── Connection lifecycle ────────────────────────────────────────────────────

/// User-initiated connection flow. **Must be called from a click
/// handler** — Phantom rejects the connect popup if the call is not
/// rooted in a user gesture.
///
/// Returns the connected wallet's public key as a base58 string.
/// Throws on missing provider, user rejection, or any underlying
/// Phantom error.
export async function connectPhantom(): Promise<{ pubkey: string }> {
  const provider = getPhantomProvider();
  if (!provider) {
    throw new Error(
      "Phantom not detected. Install the Phantom browser extension and reload.",
    );
  }
  const resp = await provider.connect();
  return { pubkey: resp.publicKey.toBase58() };
}

/// Best-effort disconnect — supported by Phantom v2; a no-op for v1
/// (older versions do not expose `disconnect`). Safe to call without
/// guards; the wrapper detects support.
export async function disconnectPhantom(): Promise<void> {
  const provider = getPhantomProvider();
  if (provider?.disconnect) {
    await provider.disconnect();
  }
}

// ── Signing ─────────────────────────────────────────────────────────────────

/// Sign a Solend partial-signed legacy transaction.
///
/// **Wire shape (from Phase 4C / 5G):** the daemon hands back a base64
/// blob via `GET /sessions/:id/solend-signatures/:request_id`. That
/// blob is a Solana legacy `Transaction` already signed in the
/// obligation slot by an ephemeral keypair held in the daemon's
/// `SolendSigningStore`. The session-wallet slot is empty.
///
/// **What we do:**
///   1. Decode base64 → bytes
///   2. Deserialise into `Transaction` (legacy)
///   3. Hand to Phantom — Phantom only signs the slot whose pubkey
///      matches the connected wallet (i.e., the session-wallet slot)
///   4. Re-serialise and base64-encode for return to the daemon
///
/// Phantom never touches the obligation slot because the obligation
/// keypair is not in the user's wallet. The 4C-5 verification pipeline
/// asserts the obligation signature is bit-equal before vs after
/// signing — proves the user did not tamper.
export async function signSolendTransaction(unsignedTxB64: string): Promise<string> {
  const provider = getPhantomProvider();
  if (!provider) {
    throw new Error("Phantom not connected — connect the wallet before signing.");
  }
  const txBytes = base64ToUint8Array(unsignedTxB64);
  const tx = Transaction.from(txBytes);
  const signed = await provider.signTransaction(tx);
  // Solana wire format: `signed.serialize()` produces the canonical
  // bytes the RPC accepts. The legacy `Transaction` class refuses to
  // serialise unless every required signature is present — Phantom
  // having signed the session-wallet slot satisfies that for the
  // Solend partial-sign pattern.
  const signedBytes = signed.serialize();
  return uint8ArrayToBase64(signedBytes);
}

/// Sign a UTF-8 string with the connected wallet's ed25519 key.
/// Reserved for the wallet-bind challenge-response flow in a later
/// phase; included now so the wrapper surface is complete.
export async function signMessage(text: string): Promise<{ signatureB64: string }> {
  const provider = getPhantomProvider();
  if (!provider) {
    throw new Error("Phantom not connected — connect the wallet before signing.");
  }
  const messageBytes = new TextEncoder().encode(text);
  const result = await provider.signMessage(messageBytes, "utf8");
  return { signatureB64: uint8ArrayToBase64(result.signature) };
}

// ── Base64 ↔ bytes helpers ──────────────────────────────────────────────────
//
// The daemon's wire shape is base64 strings. Phantom's wire shape is
// `Uint8Array`. We keep the conversion local to this file so callers
// never see raw bytes outside this module.

function base64ToUint8Array(b64: string): Uint8Array {
  // Browser-native atob; works for the standard-base64 strings the
  // daemon produces (no URL-safe variant in this code path).
  const bin = atob(b64);
  const bytes = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) {
    bytes[i] = bin.charCodeAt(i);
  }
  return bytes;
}

function uint8ArrayToBase64(bytes: Uint8Array): string {
  let bin = "";
  for (let i = 0; i < bytes.length; i++) {
    bin += String.fromCharCode(bytes[i]);
  }
  return btoa(bin);
}
