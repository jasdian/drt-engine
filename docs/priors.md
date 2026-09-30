# Priors — Phase 0 "Understand" synthesis

Decision record for the leaf-runtime project: a deterministic-execution runtime (Rust) for LLM-agent
tool subtrees + a benchmark harness testing whether **routing-only LLM + deterministic, seeded,
MCP-stdio leaf tools** beats the AXI baseline on cost at equal success rate.

Status: AXI and MCP facts below verified from source 2026-07-10. Prior-art claims verified against
vendor docs (URLs in Sources) on the same date. Phase-1 predictions in §5 are pre-registered —
written down before any experiment ran.

---

## 1. AXI baseline (github.com/kunchenguid/axi — verified from source 2026-07-10)

**What AXI is.** "Agent eXperience Interface": 10 design principles for agent-ergonomic CLI tools.
TypeScript pnpm monorepo, MIT license, ~1.4k stars, site axi.md.

**The claim under test.** The `bench-github` study (STUDY.md; table repeated in README) reports for
the `axi` condition **100% task success at $0.050/task**. Other conditions:

| condition            | success | cost/task |
|----------------------|---------|-----------|
| axi                  | 100%    | $0.050    |
| cli                  | 86%     | $0.054    |
| mcp-no-toolsearch    | 87%     | $0.148    |
| mcp-with-toolsearch  | 82%     | $0.147    |
| mcp-with-code-mode   | 84%     | $0.101    |

**Methodology (from source).**
- 17 tasks (`config/tasks.yaml`) run against **live** `openclaw/openclaw` GitHub data. Categories:
  single_step (5), multi_step (4), investigation (6), error_recovery (2).
- 5 conditions × 17 tasks × 5 repeats = **425 runs**.
- Agent: claude-sonnet-4-6 driven via `claude -p ... --output-format stream-json` with a generic
  Bash tool — **not** structured function-calling. 5-minute per-run timeout.
- Grading: binary LLM-as-judge with a per-task `grading_hint` ground truth; judge is
  claude-sonnet-4-6 for the Claude backend.
- Cost: computed from JSONL usage × pinned prices — sonnet $3 input / $0.30 cache-read /
  $15 output per MTok, cache-write at 1.25× = $3.75 (`src/usage.ts`). **Judge cost is excluded**
  from cost/task (`src/grader.ts`).

**Validity caveats we must handle in our replication:**
1. **Ground-truth drift** — grading hints hard-code live GitHub facts (issue counts, PR states)
   that drift over time; re-running the study later grades against stale truth.
2. **Possible cross-agent confound** — the MCP conditions' setup uses `codex mcp add`, and the
   grader has a gpt-5.4-mini path; the MCP conditions may have run under Codex/GPT rather than
   the Claude agent. If so, axi-vs-mcp is not a same-agent comparison.
3. **Author-run, self-judged** — the tool author ran and graded the study; no third-party runs.
4. **Cost is dominated by tool stdout verbosity**, i.e. the benchmark largely measures output
   formatting, which is precisely the variable AXI (and we) manipulate — fine, but it means the
   headline is an encoding result, not an agent-capability result.

**TOON format.** The spec is a **separate project** (github.com/toon-format/spec, toonformat.dev):
a tabular header+rows re-encoding of JSON (`users[2]{id,name}:` header followed by CSV-ish rows),
claimed ~40% token savings on uniform arrays. AXI references it as Principle #1; it is not part of
the AXI codebase.

---

## 2. Prior art / durable execution — clause-by-clause

Our product definition, as clauses:
- **(A)** seeded, journaled, replayable execution of an agent's **tool subtree** (not workflow code);
- **(B)** LLM used **only for routing** — every mechanical decision is deterministic code;
- **(C)** MCP-native **stdio** leaves, usable by any stock MCP client;
- **(D)** an explicit per-tool **determinism contract** (deterministic vs journaled-replay);
- **(E)** CI-grade **replay tests for agent behavior** (byte-identical re-runs as a test primitive);
- **(F)** hobbyist weight: single binary, **no server, no database**.

