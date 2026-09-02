import { useCallback, useEffect, useState } from "react";
import { fetchSnapshot, type Snapshot } from "./api";

const DEFAULT_BASE = "http://127.0.0.1:4477";
const POLL_MS = 3000;

function formatTime(ms: number): string {
  return new Date(ms).toLocaleTimeString();
}

function isDenial(decision: string): boolean {
  return decision.startsWith("Deny") || decision.startsWith("Escalate");
}

function AgentsPanel({ agents }: { agents: Snapshot["agents"] }) {
  return (
    <section className="panel">
      <h2>Agents ({agents.length})</h2>
      <table>
        <thead>
          <tr>
            <th>name</th>
            <th>trust</th>
            <th>uid</th>
            <th>parent</th>
            <th>spawned</th>
          </tr>
        </thead>
        <tbody>
          {agents.map((a) => (
            <tr key={a.uid}>
              <td className="mono">{a.name}</td>
              <td>{a.trust}</td>
              <td className="mono dim">{a.uid.slice(0, 8)}</td>
              <td className="mono dim">{a.parent ? a.parent.slice(0, 8) : "—"}</td>
              <td className="dim">{formatTime(a.spawned_at_ms)}</td>
            </tr>
          ))}
          {agents.length === 0 && (
            <tr>
              <td colSpan={5} className="empty">
                no agents visible
              </td>
            </tr>
          )}
        </tbody>
      </table>
    </section>
  );
}

function DiaryPanel({ diary }: { diary: Snapshot["diary"] }) {
  const sorted = [...diary].sort((a, b) => b.seq - a.seq);
  return (
    <section className="panel">
      <h2>Diary ({diary.length})</h2>
      <table>
        <thead>
          <tr>
            <th>#</th>
            <th>kind</th>
            <th>content</th>
            <th>refs</th>
            <th>when</th>
          </tr>
        </thead>
        <tbody>
          {sorted.map((e) => (
            <tr key={e.seq}>
              <td className="mono dim">{e.seq}</td>
              <td>
                <span className={`badge kind-${e.kind.toLowerCase()}`}>{e.kind}</span>
              </td>
              <td>{e.content}</td>
              <td className="mono dim">{e.refs.length ? e.refs.join(", ") : "—"}</td>
              <td className="dim">{formatTime(e.at_ms)}</td>
            </tr>
          ))}
          {diary.length === 0 && (
            <tr>
              <td colSpan={5} className="empty">
                nothing recorded yet
              </td>
            </tr>
          )}
        </tbody>
      </table>
    </section>
  );
}

function SlipsPanel({ topics, slips }: { topics: Snapshot["topics"]; slips: Snapshot["slips"] }) {
  return (
    <section className="panel">
      <h2>Topics &amp; Slips ({topics.length} topics, {slips.length} slips)</h2>
      {topics.length === 0 ? (
        <p className="empty">no topics tagged yet</p>
      ) : (
        topics.map((topic) => (
          <div key={topic} className="topic-group">
            <h3>{topic}</h3>
            <ul>
              {slips
                .filter((s) => s.topic === topic)
                .map((s, i) => (
                  <li key={i}>
                    {s.descriptor} <span className="mono dim">→ #{s.refs.join(", #")}</span>
                  </li>
                ))}
            </ul>
          </div>
        ))
      )}
    </section>
  );
}

function IdeasPanel({ ideas }: { ideas: Snapshot["ideas"] }) {
  return (
    <section className="panel">
      <h2>Ideas ({ideas.length})</h2>
      <table>
        <thead>
          <tr>
            <th>#</th>
            <th>topic</th>
            <th>content</th>
            <th>status</th>
          </tr>
        </thead>
        <tbody>
          {[...ideas].sort((a, b) => b.id - a.id).map((s) => (
            <tr key={s.id} className={s.struck ? "struck" : ""}>
              <td className="mono dim">{s.id}</td>
              <td>{s.topic}</td>
              <td>{s.content}</td>
              <td className="dim">
                {s.struck ? "struck" : "live"}
                {s.supersedes !== null && ` (supersedes #${s.supersedes})`}
              </td>
            </tr>
          ))}
          {ideas.length === 0 && (
            <tr>
              <td colSpan={4} className="empty">
                no speculation jotted yet
              </td>
            </tr>
          )}
        </tbody>
      </table>
    </section>
  );
}

function AuditPanel({ audit }: { audit: Snapshot["audit"] }) {
  const denied = audit.filter((e) => isDenial(e.decision)).length;
  const sorted = [...audit].sort((a, b) => b.seq - a.seq);
  return (
    <section className="panel">
      <h2>
        Audit ({audit.length} checked, {denied} denied)
      </h2>
      <table>
        <thead>
          <tr>
            <th>#</th>
            <th>who</th>
            <th>action</th>
            <th>resource</th>
            <th>decision</th>
          </tr>
        </thead>
        <tbody>
          {sorted.map((e) => (
            <tr key={e.seq} className={isDenial(e.decision) ? "denied" : ""}>
              <td className="mono dim">{e.seq}</td>
              <td className="mono">{e.principal.agent}</td>
              <td>{e.action}</td>
              <td className="mono">{e.resource}</td>
              <td className="mono dim decision">{e.decision}</td>
            </tr>
          ))}
          {audit.length === 0 && (
            <tr>
              <td colSpan={5} className="empty">
                no checks recorded yet
              </td>
            </tr>
          )}
        </tbody>
      </table>
    </section>
  );
}

function initialBase(): string {
  // mnemosyne's DEBUG mode opens the dashboard with ?api=<its actual
  // API_PORT> — that always wins, since it reflects how THIS run was
  // actually configured, not whatever was saved from a previous one.
  const fromQuery = new URLSearchParams(window.location.search).get("api");
  return fromQuery ?? localStorage.getItem("mnemosyne-api-base") ?? DEFAULT_BASE;
}

export default function App() {
  const [base, setBase] = useState(initialBase);
  const [snapshot, setSnapshot] = useState<Snapshot | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [lastRefresh, setLastRefresh] = useState<number | null>(null);

  const refresh = useCallback(async () => {
    try {
      const snap = await fetchSnapshot(base);
      setSnapshot(snap);
      setError(null);
      setLastRefresh(Date.now());
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  }, [base]);

  useEffect(() => {
    localStorage.setItem("mnemosyne-api-base", base);
    refresh();
    const id = setInterval(refresh, POLL_MS);
    return () => clearInterval(id);
  }, [base, refresh]);

  return (
    <div className="app">
      <header>
        <h1>mnemosyne diagnostics</h1>
        <div className="controls">
          <label>
            API base
            <input value={base} onChange={(e) => setBase(e.target.value)} spellCheck={false} />
          </label>
          <button onClick={refresh}>refresh now</button>
          <span className="status">
            {error ? (
              <span className="error">⚠ {error}</span>
            ) : lastRefresh ? (
              `updated ${formatTime(lastRefresh)} · polling every ${POLL_MS / 1000}s`
            ) : (
              "connecting…"
            )}
          </span>
        </div>
      </header>

      {snapshot && (
        <main>
          <AgentsPanel agents={snapshot.agents} />
          <DiaryPanel diary={snapshot.diary} />
          <SlipsPanel topics={snapshot.topics} slips={snapshot.slips} />
          <IdeasPanel ideas={snapshot.ideas} />
          <AuditPanel audit={snapshot.audit} />
        </main>
      )}
    </div>
  );
}
