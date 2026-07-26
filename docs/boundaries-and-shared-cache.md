# Boundaries and the shared cache

Design notes for the two cross-cutting planes added to `ai-swarm`:
a **permission boundary** modelled on an OS, and a **shared KV cache** scoped by
model type. Both are trait-and-type scaffolding today — the shapes are settled,
most implementations are deliberately empty.

- Permission plane: `identity.rs`, `policy.rs`, `kernel.rs`, `audit.rs`
- Cache plane: `cache/entry.rs`, `cache/pool.rs`, `cache/federation.rs`

They are one system, not two. The cache decides what is *findable*; the policy
layer decides what is *permitted*; nothing reaches a model without both.

---

## 1. The permission boundary

### 1.1 Where the boundary is

Today `ServiceHandle` is the syscall surface, and it is wide open: any tool that
holds one can write any key, message any peer, and shut the swarm down.

The guard goes **at that handle**, not inside each tool:

```
  before                              after

  Tool ──► ServiceHandle ──► effect   Tool ──► GuardedServices ──► check ──► audit ──► effect
                                                     │                         │
           (tool must remember                       └── PolicyEngine          └── AuditSink
            to check — it won't)                          Allow/Deny/Escalate
```

A tool cannot forget a check it has no way to skip: the unchecked methods are
not on the object it holds. `GuardedServices::unguarded()` exists as a migration
escape hatch and every call site of it is a hole in the boundary.

### 1.2 The five nouns

| Noun | Type | Answers |
|---|---|---|
| Subject | `Principal` | who is acting — harness, agent role, model class, tenant, trust tier |
| Verb | `Action` | Read, Write, Invoke, Send, Publish, Pull, Spawn, Control |
| Object | `Resource` | Tool, Memory (key), Cache (pool + class + label), Peer, Model, Spawn, Swarm |
| Rule | `Rule` | subject pattern × actions × resource pattern × conditions → Allow / Deny / Escalate |
| Record | `AuditEvent` | who did what to which thing, decided how, with what result, caused by which message |

Eight verbs and seven object kinds is the whole vocabulary. It stays small on
purpose: a policy nobody can read in one sitting is a policy nobody audits.

### 1.3 Three invariants

**Default deny.** No matching rule → `DenyReason::NoMatchingRule`. Adding a tool
never silently widens anyone's reach.

**Deny wins.** Within a grant set, `Deny` beats `Escalate` beats `Allow`,
regardless of insertion order. This is what makes grant sets composable — you
can merge two sets without auditing the merge for ordering bugs.

**Attenuation only.** `GrantSet::attenuate` keeps a requested allow only if the
parent already covers it, and inherits every parent denial unconditionally. A
planner cannot spawn a worker with powers the planner does not itself hold, and
cannot delegate away a restriction it is under.

```
  parent:  allow read/write  mem:proj/*
           deny  read        mem:proj/secrets/*

  child asks for:                       result
    read  mem:proj/notes/*      →       kept    (narrower)
    read  mem:*                 →       dropped (wider)
    spawn *                     →       dropped (parent never held Spawn)
    (inherited)                 →       deny read mem:proj/secrets/*
```

### 1.4 Two enforcement points

- **Advertise time** — `ToolBroker` filters the toolbelt before the model is
  told what exists. An agent cannot be talked into calling a tool it was never
  shown; this shrinks the blast radius of prompt injection but is *not* a
  security boundary on its own.
- **Call time** — `GuardedServices::authorize_tool`. Authoritative.

Both matter. The first reduces temptation, the second is the actual wall.

### 1.5 Memory as an address space

Memory permissions are key-prefix patterns, which is how a flat KV store gets
process-like isolation:

```
  scratch/<harness>/*     private working set        agent rw
  proj/shared/*           shared results             all agents r, planner w
  proj/secrets/*          credentials                nobody; explicit deny
  audit/*                 the trail itself           kernel only
```

`storage_keys()` filters the *listing* too, per key. An unfiltered listing leaks
the shape of another agent's memory even when the values stay hidden.

### 1.6 Lifecycle

