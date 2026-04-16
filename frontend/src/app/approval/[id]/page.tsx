import Link from "next/link";
import { fetchApproval, showcase } from "@/lib/api";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Badge } from "@/components/ui/badge";
import { Separator } from "@/components/ui/separator";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { formatRelative, shortPubkey } from "@/lib/format";
import { LeaseCountdown } from "@/components/lease-countdown";
import type { ApprovalStage, ApprovalWorkflow, StageDecision } from "@/lib/types";

export default async function ApprovalChainPage({
  params,
}: {
  params: Promise<{ id: string }>;
}) {
  const { id } = await params;
  const { request, workflow, proposal } = await fetchApproval(id);
  const title = proposal?.description ?? request.description;

  return (
    <div className="space-y-8">
      <header className="space-y-1">
        <div className="text-xs uppercase tracking-wider text-muted-foreground">Approval chain</div>
        <h1 className="text-2xl font-semibold tracking-tight">{title}</h1>
        <div className="flex items-center gap-3 text-sm text-muted-foreground">
          <code className="text-xs">{shortPubkey(request.id, 6)}</code>
          <span>·</span>
          <span>requested {formatRelative(request.requested_at)}</span>
          <span>·</span>
          <StateBadge state={workflow.state} />
        </div>
      </header>

      <LeaseBanner workflow={workflow} />

      <Tabs defaultValue="live">
        <TabsList>
          <TabsTrigger value="live">Current workflow</TabsTrigger>
          <TabsTrigger value="flow">Demo flow (showcase)</TabsTrigger>
        </TabsList>

        <TabsContent value="live" className="pt-4">
          <ChainVisualization workflow={workflow} />
        </TabsContent>

        <TabsContent value="flow" className="pt-4 space-y-6">
          <p className="text-sm text-muted-foreground">
            The same request at four points in its lifecycle. Every transition is lease-enforced
            and written to the audit trail.
          </p>
          <ChainSnapshot title="1. Pending — just created" workflow={showcase.pending} />
          <ChainSnapshot title="2. Risk approved, treasury quorum in progress" workflow={showcase.stageTwo} />
          <ChainSnapshot title="3. All stages cleared — approved, ready to sign" workflow={showcase.approved} />
          <ChainSnapshot title="4. Alternative: lease expired before CFO decided" workflow={showcase.expired} />
        </TabsContent>
      </Tabs>

      <Separator />

      <div className="flex gap-3">
        <Link
          href={`/proposal/${request.id}`}
          className="inline-flex items-center rounded-md border px-4 py-2 text-sm hover:bg-accent"
        >
          ← Back to proposal review
        </Link>
        <Link
          href="/audit"
          className="inline-flex items-center rounded-md border px-4 py-2 text-sm hover:bg-accent"
        >
          See audit trail →
        </Link>
      </div>
    </div>
  );
}

function LeaseBanner({ workflow }: { workflow: ApprovalWorkflow }) {
  if (workflow.state === "approved") {
    return (
      <Alert>
        <AlertTitle>Approved — signing resumes</AlertTitle>
        <AlertDescription>
          All stages cleared {formatRelative(workflow.updated_at)}. The parked signing task has been
          unblocked and the transaction is being submitted.
        </AlertDescription>
      </Alert>
    );
  }
  if (workflow.state === "rejected") {
    return (
      <Alert variant="destructive">
        <AlertTitle>Rejected</AlertTitle>
        <AlertDescription>
          A stage decision rejected this request. The parked signing task was signalled and dropped.
        </AlertDescription>
      </Alert>
    );
  }
  if (workflow.state === "expired") {
    return (
      <Alert className="border-amber-500/40">
        <AlertTitle>Lease expired</AlertTitle>
        <AlertDescription>
          The approval lease expired before a terminal decision. The signing task was cancelled; the
          event was recorded as <code>approval_lease_expired_no_action</code>, not a false approval.
        </AlertDescription>
      </Alert>
    );
  }
  return (
    <div className="rounded-md border bg-card px-4 py-3 text-sm flex items-center justify-between">
      <div>
        <div className="font-medium">Lease active</div>
        <div className="text-xs text-muted-foreground">
          Enforced by <code>tokio::time::timeout</code>. On expiry the request becomes terminal
          without a false audit row.
        </div>
      </div>
      <div className="text-right">
        {workflow.expires_at ? (
          <LeaseCountdown expiresAt={workflow.expires_at} />
        ) : (
          <span className="font-mono text-lg">∞</span>
        )}
        <div className="text-xs text-muted-foreground">remaining</div>
      </div>
    </div>
  );
}

