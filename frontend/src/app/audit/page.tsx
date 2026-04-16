import { fetchAuditTrail } from "@/lib/api";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Badge } from "@/components/ui/badge";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { shortPubkey } from "@/lib/format";
import type { AuditRow, AuditSeverity } from "@/lib/types";

export default async function AuditPage() {
  const rows = await fetchAuditTrail();
  const sorted = [...rows].sort((a, b) => b.occurred_at - a.occurred_at);

  return (
    <div className="space-y-8">
      <header className="space-y-1">
        <h1 className="text-2xl font-semibold tracking-tight">Audit trail</h1>
        <p className="text-sm text-muted-foreground">
          Every terminal decision, lease expiry, and policy rejection lands here.
          <code className="ml-1">approval_lease_expired</code> and{" "}
          <code>approval_lease_expired_no_action</code> are distinct from{" "}
          <code>human_rejected</code> — expiry never produces a false audit row.
        </p>
      </header>

      <Card>
        <CardHeader>
          <CardTitle className="text-base">Recent events</CardTitle>
        </CardHeader>
        <CardContent>
          <Table>
            <TableHeader>
              <TableRow>
                <TableHead className="w-40">When</TableHead>
                <TableHead className="w-56">Event</TableHead>
                <TableHead>Actor</TableHead>
                <TableHead>Correlation</TableHead>
                <TableHead>Payload</TableHead>
                <TableHead className="w-24">Severity</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {sorted.map((r) => (
                <TableRow key={r.id}>
                  <TableCell className="text-xs font-mono whitespace-nowrap">
                    {new Date(r.occurred_at).toISOString().replace("T", " ").slice(0, 19)}Z
                  </TableCell>
                  <TableCell>
                    <EventBadge type={r.event_type} />
                  </TableCell>
                  <TableCell className="text-xs">
                    {r.actor === "system" ? (
                      <Badge variant="outline" className="text-[10px]">system</Badge>
                    ) : (
                      <code>{r.actor}</code>
                    )}
                  </TableCell>
                  <TableCell className="text-xs font-mono">{shortPubkey(r.correlation_id, 6)}</TableCell>
                  <TableCell className="text-xs">
                    <PayloadCell row={r} />
                  </TableCell>
                  <TableCell>
                    <SeverityBadge s={r.severity} />
                  </TableCell>
                </TableRow>
              ))}
            </TableBody>
          </Table>
        </CardContent>
      </Card>
    </div>
  );
}

function EventBadge({ type }: { type: string }) {
  const tone: Record<string, "default" | "secondary" | "destructive" | "outline"> = {
    human_approved: "default",
    quorum_progress: "secondary",
    approval_lease_expired: "outline",
    approval_lease_expired_no_action: "outline",
    policy_rejected: "destructive",
    human_rejected: "destructive",
  };
  return <Badge variant={(tone[type] ?? "secondary") as never}><code className="text-[10px]">{type}</code></Badge>;
}

function SeverityBadge({ s }: { s: AuditSeverity }) {
  const color: Record<AuditSeverity, string> = {
    info: "text-green-600",
    warning: "text-amber-600",
    error: "text-red-600",
    critical: "text-red-700 font-semibold",
  };
  return <span className={`text-xs ${color[s]}`}>{s}</span>;
}

function PayloadCell({ row }: { row: AuditRow }) {
  let parsed: unknown = null;
  try {
    parsed = JSON.parse(row.payload);
  } catch {
    return <code className="text-muted-foreground truncate">{row.payload}</code>;
  }
  const obj = parsed as Record<string, unknown>;
  const keys = Object.keys(obj).slice(0, 3);
  return (
    <div className="flex flex-wrap gap-1.5">
      {keys.map((k) => (
        <span key={k} className="text-muted-foreground">
          <span className="uppercase text-[9px] tracking-wider">{k}</span>{" "}
          <code>{truncate(String(obj[k]), 28)}</code>
        </span>
      ))}
    </div>
  );
}

function truncate(s: string, n: number): string {
  return s.length > n ? s.slice(0, n) + "…" : s;
}
