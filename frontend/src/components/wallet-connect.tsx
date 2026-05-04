"use client";

// Phase 6 Day 2 — Wallet connect UI.
//
// A small, focused control: connect / disconnect Phantom and display
// the connected pubkey (shortened). No auto-connect on mount —
// Phantom requires a user gesture to invoke the connect popup, so we
// always wait for an explicit click.
//
// Shape rules:
//   - "use client" — uses useState / useEffect.
//   - Non-blocking: if Phantom is not installed, render an "Install
//     Phantom" link instead of throwing or crashing.
//   - Errors render inline (no toasts in this slice; keep it simple).
//   - No raw key handling. The connected pubkey is the only credential
//     touch-point, and it is public on-chain data.

import { useCallback, useEffect, useState } from "react";

import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import {
  connectPhantom,
  disconnectPhantom,
  getPhantomProvider,
  isPhantomAvailable,
  type PhantomProvider,
} from "@/lib/phantom";
import { shortPubkey } from "@/lib/format";

interface WalletConnectProps {
  /** Notify parent when the connected pubkey changes (or becomes null). */
  onChange?: (pubkey: string | null) => void;
}

type DetectionState = "checking" | "missing" | "available";

export function WalletConnect({ onChange }: WalletConnectProps) {
  const [detection, setDetection] = useState<DetectionState>("checking");
  const [pubkey, setPubkey] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // Phantom detection — runs once after mount. SSR-safe because
  // isPhantomAvailable() returns false when window is undefined.
  useEffect(() => {
    setDetection(isPhantomAvailable() ? "available" : "missing");
  }, []);

  // Notify parent on pubkey change. Does NOT auto-connect — pubkey
  // only changes via explicit connect/disconnect actions below, or
  // via Phantom's own disconnect / accountChanged events (subscribed
  // separately below).
  useEffect(() => {
    onChange?.(pubkey);
  }, [pubkey, onChange]);

  // Subscribe to Phantom's disconnect / accountChanged events so the
  // UI tracks reality if the user manages the wallet from inside
  // Phantom itself (e.g., locks the wallet from the popup).
  useEffect(() => {
    if (detection !== "available") return;
    const provider: PhantomProvider | null = getPhantomProvider();
    if (!provider?.on) return;
    const onDisconnect = () => {
      setPubkey(null);
      setError(null);
    };
    const onAccountChanged = () => {
      // Re-derive from the provider's current public key.
      const next = provider.publicKey?.toBase58() ?? null;
      setPubkey(next);
    };
    provider.on("disconnect", onDisconnect);
    provider.on("accountChanged", onAccountChanged);
    return () => {
      provider.removeListener?.("disconnect", onDisconnect);
      provider.removeListener?.("accountChanged", onAccountChanged);
    };
  }, [detection]);

  const handleConnect = useCallback(async () => {
    setBusy(true);
    setError(null);
    try {
      const { pubkey: nextPubkey } = await connectPhantom();
      setPubkey(nextPubkey);
    } catch (err) {
      const msg = err instanceof Error ? err.message : "Connection failed";
      setError(msg);
    } finally {
      setBusy(false);
    }
  }, []);

  const handleDisconnect = useCallback(async () => {
    setBusy(true);
    setError(null);
    try {
      await disconnectPhantom();
      setPubkey(null);
    } catch (err) {
      const msg = err instanceof Error ? err.message : "Disconnect failed";
      setError(msg);
    } finally {
      setBusy(false);
    }
  }, []);

  // ── Render branches ────────────────────────────────────────────────────────

  if (detection === "checking") {
    return (
      <span className="text-xs text-muted-foreground" data-testid="wallet-connect-checking">
        checking wallet…
      </span>
    );
  }

  if (detection === "missing") {
    return (
      <a
        href="https://phantom.app/"
        target="_blank"
        rel="noopener noreferrer"
        className="text-xs underline text-muted-foreground hover:text-foreground"
        data-testid="wallet-connect-install"
      >
        Install Phantom →
      </a>
    );
  }

  if (pubkey) {
    return (
      <div className="flex items-center gap-2" data-testid="wallet-connect-connected">
        <Badge variant="secondary" className="font-mono text-[11px]">
          {shortPubkey(pubkey, 4)}
        </Badge>
        <Button
          variant="ghost"
          size="sm"
          onClick={handleDisconnect}
          disabled={busy}
          aria-label="Disconnect wallet"
          className="h-7 px-2 text-xs"
        >
          {busy ? "…" : "disconnect"}
        </Button>
      </div>
    );
  }

  return (
    <div className="flex items-center gap-2" data-testid="wallet-connect-disconnected">
      <Button
        size="sm"
        onClick={handleConnect}
        disabled={busy}
        className="h-7 px-3 text-xs"
      >
        {busy ? "Connecting…" : "Connect Phantom"}
      </Button>
      {error && (
        <span className="text-xs text-destructive max-w-[260px] truncate" title={error}>
          {error}
        </span>
      )}
    </div>
  );
}
