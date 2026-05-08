"use client";

// Stage 1 Tail — `<CanonicalIntentPreview>`.
//
// Renders a canonical intent and its locally-computed canonical hash so
// the operator can visually verify "this is exactly what I'm about to
// sign" before invoking Phantom. When the caller supplies an
// `expectedHashHex` (the hash the backend / pre-existing artifact
// claims), this component re-computes the hash locally over the same
// Borsh bytes and refuses to render a green / Continue state on
// mismatch.
//
// Security framing (mirrors the Rust crate doc):
//
//   - Stage 1 Tail is *tamper-evident*, not full action-enforcement.
//   - The frontend re-hash is the real-time tamper check before
//     signing.
//   - The on-chain `record_intent` PDA is forensic / audit proof
//     after signing.
//   - This component does NOT call any signing flow itself — it
//     exposes a `verification` status (matching, mismatch, computing,
//     error) and the caller is responsible for refusing to sign when
//     `verification.matches !== true`.
//
// What this component does NOT do:
//
//   - Does NOT trigger Phantom.
//   - Does NOT call any API.
//   - Does NOT submit, broadcast, or record_intent on chain.
//   - Does NOT poll RPC for current slot. `currentSlot` is an
//     optional caller-supplied prop.

import { useEffect, useMemo, useState } from "react";

import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Separator } from "@/components/ui/separator";
import { shortPubkey } from "@/lib/format";
import {
  bytesToHex,
  canonicalHash,
  checkIntentExpiry,
  type CanonicalIntent,
  type IntentExpiryStatus,
} from "@/lib/canonical-intent";

// ── Local hash-verification state machine ─────────────────────────────────

type VerificationState =
  | { kind: "computing" }
  | {
      kind: "computed";
      computed_hash_hex: string;
      expected_hash_hex: string | null;
      matches: boolean | null; // null = no expected hash supplied
    }
  | { kind: "error"; error: string };

interface CanonicalIntentPreviewProps {
  intent: CanonicalIntent;
  /** When supplied, the component re-computes the hash locally and
   *  surfaces a hard mismatch refusal banner if the strings differ.
   *  Lowercase recommended; the component normalises before compare. */
  expectedHashHex?: string | null;
  /** Optional caller-supplied chain slot for live expiry display. If
   *  omitted, the component shows the static `expires_at_slot` only
   *  (no expired/valid state indicator). Stage 1 Tail does NOT poll. */
  currentSlot?: bigint | null;
}