### Temporal (docs.temporal.io)
Provides: durable execution; event-sourced Event History; replay that re-executes Workflow code and
diffs emitted Commands against recorded Events (non-determinism error on mismatch); a hard
determinism constraint on workflow code — SDK APIs for time/random, versioning for code change.
This is the mature version of (A)'s journal+replay mechanics — but for **workflow code**, not agent
tool subtrees. Does not provide: (B) any LLM/agent framing, (C) MCP anything, (D) per-tool
determinism classes (its split is workflow-deterministic vs activity-anything), (E) replay as a CI
test primitive for agents, (F) — requires a Temporal Server cluster + persistence database
(or Temporal Cloud). Heaviest option here.

### Restate (docs.restate.dev)
Provides: durable execution with a journal; `ctx.run` persists non-deterministic operation results
and replays them ("without `run()`, these operations would produce different results during replay,
breaking deterministic recovery"); determinism requirement on handler code (`ctx.uuid()`, no raw
time/random). Closest to (F) among the engines: the Restate server is a self-contained single
binary (Rust, embedded storage) — but it is still a **server** your services register with, not an
embedded library. Does not provide: (B), (C), (D) as a tool-level contract, (E).

### Inngest (inngest.com/docs)
Provides: step-based memoization rather than full code replay — completed `step.run` results are
injected on retry instead of re-executed; pragmatic determinism rule (non-deterministic logic must
live inside steps). Does not provide: (A) full journaled replay (memoization resumes, it does not
re-verify), (B), (C), (D), (E), (F) — orchestration/state lives in the Inngest cloud or a
self-hosted Inngest platform.

### DBOS Transact (docs.dbos.dev)
Provides: **library-embedded** durable execution (no orchestration server — closest architectural
kin to what we want); every workflow input and step output checkpointed; deterministic-workflow
requirement (same args + same step outputs ⇒ same steps in same order); recovery replays from
checkpoints, returning cached step results. Does not provide: (F) fully — **requires Postgres** as
the system database; nor (B), (C), (D), (E).

