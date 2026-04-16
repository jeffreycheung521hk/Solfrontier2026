"use client";

import { useEffect, useState } from "react";
import { formatCountdown } from "@/lib/format";

export function LeaseCountdown({ expiresAt }: { expiresAt: string }) {
  const target = new Date(expiresAt).getTime();
  // Start with 0 so SSR and first client render agree; interval updates after mount.
  const [now, setNow] = useState(0);

  useEffect(() => {
    const id = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(id);
  }, []);

  if (now === 0) return <span className="font-mono text-lg">…</span>;
  const seconds = Math.max(0, Math.floor((target - now) / 1000));
  return <span className="font-mono text-lg">{formatCountdown(seconds)}</span>;
}
