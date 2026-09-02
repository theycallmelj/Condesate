// Types mirror crates/mnemosyne/src/api.rs's JSON exactly — verified live
// against a running server (see that file's module docs) rather than
// guessed from the Rust source alone.

export interface Agent {
  uid: string;
  name: string;
  harness: string;
  parent: string | null;
  tenant: string;
  trust: string;
  spawned_at_ms: number;
}

export interface DiaryEntry {
  seq: number;
  at_ms: number;
  harness: string;
  activation: string;
  kind: string;
  content: string;
  refs: number[];
}

export interface Slip {
  descriptor: string;
  topic: string;
  refs: number[];
}

export interface Idea {
  id: number;
  topic: string;
  content: string;
  struck: boolean;
  supersedes: number | null;
}

export interface AuditEvent {
  seq: number;
  at_ms: number;
  principal: {
    uid: string;
    harness: string;
    agent: string;
    tenant: string;
    trust: string;
  };
  action: string;
  resource: string;
  // `format!("{:?}", Decision)` on the Rust side — a Rust-Debug-shaped
  // string (e.g. `Allow { rule: RuleId("..."), obligations: [] }`), not
  // structured JSON. Rendered as-is; see Diagnostics.tsx.
  decision: string;
  outcome: string;
  activation: string;
}

export interface Snapshot {
  agents: Agent[];
  diary: DiaryEntry[];
  topics: string[];
  slips: Slip[];
  ideas: Idea[];
  audit: AuditEvent[];
}

async function getJson<T>(base: string, path: string): Promise<T> {
  const res = await fetch(`${base}${path}`);
  if (!res.ok) {
    throw new Error(`${path} -> HTTP ${res.status}`);
  }
  return (await res.json()) as T;
}

/** One round-trip per endpoint, all in parallel — a single "refresh". */
export async function fetchSnapshot(base: string): Promise<Snapshot> {
  const [agents, diary, topics, slips, ideas, audit] = await Promise.all([
    getJson<Agent[]>(base, "/api/agents"),
    getJson<DiaryEntry[]>(base, "/api/memory/diary"),
    getJson<string[]>(base, "/api/memory/topics"),
    getJson<Slip[]>(base, "/api/memory/slips"),
    getJson<Idea[]>(base, "/api/memory/ideas"),
    getJson<AuditEvent[]>(base, "/api/audit"),
  ]);
  return { agents, diary, topics, slips, ideas, audit };
}
