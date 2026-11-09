/** CPU profile (CDP `Profiler.stop`) → functions by self time. */

export interface CpuProfile {
  nodes: Array<{
    id: number;
    callFrame: { functionName: string; url: string; lineNumber: number; columnNumber: number };
    children?: number[];
  }>;
  samples: number[];
  timeDeltas: number[];
}

export interface FunctionCost {
  name: string;
  where: string;
  selfMs: number;
  /** Time with the function anywhere on the stack (recursion counted once). */
  totalMs: number;
}

export function topFunctions(profile: CpuProfile, limit: number): FunctionCost[] {
  const byId = new Map(profile.nodes.map((n) => [n.id, n]));
  const parent = new Map<number, number>();
  for (const n of profile.nodes) for (const c of n.children ?? []) parent.set(c, n.id);
  const keyOf = (id: number) => {
    const f = byId.get(id)!.callFrame;
    const file = f.url.slice(f.url.lastIndexOf("/") + 1);
    return `${f.functionName || "(anonymous)"}\u0000${file ? `${file}:${f.lineNumber + 1}` : ""}`;
  };
  const self = new Map<string, number>();
  const total = new Map<string, number>();
  profile.samples.forEach((id, i) => {
    const ms = (profile.timeDeltas[i + 1] ?? 0) / 1000;
    const leaf = keyOf(id);
    if (leaf.startsWith("(idle)") || leaf.startsWith("(program)")) return;
    self.set(leaf, (self.get(leaf) ?? 0) + ms);
    const seen = new Set<string>();
    for (let n: number | undefined = id; n !== undefined; n = parent.get(n)) {
      const k = keyOf(n);
      if (seen.has(k)) continue;
      seen.add(k);
      total.set(k, (total.get(k) ?? 0) + ms);
    }
  });
  return [...self.entries()]
    .sort((a, b) => b[1] - a[1])
    .slice(0, limit)
    .map(([k, selfMs]) => {
      const [name, where] = k.split("\u0000") as [string, string];
      return { name, where, selfMs, totalMs: total.get(k) ?? selfMs };
    });
}