```
  1. DECLARE   AgentManifest      what the agent says it needs
  2. ADMIT     Kernel::admit      intersect with authority actually held
                                  → Principal + GrantSet   (never a union)
  3. ATTACH    Kernel::attach     wrap ServiceHandle → GuardedServices
  4. ADVERTISE ToolBroker         model sees only permitted tools
  5. CALL      guarded syscall
  6. CHECK     PolicyEngine       Allow / Deny / Escalate (+ obligations)
  7. RECORD    AuditSink          every outcome, allow and deny alike
  8. EFFECT    inner service      only now does anything happen
  9. REVOKE    Kernel::revoke_all bump epoch; live handles go stale
```

Steps 6 and 7 are not reorderable and not optional: **if the audit sink cannot
record the decision, the effect does not run.** An effect nobody can see having
happened is worse than one that did not happen.

`Admission::dropped` lists the rules a manifest asked for and did not get. Not
an error — but it is the first thing to look at when an agent starts failing in
ways nobody expected.

### 1.7 Escalation and obligations

`Effect::Escalate` is the third answer: refuse *for now* and route to an
`Approver` (a supervising harness, or a human). This is how you express "an
agent may deploy, but only with a person in the loop" without hard-coding a
prompt into a tool.

`Obligation` is the other half — permitted, but conditionally: `Redact`,
`LimitBytes`, `ExpireAfter`, `AuditVerbose`, `NotifyOnUse`. The kernel applies
them; tools never see them, so a tool cannot decline to honour one.

### 1.8 Revocation

Grants carry an epoch. `revoke_all()` bumps it and every outstanding handle
fails its next check with `DenyReason::StaleEpoch`. Blunt but immediate, and it
stops work at a syscall boundary rather than mid-write. Per-principal
revocation is a natural refinement; the epoch field is already per-grant-set.

---

## 2. The shared KV cache

### 2.1 The sharing rule

One pool per `ModelClass`. Agents share a pool when their classes match; they
reach into another pool only as far as `Compatibility` allows.

```rust
ModelClass { provider, family, revision, embedding_space, quantization }
```

```
  A (claude-opus-5) ─┐
  B (claude-opus-5) ─┼─► pool "anthropic/claude-opus/claude-opus-5/emb-v1/-"
  C (claude-opus-5) ─┘   Identical — everything shared, KV blocks included

  D (claude-sonnet-5, same embedding space)
      → SameEmbeddingSpace with the pool above:
        may read RAG chunks, tool results, code facts
        may NOT read summaries, plans (another model's judgment)
        may NOT read KV blocks (different tensor layout)

  E (llama-3, different embedding space)
      → Incompatible. Its own pool, fully isolated.
```

The compatibility ladder, weakest to strongest: `Incompatible <
SameEmbeddingSpace < SameFamily < Identical`. Each `ValueClass` declares the
floor it needs (`ValueClass::min_compatibility`), so "what may cross" is data,
not scattered `if` statements:

| Value class | Floor | Why |
|---|---|---|
| `KvBlock` | `Identical` | tensor layout is revision- and quantization-specific |
| `Summary`, `Plan` | `SameFamily` | carries the writing model's judgment |
| `RagChunk`, `Embedding` | `SameEmbeddingSpace` | vector-space artifacts |
| `ToolResult`, `CodeFact` | `SameEmbeddingSpace` | model-independent facts |

Note `quantization` splits the pool key but leaves classes `SameFamily`: an int8
and a bf16 copy of the same model can trade summaries and must not trade KV.

### 2.2 Three gates, in order

```
  read    agent ──► lookup(demand)
                       │
                       ├─ 1. geometry     near enough?         CachePool
                       ├─ 2. compatible   reusable class?      CachePool
                       └─ 3. authorized   label clears?        PolicyEngine
                                             │
                                   all three ▼ → value reaches the model
```

Gate 3 is not the cache's decision. `CachePool::lookup` returns **`Candidate`s,
not hits** — geometry produced them, policy has not yet cleared them.
`GuardedServices::cache_lookup` applies gate 3 per candidate, because one lookup
can return values under several different labels.

Keeping the naming honest here is the point. The failure mode this design exists
to prevent is "the embeddings matched" quietly becoming "the agent was allowed
to see it".

### 2.3 Labels travel with values

