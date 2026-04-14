import Link from "next/link";
import { fetchDashboard } from "@/lib/api";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Badge } from "@/components/ui/badge";
import { Progress } from "@/components/ui/progress";
import { Separator } from "@/components/ui/separator";
import { formatCountdown, formatRelative, formatSol, shortPubkey } from "@/lib/format";

export default async function DashboardPage() {
  const snap = await fetchDashboard();

  const pendingCount = snap.pending.length;
  const chainCount = snap.pending.filter(
    (p) => p.request.policy_verdict.type === "RequiresHumanApproval" && p.request.policy_verdict.approval_chain,
  ).length;
  const expiringSoon = snap.expiring_soon.length;

  return (
    <div className="space-y-8">
      <header className="space-y-1">
        <h1 className="text-2xl font-semibold tracking-tight">Dashboard</h1>
        <p className="text-sm text-muted-foreground">
          Today&apos;s pending approvals, wallet spend, and expiring leases across the control plane.
        </p>
      </header>

      <div className="grid grid-cols-1 md:grid-cols-3 gap-4">
        <StatCard label="Pending approvals" value={pendingCount.toString()} hint={`${chainCount} multi-stage`} />
        <StatCard label="Expiring within 5 min" value={expiringSoon.toString()} hint="lease-enforced" tone={expiringSoon > 0 ? "warn" : undefined} />
        <StatCard label="Wallets monitored" value={snap.wallets.length.toString()} hint="with policy attached" />
      </div>

      <section className="space-y-3">
        <h2 className="text-sm font-medium uppercase tracking-wider text-muted-foreground">Pending approvals</h2>
        <div className="space-y-2">
          {snap.pending.map((p) => {
            const verdict = p.request.policy_verdict;
            const isChain = verdict.type === "RequiresHumanApproval" && !!verdict.approval_chain;
            const riskLabel = isChain ? "chain" : verdict.type === "RequiresHumanApproval" ? "single" : "unknown";
            const rule = verdict.type === "RequiresHumanApproval" ? verdict.rule_name : "";
            const expiring = snap.expiring_soon.find((e) => e.request_id === p.request.id);
            return (
              <Link
                key={p.request.id}
                href={`/approval/${p.request.id}`}
                className="block rounded-lg border bg-card px-5 py-4 hover:border-foreground/30 transition-colors"
              >
                <div className="flex items-start justify-between gap-6">
                  <div className="min-w-0 space-y-1">
                    <div className="flex items-center gap-2">
                      <Badge variant={riskLabel === "chain" ? "default" : "secondary"} className="capitalize">
                        {riskLabel}
                      </Badge>
                      <code className="text-xs text-muted-foreground">{rule}</code>
                    </div>
                    <div className="font-medium truncate">{p.proposal.description}</div>
                    <div className="text-xs text-muted-foreground">
                      wallet <code>{shortPubkey(p.proposal.wallet_pubkey)}</code> · requested {formatRelative(p.request.requested_at)}
                    </div>
                  </div>
                  <div className="text-right shrink-0">
                    {expiring && (
                      <div className="text-sm font-mono">
                        {formatCountdown(expiring.seconds_remaining)}
                      </div>
                    )}
                    <div className="text-xs text-muted-foreground">lease</div>
                  </div>
                </div>
              </Link>
            );
          })}
        </div>
      </section>

      <Separator />

      <section className="grid grid-cols-1 md:grid-cols-2 gap-6">
        <Card>
          <CardHeader>
            <CardTitle className="text-base">Wallet daily spend</CardTitle>
          </CardHeader>
          <CardContent className="space-y-4">
            {snap.wallets.map((w) => {
              const capLamports = w.policy?.max_amount_lamports ?? 0;
              const pct = capLamports > 0 ? Math.min(100, (w.daily_spend_lamports / capLamports) * 100) : 0;
              return (
                <div key={w.pubkey} className="space-y-1.5">
                  <div className="flex justify-between text-sm">
                    <div>
                      <span className="font-medium">{w.label}</span>{" "}
                      <code className="text-xs text-muted-foreground">{shortPubkey(w.pubkey)}</code>
                    </div>
                    <div className="text-muted-foreground">
                      {formatSol(w.daily_spend_lamports)}
                      {capLamports > 0 && <> / {formatSol(capLamports)}</>}
                    </div>
                  </div>
                  {capLamports > 0 && <Progress value={pct} />}
                </div>
              );
            })}
          </CardContent>
        </Card>

        <Card>
          <CardHeader>
            <CardTitle className="text-base">Expiring leases</CardTitle>
          </CardHeader>
          <CardContent>
            {snap.expiring_soon.length === 0 && (
              <div className="text-sm text-muted-foreground">Nothing expiring in the next window.</div>
            )}
            <ul className="space-y-2">
              {snap.expiring_soon.map((e) => (
                <li key={e.request_id} className="flex items-center justify-between text-sm">
                  <Link href={`/approval/${e.request_id}`} className="font-mono text-xs hover:underline">
                    {shortPubkey(e.request_id, 6)}
                  </Link>
                  <span className="font-mono">{formatCountdown(e.seconds_remaining)}</span>
                </li>
              ))}
            </ul>
          </CardContent>
        </Card>
      </section>
    </div>
  );
}

function StatCard({ label, value, hint, tone }: { label: string; value: string; hint?: string; tone?: "warn" }) {
  return (
    <Card className={tone === "warn" ? "border-amber-500/40" : undefined}>
      <CardContent className="py-6">
        <div className="text-xs uppercase tracking-wider text-muted-foreground">{label}</div>
        <div className="mt-2 text-3xl font-semibold tracking-tight">{value}</div>
        {hint && <div className="mt-1 text-xs text-muted-foreground">{hint}</div>}
      </CardContent>
    </Card>
  );
}
