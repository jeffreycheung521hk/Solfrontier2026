import { fetchPolicyRules, fetchWallets } from "@/lib/api";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Badge } from "@/components/ui/badge";
import { Separator } from "@/components/ui/separator";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { formatSol, formatToken, mintSymbol, shortPubkey } from "@/lib/format";
import type { PolicyAction, PolicyCondition, PolicyRule, WalletSummary } from "@/lib/types";

export default async function PolicyPage() {
  const [rules, wallets] = await Promise.all([fetchPolicyRules(), fetchWallets()]);

  return (
    <div className="space-y-8">
      <header className="space-y-1">
        <h1 className="text-2xl font-semibold tracking-tight">Policy</h1>
        <p className="text-sm text-muted-foreground">
          Rules are evaluated <strong>first-match-wins</strong>. Per-wallet overrides are prepended
          to the global rule set before evaluation.
        </p>
      </header>

      <Tabs defaultValue="global">
        <TabsList>
          <TabsTrigger value="global">Global rules</TabsTrigger>
          <TabsTrigger value="wallets">Per-wallet overrides</TabsTrigger>
        </TabsList>

        <TabsContent value="global" className="pt-4 space-y-3">
          {rules.map((r, i) => (
            <RuleCard key={r.name} rule={r} order={i + 1} />
          ))}
        </TabsContent>

        <TabsContent value="wallets" className="pt-4 space-y-4">
          {wallets.map((w) => (
            <WalletCard key={w.pubkey} w={w} />
          ))}
        </TabsContent>
      </Tabs>
    </div>
  );
}

function RuleCard({ rule, order }: { rule: PolicyRule; order: number }) {
  const action = rule.action;
  const tone =
    action.type === "Reject"
      ? "border-red-500/40"
      : action.type === "RequireApprovalChain"
      ? "border-amber-500/40"
      : action.type === "RequireHumanApproval"
      ? "border-amber-400/30"
      : "";
  return (
    <Card className={tone}>
      <CardHeader className="pb-3">
        <CardTitle className="text-sm flex items-center gap-3">
          <span className="font-mono text-xs text-muted-foreground">#{order}</span>
          <code>{rule.name}</code>
          <ActionBadge action={action} />
        </CardTitle>
      </CardHeader>
      <CardContent className="space-y-3">
        <div className="text-sm">{rule.description}</div>
        <Separator />
        <div className="grid md:grid-cols-2 gap-4">
          <div>
            <div className="text-xs uppercase tracking-wider text-muted-foreground mb-1">Condition</div>
            <ConditionLine c={rule.condition} />
          </div>
          <div>
            <div className="text-xs uppercase tracking-wider text-muted-foreground mb-1">Action</div>
            <ActionDetail a={action} />
          </div>
        </div>
      </CardContent>
    </Card>
  );
}

function WalletCard({ w }: { w: WalletSummary }) {
  const p = w.policy;
  return (
    <Card>
      <CardHeader className="pb-3">
        <CardTitle className="text-sm flex items-center gap-3">
          <span>{w.label}</span>
          <code className="text-xs text-muted-foreground">{shortPubkey(w.pubkey)}</code>
          <Badge variant="outline">{w.signer_type}</Badge>
        </CardTitle>
      </CardHeader>
      <CardContent className="grid md:grid-cols-2 gap-4 text-sm">
        {p?.max_amount_lamports != null && (
          <Field label="Per-tx cap">{formatSol(p.max_amount_lamports)}</Field>
        )}
        <Field label="Today's spend">{formatSol(w.daily_spend_lamports)}</Field>
        {p?.required_approver_role && (
          <Field label="Required approver role"><code>{p.required_approver_role}</code></Field>
        )}
        {p && p.program_allowlist.length > 0 && (
          <Field label="Program allowlist">
            <div className="flex flex-wrap gap-1 mt-1">
              {p.program_allowlist.map((prog) => (
                <code key={prog} className="text-[11px] bg-muted px-1.5 py-0.5 rounded">{shortPubkey(prog)}</code>
              ))}
            </div>
          </Field>
        )}
        {(!p || (p.program_allowlist.length === 0 && p.max_amount_lamports == null)) && (
          <Field label="Policy">no wallet-specific overrides</Field>
        )}
      </CardContent>
    </Card>
  );
}

