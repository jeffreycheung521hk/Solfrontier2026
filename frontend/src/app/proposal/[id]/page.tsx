import Link from "next/link";
import { fetchApproval } from "@/lib/api";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Badge } from "@/components/ui/badge";
import { Separator } from "@/components/ui/separator";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { formatSol, formatToken, mintSymbol, shortPubkey } from "@/lib/format";

export default async function ProposalReviewPage({
  params,
}: {
  params: Promise<{ id: string }>;
}) {
  const { id } = await params;
  const { request, proposal } = await fetchApproval(id);
  const sim = request.simulation;
  const verdict = request.policy_verdict;

  return (
    <div className="space-y-8">
      <header className="space-y-1">
        <div className="text-xs uppercase tracking-wider text-muted-foreground">Proposal review</div>
        <h1 className="text-2xl font-semibold tracking-tight">
          {proposal?.description ?? request.description}
        </h1>
        {proposal ? (
          <p className="text-sm text-muted-foreground">
            wallet <code>{shortPubkey(proposal.wallet_pubkey)}</code> · network{" "}
            <Badge variant="outline" className="uppercase">{proposal.network}</Badge>
          </p>
        ) : (
          <p className="text-sm text-muted-foreground">
            full proposal details are not exposed by the live gateway yet — showing
            approval-request data only.
          </p>
        )}
      </header>

      {/* Why this was escalated */}
      <VerdictBanner verdict={verdict} />

      {/* Simulation */}
      <Card>
        <CardHeader>
          <CardTitle className="text-base">Simulation result</CardTitle>
        </CardHeader>
        <CardContent>
          <div className="grid grid-cols-2 md:grid-cols-4 gap-4 text-sm">
            <StatLine label="Status" value={sim.success ? "success" : "failed"} tone={sim.success ? "ok" : "err"} />
            <StatLine label="Compute units" value={sim.compute_units_used?.toLocaleString() ?? "—"} />
            <StatLine label="Fee" value={sim.fee_lamports ? formatSol(sim.fee_lamports) : "—"} />
            <StatLine label="Account diffs" value={sim.account_diffs.length.toString()} />
          </div>
          {sim.error && (
            <Alert variant="destructive" className="mt-4">
              <AlertTitle>Simulation error</AlertTitle>
              <AlertDescription>{sim.error}</AlertDescription>
            </Alert>
          )}
          {sim.logs.length > 0 && (
            <>
              <Separator className="my-4" />
              <div className="text-xs uppercase tracking-wider text-muted-foreground mb-2">Program logs</div>
              <pre className="text-xs bg-muted rounded-md p-3 overflow-x-auto leading-relaxed">
                {sim.logs.join("\n")}
              </pre>
            </>
          )}
        </CardContent>
      </Card>

      {/* Instructions */}
      <Card>
        <CardHeader>
          <CardTitle className="text-base">Instructions &amp; token flow</CardTitle>
        </CardHeader>
        <CardContent className="space-y-5">
          {!proposal && (
            <div className="text-sm text-muted-foreground">
              Instruction detail is not returned by the current gateway endpoints.
              This pane will populate once <code>GET /approvals/:id</code> is wired.
            </div>
          )}
          {(proposal?.instructions_summary ?? []).map((ix, i) => {
            const tt = ix.token_transfer;
            const tokenAmount = tt?.amount != null && tt.decimals != null
              ? formatToken(tt.amount, tt.decimals, mintSymbol(tt.mint))
              : null;
            return (
              <div key={i} className="border rounded-md p-4 space-y-3 bg-card">
                <div className="flex items-center flex-wrap gap-2">
                  <Badge variant="secondary">{ix.program_name ?? "program"}</Badge>
                  <code className="text-xs text-muted-foreground">{shortPubkey(ix.program_id)}</code>
                  {ix.is_legacy_token_transfer && (
                    <Badge variant="destructive">legacy Transfer</Badge>
                  )}
                </div>
                <div className="text-sm">{ix.description}</div>
                {tt && (
                  <div className="grid grid-cols-2 md:grid-cols-4 gap-3 text-xs bg-muted/50 rounded-md p-3">
                    <FieldLine label="Mint" value={tt.mint ? `${mintSymbol(tt.mint)} · ${shortPubkey(tt.mint)}` : "—"} />
                    <FieldLine label="Amount" value={tokenAmount ?? "—"} />
                    <FieldLine label="From" value={tt.source ? shortPubkey(tt.source) : "—"} />
                    <FieldLine label="To" value={tt.destination ? shortPubkey(tt.destination) : "—"} />
                  </div>
                )}
                {ix.transfer_lamports != null && (
                  <div className="text-sm">
                    <span className="text-muted-foreground">SOL transfer:</span> {formatSol(ix.transfer_lamports)}
                  </div>
                )}
                <div>
                  <div className="text-xs uppercase tracking-wider text-muted-foreground mb-1.5">Accounts</div>
                  <div className="space-y-1 text-xs">
                    {ix.accounts.map((a) => (
                      <div key={a.pubkey} className="flex items-center gap-2">
                        <code>{shortPubkey(a.pubkey)}</code>
                        {a.label && <span className="text-muted-foreground">{a.label}</span>}
                        <div className="ml-auto flex gap-1">
                          {a.is_signer && <Badge variant="outline" className="text-[10px]">signer</Badge>}
                          {a.is_writable && <Badge variant="outline" className="text-[10px]">writable</Badge>}
                        </div>
                      </div>
                    ))}
                  </div>
                </div>
              </div>
            );
          })}
        </CardContent>
      </Card>

      <div className="flex gap-3">
        <Link
          href={`/approval/${request.id}`}
          className="inline-flex items-center rounded-md bg-primary text-primary-foreground px-4 py-2 text-sm font-medium hover:opacity-90"
        >
          View approval chain →
        </Link>
      </div>
    </div>
  );
}

