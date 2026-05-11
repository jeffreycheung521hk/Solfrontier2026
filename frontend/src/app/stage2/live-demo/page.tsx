"use client";

// Stage 2 — `/stage2/live-demo` Phantom controlled-wallet funding UI.
//
// Lets the operator's connected Phantom wallet transfer EXACTLY 50
// USDC into the controlled demo wallet
// (`BPfDMmeMBmCbMC1rWh7hwigMBoKGBrKwXxSeUu9hhs5L`). This is the
// reliable true-RPC demo moment for Stage 2 — the controlled wallet
// is what the (future) automation watcher/executor will eventually
// act on, scoped by construction to those funds only.
//
// HARD SAFETY upheld here:
//   - No auto-send. Operator must click the button.
//   - Phantom always shows the approval popup before signing.
//   - No controlled-wallet keypair on the frontend (private keys
//     live ONLY on the operator's filesystem).
//   - No Solend live execution. No Jupiter conditional execution.
//   - No arbitrary amount input — the amount is pinned at exactly
//     50 USDC = 50_000_000 base units.
//   - The page is gated behind `NEXT_PUBLIC_STAGE2_LIVE_DEMO=1`.
//     Without that env var set, the page renders a disabled
//     explanation and never builds a transaction.
//   - Disabled when not connected, or when balance < 50 USDC, or
//     when the connected wallet equals the controlled wallet (no
//     self-funding loops without explicit user override).

import { useCallback, useEffect, useMemo, useState } from "react";

import { Connection, PublicKey } from "@solana/web3.js";
import type { Transaction } from "@solana/web3.js";

import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Separator } from "@/components/ui/separator";
import { WalletConnect } from "@/components/wallet-connect";
import { shortPubkey } from "@/lib/format";
import { getPhantomProvider } from "@/lib/phantom";
import {
  CONTROLLED_WALLET_BASE58,
  FUNDING_AMOUNT_BASE_UNITS,
  FUNDING_AMOUNT_UI_LABEL,
  USDC_DECIMALS,
  USDC_MINT_BASE58,
  buildFundingTransaction,
  deriveAtaPubkey,
  formatUsdcBaseUnits,
  solscanTxUrl,
} from "@/lib/stage2-funding";

// ── Env gates ────────────────────────────────────────────────────────────

const LIVE_DEMO_ENABLED =
  process.env.NEXT_PUBLIC_STAGE2_LIVE_DEMO === "1";

/// Public RPC endpoint. Operator can override to a Helius / Triton
/// URL via `NEXT_PUBLIC_SOLANA_RPC_URL`; any value set here is
/// browser-visible so DO NOT include an API key the operator wouldn't
/// already accept exposing.
const SOLANA_RPC_URL =
  process.env.NEXT_PUBLIC_SOLANA_RPC_URL ??
  "https://api.mainnet-beta.solana.com";

const CLUSTER_LABEL = "mainnet-beta";

// ── Funding-attempt state machine ────────────────────────────────────────

type FundingState =
  | { kind: "idle" }
  | { kind: "preparing" }
  | { kind: "awaiting_signature" }
  | { kind: "broadcasting" }
  | { kind: "confirming"; signature: string }
  | { kind: "finalized"; signature: string }
  | { kind: "error"; reason: string };

interface SourceAccount {
  pubkey: PublicKey;
  /** Source USDC ATA derived from `(connected, USDC_MINT)`. */
  ata: PublicKey;
  /** Balance in raw u64 base units. `null` while loading or if the
   *  ATA does not exist on chain. */
  balance_base_units: bigint | null;
  /** True iff `getAccountInfo(ata) !== null`. When false, the user
   *  has no USDC on this wallet. */
  ata_exists: boolean;
}

export default function Stage2LiveDemoPage() {
  // The page renders a disabled explanation when the env gate is off
  // so we never build / submit a transaction outside of an explicit
  // operator opt-in.
  if (!LIVE_DEMO_ENABLED) {
    return <EnvGateOffPanel />;
  }
  return <LiveDemoBody />;
}

// ── Disabled / env-gate-off panel ────────────────────────────────────────