function Field({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div>
      <div className="text-xs uppercase tracking-wider text-muted-foreground">{label}</div>
      <div className="mt-0.5">{children}</div>
    </div>
  );
}

function ConditionLine({ c }: { c: PolicyCondition }) {
  switch (c.type) {
    case "Always":
      return <Detail>always</Detail>;
    case "NetworkIn":
      return <Detail>network in [{c.networks.join(", ")}]</Detail>;
    case "ProgramNotInAllowlist":
      return <Detail>program ∉ allowlist</Detail>;
    case "DestinationInDenylist":
      return <Detail>destination in denylist</Detail>;
    case "AmountExceedsLamports":
      return <Detail>amount &gt; {formatSol(c.threshold)}</Detail>;
    case "CostExceedsSol":
      return <Detail>cost &gt; {c.threshold} SOL</Detail>;
    case "DailySpendExceedsSol":
      return <Detail>daily spend &gt; {c.threshold} SOL</Detail>;
    case "SimulationNotPassed":
      return <Detail>simulation failed</Detail>;
    case "TokenAmountExceeds":
      return (
        <Detail>
          {mintSymbol(c.mint)} transfer &gt; {formatToken(c.threshold, mintSymbol(c.mint) === "USDC" ? 6 : 0, mintSymbol(c.mint))}
        </Detail>
      );
    case "MintNotInAllowlist":
      return <Detail>mint ∉ allowlist ({c.allowed_mints.length} allowed)</Detail>;
    case "OutsideAllowedHours":
      return <Detail>outside allowed hours ({c.start_hour}:00–{c.end_hour}:00, UTC{c.utc_offset_hours >= 0 ? "+" : ""}{c.utc_offset_hours})</Detail>;
    case "LegacyTokenTransferPresent":
      return <Detail>legacy SPL Token Transfer detected</Detail>;
    default:
      return <Detail>{(c as { type: string }).type}</Detail>;
  }
}

function ActionDetail({ a }: { a: PolicyAction }) {
  switch (a.type) {
    case "Approve":
      return <Detail>approve</Detail>;
    case "Reject":
      return <Detail>reject — {a.reason}</Detail>;
    case "RequireHumanApproval":
      return (
        <Detail>
          require human approval{a.required_approver_role ? ` (role: ${a.required_approver_role})` : ""} —{" "}
          {a.reason}
        </Detail>
      );
    case "RequireApprovalChain":
      return (
        <Detail>
          <div>require approval chain</div>
          <ul className="mt-1 text-xs list-disc ml-4">
            {a.stages.map((s) => (
              <li key={s.role}>
                {s.role}
                {s.min_approvals > 1 ? ` × ${s.min_approvals}` : ""}
                {s.description ? ` — ${s.description}` : ""}
              </li>
            ))}
          </ul>
        </Detail>
      );
  }
}

function ActionBadge({ action }: { action: PolicyAction }) {
  const label: Record<PolicyAction["type"], string> = {
    Approve: "approve",
    Reject: "reject",
    RequireHumanApproval: "human",
    RequireApprovalChain: "chain",
  };
  const variant: Record<PolicyAction["type"], string> = {
    Approve: "secondary",
    Reject: "destructive",
    RequireHumanApproval: "default",
    RequireApprovalChain: "default",
  };
  return (
    <Badge variant={variant[action.type] as never} className="ml-auto">
      {label[action.type]}
    </Badge>
  );
}

function Detail({ children }: { children: React.ReactNode }) {
  return <div className="text-sm">{children}</div>;
}