function ChainSnapshot({ title, workflow }: { title: string; workflow: ApprovalWorkflow }) {
  return (
    <Card>
      <CardHeader className="pb-3">
        <CardTitle className="text-sm flex items-center gap-3">
          <span>{title}</span>
          <StateBadge state={workflow.state} />
        </CardTitle>
      </CardHeader>
      <CardContent>
        <ChainVisualization workflow={workflow} compact />
      </CardContent>
    </Card>
  );
}

function ChainVisualization({ workflow, compact }: { workflow: ApprovalWorkflow; compact?: boolean }) {
  const terminal = workflow.state !== "pending";
  return (
    <div className={compact ? "space-y-2" : "space-y-3"}>
      {workflow.stages.map((stage, i) => {
        const status = stageStatus(workflow, stage);
        return (
          <div key={i} className="flex gap-4 items-start">
            <div className="flex flex-col items-center">
              <div
                className={`w-8 h-8 rounded-full border-2 flex items-center justify-center text-xs font-semibold ${
                  status === "done"
                    ? "bg-green-600 text-white border-green-600"
                    : status === "active"
                    ? "border-amber-500 text-amber-600"
                    : status === "rejected"
                    ? "bg-red-600 text-white border-red-600"
                    : status === "expired"
                    ? "bg-amber-200 text-amber-900 border-amber-400"
                    : "border-muted-foreground/30 text-muted-foreground"
                }`}
              >
                {status === "done" ? "✓" : status === "rejected" ? "✗" : i + 1}
              </div>
              {i < workflow.stages.length - 1 && <div className="w-px flex-1 bg-border min-h-8" />}
            </div>
            <div className="flex-1 pb-4">
              <div className="flex items-center gap-2">
                <span className="font-medium capitalize">
                  {stage.allowed_roles.join(" / ") || "any role"}
                </span>
                {stage.min_approvals > 1 && (
                  <Badge variant="outline">
                    quorum {stage.decisions.filter((d) => d.approved).length}/{stage.min_approvals}
                  </Badge>
                )}
                {terminal && status === "pending" && <Badge variant="outline">skipped</Badge>}
              </div>
              {stage.decisions.length > 0 && (
                <ul className="mt-1.5 space-y-1 text-xs text-muted-foreground">
                  {stage.decisions.map((d, j) => (
                    <li key={j}>
                      {d.approved ? "✓" : "✗"} {decisionLabel(d)} · {formatRelative(d.decided_at)}
                    </li>
                  ))}
                </ul>
              )}
              {stage.decisions.length === 0 && status === "active" && (
                <div className="mt-1 text-xs text-amber-600">awaiting decision</div>
              )}
            </div>
          </div>
        );
      })}
    </div>
  );
}

function stageStatus(workflow: ApprovalWorkflow, stage: ApprovalStage): "done" | "active" | "pending" | "rejected" | "expired" {
  const approved = stage.decisions.filter((d) => d.approved).length;
  const hasRejection = stage.decisions.some((d) => !d.approved);
  if (hasRejection) return "rejected";
  if (approved >= stage.min_approvals) return "done";
  if (workflow.state === "expired" && approved < stage.min_approvals && stage.decisions.length === 0) {
    // Non-first un-started stage when expired: "expired" for first active one, "pending" for rest.
    const isFirstPending = workflow.stages.find((s) => s.decisions.filter((d) => d.approved).length < s.min_approvals) === stage;
    return isFirstPending ? "expired" : "pending";
  }
  // Active = the first stage that isn't done
  const firstActive = workflow.stages.find(
    (s) => s.decisions.filter((d) => d.approved).length < s.min_approvals && !s.decisions.some((d) => !d.approved),
  );
  if (firstActive === stage) return "active";
  return "pending";
}

function decisionLabel(d: StageDecision): string {
  const who = d.operator_id ?? "unknown";
  const role = d.approver_role ? ` (${d.approver_role})` : "";
  return `${who}${role}`;
}

function StateBadge({ state }: { state: ApprovalWorkflow["state"] }) {
  const variant =
    state === "approved"
      ? "default"
      : state === "rejected"
      ? "destructive"
      : state === "expired"
      ? "outline"
      : "secondary";
  return <Badge variant={variant as never} className="capitalize">{state}</Badge>;
}