function EnvGateOffPanel() {
  return (
    <div className="space-y-6">
      <header className="space-y-1">
        <div className="text-xs uppercase tracking-wider text-muted-foreground">
          Stage 2 live demo
        </div>
        <h1 className="text-2xl font-semibold tracking-tight">
          Controlled wallet funding
        </h1>
      </header>
      <Alert>
        <AlertTitle className="text-sm">Live demo gated off</AlertTitle>
        <AlertDescription className="text-xs space-y-2">
          <span className="block">
            This page transfers <strong>real USDC on mainnet</strong> and is
            therefore gated behind the explicit env var{" "}
            <code>NEXT_PUBLIC_STAGE2_LIVE_DEMO=1</code>. The current build did
            not set the gate; no transaction can be built or sent from this
            session.
          </span>
          <span className="block text-muted-foreground">
            To enable: stop the dev server, set{" "}
            <code>NEXT_PUBLIC_STAGE2_LIVE_DEMO=1</code> in{" "}
            <code>frontend/.env.local</code>, restart{" "}
            <code>npm run dev</code>, and reload this page.
          </span>
        </AlertDescription>
      </Alert>
    </div>
  );
}

// ── Live demo body ───────────────────────────────────────────────────────

function LiveDemoBody() {
  const [walletPubkey, setWalletPubkey] = useState<string | null>(null);
  const [source, setSource] = useState<SourceAccount | null>(null);
  const [destinationAta, setDestinationAta] = useState<PublicKey | null>(null);
  const [destinationAtaExists, setDestinationAtaExists] = useState<
    boolean | null
  >(null);
  const [loadingBalance, setLoadingBalance] = useState(false);
  const [state, setState] = useState<FundingState>({ kind: "idle" });
  const [overrideSelfFunding, setOverrideSelfFunding] = useState(false);

  // Single Connection per page lifetime — JSON-RPC GET-style helpers
  // only. We never sign or broadcast from this object directly; the
  // signed tx is sent via `sendRawTransaction` in `handleFund`.
  const connection = useMemo(
    () => new Connection(SOLANA_RPC_URL, "confirmed"),
    [],
  );

  // ── ATA derivation on wallet change ───────────────────────────────────
  useEffect(() => {
    if (walletPubkey === null) {
      setSource(null);
      setDestinationAta(null);
      setDestinationAtaExists(null);
      return;
    }
    try {
      const payer = new PublicKey(walletPubkey);
      const usdcMint = new PublicKey(USDC_MINT_BASE58);
      const controlled = new PublicKey(CONTROLLED_WALLET_BASE58);
      const sourceAta = deriveAtaPubkey(payer, usdcMint);
      const destAta = deriveAtaPubkey(controlled, usdcMint);
      setSource({
        pubkey: payer,
        ata: sourceAta,
        balance_base_units: null,
        ata_exists: false,
      });
      setDestinationAta(destAta);
    } catch (err) {
      setState({
        kind: "error",
        reason:
          err instanceof Error
            ? `wallet pubkey parse failed: ${err.message}`
            : "wallet pubkey parse failed",
      });
    }
  }, [walletPubkey]);

  // ── Balance + destination-ATA-exists fetch ────────────────────────────
  const refreshBalances = useCallback(async () => {
    if (source === null || destinationAta === null) return;
    setLoadingBalance(true);
    try {
      // Source: getTokenAccountBalance. Throws when the ATA does not
      // exist — we catch and treat as "balance = 0, ata_exists = false".
      let balance_base_units: bigint | null = null;
      let ata_exists = false;
      try {
        const r = await connection.getTokenAccountBalance(
          source.ata,
          "confirmed",
        );
        // `r.value.amount` is a base-unit decimal string.
        balance_base_units = BigInt(r.value.amount);
        ata_exists = true;
      } catch {
        balance_base_units = BigInt(0);
        ata_exists = false;
      }
      // Destination ATA: just check whether the account info exists.
      // If it doesn't, we'll include the idempotent create-ATA ix.
      const destInfo = await connection.getAccountInfo(
        destinationAta,
        "confirmed",
      );
      setDestinationAtaExists(destInfo !== null);
      setSource((prev) =>
        prev === null
          ? prev
          : { ...prev, balance_base_units, ata_exists },
      );
    } catch (err) {
      setState({
        kind: "error",
        reason:
          err instanceof Error
            ? `RPC balance lookup failed: ${err.message}`
            : "RPC balance lookup failed",
      });
    } finally {
      setLoadingBalance(false);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [source?.pubkey?.toBase58(), destinationAta?.toBase58()]);

  useEffect(() => {
    void refreshBalances();
  }, [refreshBalances]);

  // ── Disable-state derivation ──────────────────────────────────────────
  const isSelfFunding =
    walletPubkey !== null &&
    walletPubkey === CONTROLLED_WALLET_BASE58;
  const balanceSufficient =
    source !== null &&
    source.balance_base_units !== null &&
    source.balance_base_units >= FUNDING_AMOUNT_BASE_UNITS;

  const inFlight =
    state.kind === "preparing" ||
    state.kind === "awaiting_signature" ||
    state.kind === "broadcasting" ||
    state.kind === "confirming";

  const disabled =
    inFlight ||
    walletPubkey === null ||
    source === null ||
    destinationAta === null ||
    loadingBalance ||
    !balanceSufficient ||
    (isSelfFunding && !overrideSelfFunding);

  // ── Fund handler ──────────────────────────────────────────────────────
  const handleFund = useCallback(async () => {
    if (source === null || destinationAta === null) return;
    if (!balanceSufficient) return;
    if (isSelfFunding && !overrideSelfFunding) return;

    setState({ kind: "preparing" });

    // 1. Get a recent blockhash.
    let blockhash: string;
    try {
      const { blockhash: bh } = await connection.getLatestBlockhash(
        "confirmed",
      );
      blockhash = bh;
    } catch (err) {
      setState({
        kind: "error",
        reason:
          err instanceof Error
            ? `RPC blockhash fetch failed: ${err.message}`
            : "RPC blockhash fetch failed",
      });
      return;
    }

    // 2. Build the unsigned transaction.
    let tx: Transaction;
    try {
      tx = buildFundingTransaction({
        payer: source.pubkey,
        sourceAta: source.ata,
        destinationAta,
        // Idempotent — safe to include unconditionally; succeeds if
        // the ATA already exists. We still surface to the operator
        // whether it pre-existed so they understand the transaction
        // they're approving.
        includeCreateAta: true,
        recentBlockhash: blockhash,
      });
    } catch (err) {
      setState({
        kind: "error",
        reason:
          err instanceof Error
            ? `tx build failed: ${err.message}`
            : "tx build failed",
      });
      return;
    }

    // 3. Phantom signs (popup). Phantom only signs the payer slot;
    //    nothing else in this transaction has signer flag set.
    const provider = getPhantomProvider();
    if (!provider) {
      setState({
        kind: "error",
        reason: "Phantom provider not detected — please connect first.",
      });
      return;
    }
    setState({ kind: "awaiting_signature" });
    let signedTx: Transaction;
    try {
      signedTx = await provider.signTransaction(tx);
    } catch (err) {
      setState({
        kind: "error",
        reason:
          err instanceof Error
            ? `Phantom rejected / signing failed: ${err.message}`
            : "Phantom rejected / signing failed",
      });
      return;
    }

    // 4. Submit signed bytes via JSON-RPC.
    setState({ kind: "broadcasting" });
    let signature: string;
    try {
      const raw = signedTx.serialize();
      signature = await connection.sendRawTransaction(raw, {
        skipPreflight: false,
        preflightCommitment: "confirmed",
      });
    } catch (err) {
      setState({
        kind: "error",
        reason:
          err instanceof Error
            ? `RPC send failed: ${err.message}`
            : "RPC send failed",
      });
      return;
    }

    // 5. Confirm.
    setState({ kind: "confirming", signature });
    try {
      const conf = await connection.confirmTransaction(
        { signature, blockhash, lastValidBlockHeight: 0 },
        "confirmed",
      );
      if (conf.value.err !== null) {
        setState({
          kind: "error",
          reason: `Transaction confirmed with error: ${JSON.stringify(
            conf.value.err,
          )}`,
        });
        return;
      }
      setState({ kind: "finalized", signature });
      // Refresh balances so the UI shows the new state.
      void refreshBalances();
    } catch (err) {
      // Confirmation can fail with a transient error even though the
      // signature DID broadcast. Surface the signature so the
      // operator can verify on Solscan.
      setState({
        kind: "error",
        reason:
          err instanceof Error
            ? `Confirmation poll failed (tx may still land — check ${signature}): ${err.message}`
            : `Confirmation poll failed; signature: ${signature}`,
      });
    }
  }, [
    balanceSufficient,
    connection,
    destinationAta,
    isSelfFunding,
    overrideSelfFunding,
    refreshBalances,
    source,
  ]);

  // ── Render ────────────────────────────────────────────────────────────
  return (
    <div className="space-y-6">
      <header className="space-y-1">
        <div className="text-xs uppercase tracking-wider text-muted-foreground">
          Stage 2 live demo
        </div>
        <h1 className="text-2xl font-semibold tracking-tight">
          Controlled wallet funding
        </h1>
        <div className="flex items-center gap-3 text-sm text-muted-foreground flex-wrap">
          <Badge variant="default">{CLUSTER_LABEL}</Badge>
          <span>·</span>
          <Badge variant="outline">live mainnet · 50 USDC</Badge>
          <span className="ml-auto">
            <WalletConnect onChange={setWalletPubkey} />
          </span>
        </div>
      </header>

      <Alert className="border-amber-500/40">
        <AlertTitle className="text-sm">Read this before you sign</AlertTitle>
        <AlertDescription className="text-xs">
          This transfers exactly <strong>50 USDC</strong> from your connected
          wallet into a controlled demo wallet. Automation can only act on
          funds inside that controlled wallet — your main wallet is never
          delegated. No Solend / Jupiter execution is triggered from this
          page; the only outbound action is the SPL Token transfer you
          approve in Phantom.
        </AlertDescription>
      </Alert>

      <Card>
        <CardHeader className="pb-3">
          <CardTitle className="text-sm font-medium">
            Funding parameters
          </CardTitle>
        </CardHeader>
        <CardContent className="space-y-1 text-xs">
          <KeyValueRow
            k="cluster"
            v={<span className="font-mono text-foreground">{CLUSTER_LABEL}</span>}
          />
          <KeyValueRow
            k="connected wallet"
            v={
              walletPubkey === null ? (
                <span className="text-muted-foreground italic">
                  not connected
                </span>
              ) : (
                <code className="font-mono text-foreground">
                  {shortPubkey(walletPubkey, 6)}
                </code>
              )
            }
          />
          <KeyValueRow
            k="controlled wallet"
            v={
              <code className="font-mono text-foreground">
                {shortPubkey(CONTROLLED_WALLET_BASE58, 6)}
              </code>
            }
          />
          <KeyValueRow
            k="USDC mint"
            v={
              <code className="font-mono text-muted-foreground">
                {shortPubkey(USDC_MINT_BASE58, 6)}
              </code>
            }
          />
          <KeyValueRow
            k="source ATA"
            v={
              source === null ? (
                <span className="text-muted-foreground italic">
                  derived from connected wallet
                </span>
              ) : (
                <code className="font-mono text-muted-foreground">
                  {shortPubkey(source.ata.toBase58(), 6)}
                </code>
              )
            }
          />
          <KeyValueRow
            k="destination ATA"
            v={
              destinationAta === null ? (
                <span className="text-muted-foreground italic">—</span>
              ) : (
                <span>
                  <code className="font-mono text-muted-foreground">
                    {shortPubkey(destinationAta.toBase58(), 6)}
                  </code>
                  {destinationAtaExists === false && (
                    <span className="ml-2 text-amber-600">
                      (will be created — idempotent)
                    </span>
                  )}
                  {destinationAtaExists === true && (
                    <span className="ml-2 text-muted-foreground">
                      (already exists)
                    </span>
                  )}
                </span>
              )
            }
          />
          <KeyValueRow
            k="amount"
            v={
              <span>
                <span className="font-mono text-foreground">
                  {FUNDING_AMOUNT_UI_LABEL}
                </span>
                <span className="ml-2 text-muted-foreground">
                  ({FUNDING_AMOUNT_BASE_UNITS.toString()} base units)
                </span>
              </span>
            }
          />
          <KeyValueRow
            k="your USDC balance"
            v={
              source === null || source.balance_base_units === null ? (
                <span className="text-muted-foreground italic">
                  {loadingBalance ? "loading…" : "connect to load"}
                </span>
              ) : (
                <span
                  className={`font-mono ${
                    balanceSufficient
                      ? "text-foreground"
                      : "text-destructive font-semibold"
                  }`}
                >
                  {formatUsdcBaseUnits(source.balance_base_units)}
                </span>
              )
            }
          />
          <KeyValueRow
            k="decimals"
            v={
              <span className="font-mono text-muted-foreground">
                {USDC_DECIMALS}
              </span>
            }
          />
          <KeyValueRow
            k="instruction"
            v={
              <span className="font-mono text-foreground">
                TransferChecked (+ idempotent CreateAta if needed)
              </span>
            }
          />
        </CardContent>
      </Card>

      {isSelfFunding && (
        <Alert variant="destructive" data-testid="self-funding-warning">
          <AlertTitle className="text-sm">
            Connected wallet equals controlled wallet
          </AlertTitle>
          <AlertDescription className="text-xs space-y-2">
            <span className="block">
              The connected Phantom wallet is the SAME as the controlled
              demo wallet. Funding it from itself is a no-op — and almost
              certainly means you connected the wrong Phantom account.
            </span>
            <label className="flex items-center gap-2 text-foreground">
              <input
                type="checkbox"
                checked={overrideSelfFunding}
                onChange={(e) => setOverrideSelfFunding(e.target.checked)}
                data-testid="self-funding-override"
              />
              <span>
                I understand and want to proceed anyway (explicit override).
              </span>
            </label>
          </AlertDescription>
        </Alert>
      )}

      {state.kind === "error" && (
        <Alert variant="destructive">
          <AlertTitle className="text-sm">Funding error</AlertTitle>
          <AlertDescription className="text-xs break-words">
            {state.reason}
          </AlertDescription>
        </Alert>
      )}

      {(state.kind === "confirming" || state.kind === "finalized") && (
        <SignaturePanel state={state} />
      )}

      <Separator />

      <div className="flex flex-wrap items-center justify-between gap-3">
        <Button
          type="button"
          size="sm"
          variant="outline"
          onClick={() => void refreshBalances()}
          disabled={source === null || loadingBalance}
          data-testid="refresh-balance"
        >
          {loadingBalance ? "Refreshing…" : "Refresh balance"}
        </Button>
        <Button
          type="button"
          onClick={() => void handleFund()}
          disabled={disabled}
          aria-busy={inFlight}
          data-testid="fund-controlled-wallet-button"
        >
          {state.kind === "preparing"
            ? "Preparing…"
            : state.kind === "awaiting_signature"
              ? "Awaiting Phantom…"
              : state.kind === "broadcasting"
                ? "Broadcasting…"
                : state.kind === "confirming"
                  ? "Confirming…"
                  : state.kind === "finalized"
                    ? "Funded ✓"
                    : "Fund Controlled Wallet (50 USDC)"}
        </Button>
      </div>

      <div className="text-[11px] text-muted-foreground italic">
        Phantom will show its standard approval popup. You sign one
        transaction — a TransferChecked of 50 USDC. No Solend, Jupiter, or
        automation action is initiated from this page.
      </div>
    </div>
  );
}

// ── Sub-components ───────────────────────────────────────────────────────

function SignaturePanel({
  state,
}: {
  state:
    | { kind: "confirming"; signature: string }
    | { kind: "finalized"; signature: string };
}) {
  const finalized = state.kind === "finalized";
  return (
    <Alert
      className={finalized ? "border-green-500/40" : "border-foreground/15"}
      data-testid={`funding-${state.kind}`}
    >
      <AlertTitle className="text-sm">
        {finalized ? "Funding confirmed" : "Confirming on chain…"}
      </AlertTitle>
      <AlertDescription className="text-xs space-y-1">
        <div>
          tx{" "}
          <a
            href={solscanTxUrl(state.signature)}
            target="_blank"
            rel="noopener noreferrer"
            className="font-mono underline hover:text-foreground"
            data-testid="solscan-link"
          >
            {state.signature.slice(0, 8)}…{state.signature.slice(-6)}
          </a>
        </div>
        {finalized && (
          <div className="text-muted-foreground">
            50 USDC now in the controlled wallet. Automation may act only
            on this wallet&apos;s funds; your main wallet is untouched.
          </div>
        )}
      </AlertDescription>
    </Alert>
  );
}

function KeyValueRow({ k, v }: { k: string; v: React.ReactNode }) {
  return (
    <div className="flex items-baseline gap-3">
      <span className="text-muted-foreground min-w-[140px]">{k}</span>
      <span className="flex-1 break-all">{v}</span>
    </div>
  );
}