### LangGraph checkpointing (docs.langchain.com / langgraph persistence)
Provides: agent-native state persistence — checkpointers save graph-state snapshots per super-step
per thread (InMemory/SQLite/Postgres backends); time travel and human-in-the-loop by resuming from
a saved checkpoint. This is **state snapshotting, not execution journaling**: resuming/forking
re-executes nodes (including tool calls) after the checkpoint; there is no recorded tool I/O to
replay against and no determinism contract, so re-runs are not reproducible. Provides the agent
framing (closest to (A)'s subject matter) with none of (A)'s replay semantics, and nothing of
(B)–(E); (F) partially (a library, but reproducibility isn't offered at any weight).

### Honest conclusion
Durable-execution engines already own journaled replay of **workflow code** — the mechanics of (A)
are solved technology, and Temporal/Restate/DBOS all enforce the same determinism discipline we
propose, just at a different layer. LangGraph owns **agent-state checkpointing**. Nobody occupies
the intersection: **seeded replay of the agent's tool subtree, with MCP-native stdio leaves, a
per-tool determinism contract (deterministic vs journaled-replay), and byte-identical replay as a
CI test primitive for agent behavior — in a single binary with no server and no database.** That
intersection is narrow. It is the entire bet. If Phase 1 shows the differentiator is worthless
(nobody needs seeded agent-subtree replay, or the economics headline dies per §5), the fallback
value is the replay/audit capability itself — deterministic re-execution of an agent trace for
debugging, regression testing, and audit — and the project continues under that honest framing.

---

## 3. MCP spec facts we build against

Protocol (modelcontextprotocol.io spec):
- JSON-RPC 2.0 over **stdio**; stdout is the wire, stderr is for logging. A server that prints
  anything else to stdout corrupts the transport.
- Lifecycle: `initialize` → `initialized` (notification) → `tools/list` → `tools/call`.
- Tool schemas are JSON Schema under `inputSchema`; `CallToolResult` carries content blocks +
  `isError`.

Implementation binding (verified against **rmcp 1.8.0** sources in the local cargo registry):
- `rmcp = "1"` with features `client`, `transport-io`, `transport-child-process`.
- Client: `().serve(TokioChildProcess::new(cmd))` → `RunningService<RoleClient, ()>` →
  `peer().list_all_tools()` / `call_tool()`.
- Server: manual `ServerHandler` impl + `serve(stdio())`.
- `TokioChildProcess` kills the child on drop — leaf process lifetime is tied to the client handle.

Determinism contract surface: MCP has **no standard server-metadata surface** for per-tool
contracts, so the contract rides a reserved **`__leaf_manifest` tool**. Any stock MCP client sees
it in `tools/list` and can call it; clients that don't know about it lose nothing.

---

## 4. Token counting for the experiments

- **`POST /v1/messages/count_tokens`**: free, model-specific, separate rate limit from
  `/v1/messages`. This is the only correct counter — **never tiktoken** (OpenAI's tokenizer;
  undercounts Claude tokenization by 15–20%+, which would silently bias every encoding
  experiment in §5).
- **`POST /v1/messages`** with `x-api-key` + `anthropic-version: 2023-06-01`; model pinned to
  **claude-sonnet-4-6** ($3 input / $15 output per MTok) to match the AXI study's agent and
  pricing. Two behavioral facts that shape the router: **assistant prefill returns 400** on this
  model — the router must use plain-text answers, not prefill-forced formats; omitting the
  `thinking` param = no thinking (no need to explicitly disable).

---

## 5. What Phase 1 must falsify (pre-registered predictions)

Written before running anything. Each has a pass/fail line.

**P1 — Encoding.** Terse structured text (TOON-style / aligned columns) beats pretty-printed JSON
by **20–40% tokens at equal extraction accuracy** on the same payloads. Hex/base64 encodings
inflate tokens **~2–3×** AND crater extraction accuracy — BPE fragments arbitrary bytes at
~2 bytes/token vs ~4 chars/token for terse English, and the model can't read through the encoding.
Falsified if: terse text saves <20%, or accuracy drops at equal information content.

**P2 — Determinism.** Pure/local leaves are byte-identical **10/10** repeated runs under a fixed
seed. Network-touching leaves are **not** (timestamps, ordering, upstream drift). Therefore the
contract needs classes `{pure, fs_stable, network}`, and `network` implies **journaled-replay
only** (record live once, replay the journal thereafter) — never claimed-deterministic.
Falsified if: pure leaves flake (contract untenable as designed) or network leaves happen to be
stable (classes collapse and the contract is over-engineered).

**P3 — Ceiling.** Hand-optimal decomposition (perfect routing, zero wasted turns) of two AXI
bench tasks costs ~**$0.006** for `view_pr` and ~**$0.017** for `merged_pr_ci_audit` — both
comfortably under the $0.050 AXI baseline, so the economic headline stays alive with routing
overhead to spare. **Pre-registered kill rule:** if hand-optimal ≥ $0.050 on **either** task, the
economic headline is dead — no amount of routing cleverness recovers it — and the project's value
proposition becomes the replay/audit capability (§2 fallback), continued with honest framing.

---

## Sources

- AXI: github.com/kunchenguid/axi — README.md, STUDY.md, `config/tasks.yaml`, `src/usage.ts`,
  `src/grader.ts` (read from source 2026-07-10).
- TOON: github.com/toon-format/spec; toonformat.dev.
- Temporal: docs.temporal.io/workflow-definition (determinism + Event History replay).
- Restate: docs.restate.dev/concepts/durable_building_blocks (journal, `ctx.run`, determinism).
- Inngest: inngest.com/docs/learn/how-functions-are-executed (step memoization).
- DBOS: docs.dbos.dev/architecture (embedded library, Postgres checkpointing, determinism rule).
- LangGraph: docs.langchain.com/oss/python/langgraph/persistence (checkpointers, backends,
  time travel, HITL).
- MCP spec: modelcontextprotocol.io (stdio transport, lifecycle, tools).
- rmcp 1.8.0 sources: local cargo registry (`~/.cargo/registry/src/*/rmcp-1.8.0/`) — client/server
  wiring + `TokioChildProcess` drop semantics (verified 2026-07-10).
- Anthropic API: docs.anthropic.com — Messages API, `count_tokens`, claude-sonnet-4-6 pricing.