Every `Sidecar` carries a `GovernanceLabel`: tenant, branch, scopes, policy
version. Cross-tenant flow is refused structurally in `RuleSetPolicy::evaluate`
— before any rule is consulted — so no misconfigured allow can leak across a
tenant boundary. Branch and scope checks are ordinary rule conditions
(`LabelExcludesScopes`, `LabelIncludesScopes`).

`policy_version` is the subtle one: a value written under an older policy may
need re-deriving rather than reusing, even for the same principal.

### 2.4 Writes fail closed

Every write, local or remote, goes through a `Validator`: digest, embedding
space and dimension, normalization, compatibility floor, TTL, label presence,
declared vs actual byte count. `StandardValidator` currently refuses everything
with `RejectReason::ValidatorUnavailable`, which is the correct behaviour for a
safety check that has not been written yet.

Rejections are also evidence — see peer quality below.

---

## 3. Federation (optional, later)

Adopt this when a single shared pool stops being reachable from every node. The
model is Pluribus-shaped: **advertise metadata freely, move values on purpose.**

```
  control plane   compact ownership sketches, gossiped    cheap, bounded
  data plane      whole values, pulled after planning     expensive, explicit
```

A node publishes a `RangeSketch` — centroids, radii, optional low-rank shape,
counts, hotness, labels present — that fits a fixed byte budget no matter how
much it holds. That keeps gossip cost flat as the swarm grows. The cost is false
positives: a planner sometimes pulls a range that does not help. That is a
bandwidth bug, not a correctness bug, because every arriving value is
revalidated.

Two properties worth preserving in any implementation:

- **Truncation widens, never narrows.** `Ellipsoid::residual_radius` charges
  every dimension not explicitly advertised, so dropping axes to fit the budget
  costs extra pulls but can never hide a value that was really there.
- **Degrade precision before coverage.** Losing an axis widens a range; losing a
  range hides values entirely. `SketchBudget` encodes that order.

A `PullPlanner` should be judged on what it *declines*. Zero plans is a normal,
common, good answer. The four properties worth ranking on: aligned with actual
demand, novel relative to what the local pool already holds, diverse relative to
other candidates, and affordable after the local reserve.

`PeerQuality` closes the loop: every `RejectReason` is evidence, and a peer whose
responses keep failing validation gets planned against less. A compromised or
buggy node fades out without anyone having to notice manually.

Federation touches the permission plane at three distinct points, which is why
`Publish` and `Pull` are separate verbs from `Read`:

- `Publish` — may this node advertise this pool's shape at all? Centroids
  describe what a tenant is working on, even though no values move.
- `Pull` — may this principal move bytes from that peer?
- `Read` — may this principal see the label on what came back? Per item, after
  arrival, before it reaches a model.

---

## 4. What is real and what is not

**Implemented and tested (22 tests across the new modules):**
- `RuleSetPolicy` evaluation: default deny, deny-wins, conditions, structural
  tenant isolation
- `GrantSet::attenuate` and pattern containment
- `Kernel::admit` — manifest intersection, trust capping, dropped-rule reporting
- `GuardedServices` check → audit → effect path, including stale-epoch
  revocation and filtered key listings
- `MemoryAudit`, event summaries, refusal filtering
- Model-class compatibility and pool-key derivation; pool byte budgeting

**Traits with no implementation yet:** `CachePool`, `CacheRegistry`,
`Validator`, `SketchPublisher`, `SketchDirectory`, `PullPlanner`,
`PeerTransport`, `PeerQuality`.

**Known gaps, in the order they should be closed:**

1. **Tools still take `&ServiceHandle`.** Until `Tool::call` takes a
   `&GuardedServices`, the guard is an *additional* gate rather than the only
   one. This is the single most important next change.
2. **`Swarm` does not build a `Kernel`.** Harnesses get a raw `ServiceHandle` at
   spawn; admission needs to happen in `Swarm::register`/`run`.
3. **No `begin_activation` call in `StandardHarness`.** Without it every audit
   record correlates to `"boot"`, and per-activation step and byte budgets never
   reset.
4. **No in-memory `CachePool`.** Needed before any of the cache path can be
   tested end to end.
5. **`prev_digest` is unused.** Hash-chaining the audit trail makes silent
   deletion detectable; cheap to add, hard to retrofit meaningfully.