export function CanonicalIntentPreview({
  intent,
  expectedHashHex,
  currentSlot,
}: CanonicalIntentPreviewProps) {
  const [verification, setVerification] = useState<VerificationState>({
    kind: "computing",
  });

  // Compute hash on intent change. Web Crypto digest is async; we set
  // state when it resolves. The dependency array intentionally mirrors
  // the bytes that affect the hash — anything that mutates them triggers
  // a re-compute.
  useEffect(() => {
    let cancelled = false;
    (async () => {
      try {
        const computed = await canonicalHash(intent);
        if (cancelled) return;
        if (typeof expectedHashHex === "string" && expectedHashHex.length > 0) {
          const expected = expectedHashHex.toLowerCase();
          setVerification({
            kind: "computed",
            computed_hash_hex: computed,
            expected_hash_hex: expected,
            matches: computed === expected,
          });
        } else {
          setVerification({
            kind: "computed",
            computed_hash_hex: computed,
            expected_hash_hex: null,
            matches: null,
          });
        }
      } catch (err) {
        if (cancelled) return;
        const msg =
          err instanceof Error ? err.message : "canonical hash compute failed";
        setVerification({ kind: "error", error: msg });
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [intent, expectedHashHex]);

  const expiry: IntentExpiryStatus = useMemo(
    () => checkIntentExpiry(intent, currentSlot ?? null),
    [intent, currentSlot],
  );

  return (
    <Card
      className="border-foreground/15"
      data-testid="canonical-intent-preview"
    >
      <CardHeader className="pb-3">
        <div className="flex items-center justify-between">
          <CardTitle className="text-sm font-medium">
            Canonical intent preview
          </CardTitle>
          <ActionKindBadge kind={intent.action.kind} />
        </div>
      </CardHeader>
      <CardContent className="space-y-3 text-xs">
        {/* Hard mismatch refusal banner — must be first so it's
            unmissable and so screen readers read it before the rest. */}
        {verification.kind === "computed" && verification.matches === false && (
          <Alert variant="destructive" data-testid="canonical-hash-mismatch">
            <AlertTitle>Canonical hash mismatch — refusing to sign</AlertTitle>
            <AlertDescription className="space-y-1 break-words">
              <div>
                The hash of the intent rendered here does not match the hash
                supplied by the backend. Do NOT sign. The two hashes are:
              </div>
              <div>
                computed:{" "}
                <code className="break-all">
                  {verification.computed_hash_hex}
                </code>
              </div>
              <div>
                expected:{" "}
                <code className="break-all">
                  {verification.expected_hash_hex}
                </code>
              </div>
            </AlertDescription>
          </Alert>
        )}

        <ActionSummary intent={intent} />

        <Separator />

        <div className="space-y-1">
          <KeyValueRow
            k="schema version"
            v={
              <span className="font-mono text-foreground">
                {intent.schema_version}
              </span>
            }
          />
          <KeyValueRow
            k="user"
            v={<code className="text-foreground">{shortPubkey(intent.user.base58, 6)}</code>}
          />
          <KeyValueRow
            k="intent id"
            v={
              <code className="font-mono text-foreground break-all">
                {bytesToHex(intent.intent_id)}
              </code>
            }
          />
          <KeyValueRow
            k="expires at slot"
            v={
              <span className="font-mono text-foreground">
                {intent.expires_at_slot.toString()}
              </span>
            }
          />
          <ExpiryRow expiry={expiry} />
        </div>

        <Separator />

        <ActionFields action={intent.action} />

        <Separator />

        <HashSection
          verification={verification}
          intentForCopy={intent}
        />

        <Alert className="border-amber-500/40">
          <AlertTitle className="text-xs">Tamper-evidence only</AlertTitle>
          <AlertDescription className="text-[11px]">
            This is tamper-evident, not full on-chain action enforcement. The
            frontend re-hash here is the real-time check before signing; the
            on-chain <code>record_intent</code> PDA is forensic / audit proof
            after signing. Stage 2 will add full transaction → intent action
            binding.
          </AlertDescription>
        </Alert>
      </CardContent>
    </Card>
  );
}

// ── Sub-components ─────────────────────────────────────────────────────────

function ActionKindBadge({ kind }: { kind: CanonicalIntent["action"]["kind"] }) {
  const label =
    kind === "solend_deposit"
      ? "solend deposit"
      : kind === "solend_withdraw_all"
        ? "solend withdraw-all"
        : "jupiter swap";
  return (
    <Badge variant="default" className="text-xs">
      {label}
    </Badge>
  );
}

function ActionSummary({ intent }: { intent: CanonicalIntent }) {
  const a = intent.action;
  switch (a.kind) {
    case "solend_deposit": {
      const ui = formatRawWithDecimals(a.amount_raw, a.amount_decimals);
      return (
        <p className="text-foreground/90">
          Deposit{" "}
          <span className="font-mono text-foreground">
            {ui} ({a.amount_raw.toString()} raw)
          </span>{" "}
          into Solend reserve{" "}
          <code>{shortPubkey(a.reserve_pubkey.base58, 4)}</code> on behalf of{" "}
          <code>{shortPubkey(a.wallet_pubkey.base58, 4)}</code>.
        </p>
      );
    }
    case "solend_withdraw_all":
      return (
        <p className="text-foreground/90">
          Withdraw <span className="font-semibold">all</span> collateral from
          obligation{" "}
          <code>{shortPubkey(a.obligation_pubkey.base58, 4)}</code> on Solend
          reserve <code>{shortPubkey(a.reserve_pubkey.base58, 4)}</code>.
        </p>
      );
    case "jupiter_swap":
      return (
        <p className="text-foreground/90">
          Swap{" "}
          <span className="font-mono text-foreground">
            {a.input_amount_raw.toString()} raw
          </span>{" "}
          of <code>{shortPubkey(a.input_mint.base58, 4)}</code> for{" "}
          <code>{shortPubkey(a.output_mint.base58, 4)}</code> at up to{" "}
          {(a.slippage_bps / 100).toFixed(2)}% slippage.
        </p>
      );
  }
}

function ActionFields({ action }: { action: CanonicalIntent["action"] }) {
  switch (action.kind) {
    case "solend_deposit":
      return (
        <div className="space-y-1">
          <div className="text-foreground/80 font-medium">Action fields</div>
          <KeyValueRow k="wallet" v={<code>{shortPubkey(action.wallet_pubkey.base58, 6)}</code>} />
          <KeyValueRow k="input mint" v={<code>{shortPubkey(action.input_mint.base58, 6)}</code>} />
          <KeyValueRow k="reserve" v={<code>{shortPubkey(action.reserve_pubkey.base58, 6)}</code>} />
          <KeyValueRow k="lending market" v={<code>{shortPubkey(action.lending_market.base58, 6)}</code>} />
          <KeyValueRow
            k="amount raw"
            v={<span className="font-mono text-foreground">{action.amount_raw.toString()}</span>}
          />
          <KeyValueRow
            k="amount decimals"
            v={<span className="font-mono text-foreground">{action.amount_decimals}</span>}
          />
        </div>
      );
    case "solend_withdraw_all":
      return (
        <div className="space-y-1">
          <div className="text-foreground/80 font-medium">Action fields</div>
          <KeyValueRow k="wallet" v={<code>{shortPubkey(action.wallet_pubkey.base58, 6)}</code>} />
          <KeyValueRow k="obligation" v={<code>{shortPubkey(action.obligation_pubkey.base58, 6)}</code>} />
          <KeyValueRow k="reserve" v={<code>{shortPubkey(action.reserve_pubkey.base58, 6)}</code>} />
          <KeyValueRow k="lending market" v={<code>{shortPubkey(action.lending_market.base58, 6)}</code>} />
        </div>
      );
    case "jupiter_swap":
      return (
        <div className="space-y-1">
          <div className="text-foreground/80 font-medium">Action fields</div>
          <KeyValueRow k="wallet" v={<code>{shortPubkey(action.wallet_pubkey.base58, 6)}</code>} />
          <KeyValueRow k="input mint" v={<code>{shortPubkey(action.input_mint.base58, 6)}</code>} />
          <KeyValueRow k="output mint" v={<code>{shortPubkey(action.output_mint.base58, 6)}</code>} />
          <KeyValueRow
            k="input amount raw"
            v={<span className="font-mono text-foreground">{action.input_amount_raw.toString()}</span>}
          />
          <KeyValueRow
            k="slippage bps"
            v={
              <span className="font-mono text-foreground">
                {action.slippage_bps} ({(action.slippage_bps / 100).toFixed(2)}%)
              </span>
            }
          />
        </div>
      );
  }
}

function ExpiryRow({ expiry }: { expiry: IntentExpiryStatus }) {
  if (expiry.state === "unknown") {
    return (
      <KeyValueRow
        k="expiry status"
        v={
          <span className="text-muted-foreground italic">
            current slot unknown — static expiry only
          </span>
        }
      />
    );
  }
  if (expiry.state === "expired") {
    return (
      <KeyValueRow
        k="expiry status"
        v={
          <span className="font-mono text-destructive font-semibold">
            EXPIRED (current {expiry.current_slot.toString()} ≥{" "}
            {expiry.expires_at_slot.toString()})
          </span>
        }
      />
    );
  }
  return (
    <KeyValueRow
      k="expiry status"
      v={
        <span className="font-mono text-foreground">
          valid (current {expiry.current_slot.toString()} &lt;{" "}
          {expiry.expires_at_slot.toString()})
        </span>
      }
    />
  );
}

function HashSection({
  verification,
  intentForCopy,
}: {
  verification: VerificationState;
  intentForCopy: CanonicalIntent;
}) {
  // Avoid an unused-prop warning while keeping the API forward-
  // compatible: the `intent` reference may be needed by future
  // callers if we add a "copy raw bytes" affordance.
  void intentForCopy;
  return (
    <div className="space-y-2">
      <div className="text-foreground/80 font-medium">Canonical hash</div>
      {verification.kind === "computing" && (
        <p className="text-muted-foreground italic">computing SHA-256…</p>
      )}
      {verification.kind === "computed" && (
        <div className="space-y-2">
          <div className="flex items-start gap-2">
            <code
              className="font-mono text-foreground break-all flex-1"
              data-testid="computed-hash"
            >
              {verification.computed_hash_hex}
            </code>
            <CopyHashButton hashHex={verification.computed_hash_hex} />
          </div>
          {verification.expected_hash_hex && verification.matches === true && (
            <p className="text-[11px] text-foreground/80">
              ✓ Matches the hash supplied by the backend.
            </p>
          )}
          {verification.expected_hash_hex === null && (
            <p className="text-[11px] text-muted-foreground italic">
              No expected hash supplied — local hash only. Backend
              integration of canonical-intent metadata is pending
              (Stage 1 Tail B2/I).
            </p>
          )}
        </div>
      )}
      {verification.kind === "error" && (
        <Alert variant="destructive">
          <AlertTitle>Hash compute error</AlertTitle>
          <AlertDescription className="break-words">
            {verification.error}
          </AlertDescription>
        </Alert>
      )}
    </div>
  );
}

function CopyHashButton({ hashHex }: { hashHex: string }) {
  const [copied, setCopied] = useState(false);
  const onClick = async () => {
    try {
      await navigator.clipboard.writeText(hashHex);
      setCopied(true);
      setTimeout(() => setCopied(false), 1_500);
    } catch {
      // Clipboard API may be unavailable (older browsers, missing
      // permission). Fail silently — the hash is right there in the
      // <code> block for the operator to select manually.
    }
  };
  return (
    <button
      type="button"
      onClick={onClick}
      className="text-[11px] rounded-md border bg-background px-2 py-1 hover:border-foreground/30 transition-colors"
      data-testid="copy-hash-button"
    >
      {copied ? "copied" : "copy"}
    </button>
  );
}

function KeyValueRow({ k, v }: { k: string; v: React.ReactNode }) {
  return (
    <div className="flex items-baseline gap-3">
      <span className="text-muted-foreground min-w-[120px]">{k}</span>
      <span className="flex-1 break-all">{v}</span>
    </div>
  );
}

// ── Helpers ────────────────────────────────────────────────────────────────

function formatRawWithDecimals(raw: bigint, decimals: number): string {
  // Render `raw / 10^decimals` as a plain decimal string without
  // floating-point loss. Stage 1 Tail uses raw integer units only —
  // this helper is for display only.
  if (decimals === 0) return raw.toString();
  const s = raw.toString().padStart(decimals + 1, "0");
  const intPart = s.slice(0, -decimals);
  const fracPart = s.slice(-decimals);
  return `${intPart}.${fracPart}`;
}
