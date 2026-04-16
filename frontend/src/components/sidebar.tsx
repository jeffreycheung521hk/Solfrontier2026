import Link from "next/link";
import { MODE } from "@/lib/mode";
import { Badge } from "@/components/ui/badge";

const NAV = [
  { href: "/", label: "Dashboard" },
  { href: "/proposal/d3b07384-d9a8-4a52-9f2c-1a2b3c4d5e6f", label: "Proposal review" },
  { href: "/approval/d3b07384-d9a8-4a52-9f2c-1a2b3c4d5e6f", label: "Approval chain" },
  { href: "/audit", label: "Audit trail" },
  { href: "/policy", label: "Policy" },
];

export function Sidebar() {
  return (
    <aside className="w-64 shrink-0 border-r bg-sidebar text-sidebar-foreground flex flex-col">
      <div className="px-5 py-6 border-b">
        <div className="text-lg font-semibold tracking-tight">ClawSolana</div>
        <div className="text-xs text-muted-foreground mt-0.5">
          policy-driven approval control plane
        </div>
        <div className="mt-3">
          <Badge variant={MODE === "live" ? "default" : "secondary"} className="uppercase tracking-wider">
            {MODE}
          </Badge>
        </div>
      </div>
      <nav className="flex-1 p-3 space-y-0.5 text-sm">
        {NAV.map((item) => (
          <Link
            key={item.href}
            href={item.href}
            className="block rounded-md px-3 py-2 hover:bg-sidebar-accent hover:text-sidebar-accent-foreground transition-colors"
          >
            {item.label}
          </Link>
        ))}
      </nav>
      <div className="p-4 text-xs text-muted-foreground border-t">
        Showcase fixtures mirror the live API shape. Toggle with
        <code className="mx-1 rounded bg-muted px-1 py-0.5">NEXT_PUBLIC_MODE=live</code>
      </div>
    </aside>
  );
}