function VerdictBanner({ verdict }: { verdict: Awaited<ReturnType<typeof fetchApproval>>["request"]["policy_verdict"] }) {
  if (verdict.verdict === "approved") {
    return (
      <Alert>
        <AlertTitle>Auto-approved</AlertTitle>
        <AlertDescription>
          Rule <code>{verdict.rule_name}</code> approved this proposal automatically.
        </AlertDescription>
      </Alert>
    );
  }
  if (verdict.verdict === "rejected") {
    return (
      <Alert variant="destructive">
        <AlertTitle>Rejected by policy</AlertTitle>
        <AlertDescription>
          <strong>{verdict.rule_name}:</strong> {verdict.reason}
        </AlertDescription>
      </Alert>
    );
  }
  if (verdict.verdict === "requires_human_approval") {
    const chain = verdict.approval_chain;
    return (
      <Alert className="border-amber-500/40">
        <AlertTitle>Escalated to human approval</AlertTitle>
        <AlertDescription>
          <div>
            <strong>{verdict.rule_name}:</strong> {verdict.reason}
          </div>
          {chain && chain.length > 0 ? (
            <div className="mt-2 text-xs">
              Chain: {chain.map((s) => `${s.role}${s.min_approvals > 1 ? ` ×${s.min_approvals}` : ""}`).join(" → ")}
            </div>
          ) : (
            verdict.required_approver_role && (
              <div className="mt-2 text-xs">Required role: {verdict.required_approver_role}</div>
            )
          )}
        </AlertDescription>
      </Alert>
    );
  }
  return null;
}

function StatLine({ label, value, tone }: { label: string; value: string; tone?: "ok" | "err" }) {
  return (
    <div>
      <div className="text-xs uppercase tracking-wider text-muted-foreground">{label}</div>
      <div className={`text-sm font-medium ${tone === "ok" ? "text-green-600" : tone === "err" ? "text-red-600" : ""}`}>
        {value}
      </div>
    </div>
  );
}

function FieldLine({ label, value }: { label: string; value: string }) {
  return (
    <div>
      <div className="text-[10px] uppercase tracking-wider text-muted-foreground">{label}</div>
      <div className="font-mono text-xs mt-0.5">{value}</div>
    </div>
  );
}
