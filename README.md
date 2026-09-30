# DRT — Deterministic Routing Tree

> **The pattern.** The LLM routes only at genuine decision points over a tree whose
> forced sequences, arguments, and reductions are deterministic, journaled, and
> replayable. The model picks *which*, never *how*.
>
> [AXI](https://github.com/kunchenguid/axi) makes the tool surface agent-ergonomic;
> DRT makes the orchestration deterministic. Complementary, and orthogonal.

This repo is the reference DRT runtime — a deterministic-execution runtime for LLM
agents, in Rust. The LLM is invoked **only to make routing decisions** over a
**hybrid branching tree** of deterministic "leaf" plugins (MCP-over-stdio servers
wrapping `gh`/`jq`). The leaves own every argument, fetch, and reduction; the LLM
picks *which* capability. Everything below the routing decision is seeded, journaled,
and **replayable** — plus a benchmark that tests, honestly, how this compares to
AXI's `bench-github` baseline.

## Results

10 GitHub tasks × 5 repeats, model `gpt-5.4-mini`. The LLM's job is *routing*; the
pipelines are deterministic. All numbers regenerate from `bench/artifacts/` via
`leaf-bench report` (third-party reproducible). Task #10 (`repo_topics`) was added
end-to-end through the drt-no-mistakes gate — the extending guide's worked example,
committed; the matrix re-ran because `config_hash` pins every run to the tree digest.

**The routing runtime** (one general tree, three context-propagation policies):

| condition | routing accuracy | end-to-end | LLM-calls/task | cost/task |
| --------- | ---------------- | ---------- | -------------- | --------- |
| last_k    | **50/50 (100%)** | 50/50      | **1.20**       | $0.0003   |
| decayed   | **50/50 (100%)** | 45/50†     | **1.20**       | $0.0002   |
| full      | **50/50 (100%)** | 50/50      | **1.20**       | $0.0003   |

- **Routing accuracy = 150/150 (100%)** — scored against an independently-authored
  `expected_capability` label (not the tree's own output). Holds live too.
- **1.20 LLM calls/task**: a *forced* pipeline (fetch→reduce→done) routes in **1 call**;
  the two *hybrid* tasks make a genuine second decision, for 2. (An earlier all-LLM-routed
  tree spent 8–12 per task rubber-stamping forced steps.)
- † `decayed`'s 45/50 is the ambiguous-issue effect below, not an error.

**The fair comparison** — LIVE, same model, tool shape held equal (both reduce at the
tool boundary). The `control` is a free-driver given the same reduce primitive; the
only difference is orchestration (deterministic routing vs LLM free-drive):

| condition (live, `gpt-5.4-mini`) | success (AXI-faithful judge) | LLM-calls/task | cost/task   |
| -------------------------------- | ---------------------------- | -------------- | ----------- |
| **routing tree**                 | **86%** (43/50)              | **1.20**       | **$0.0003** |
| fused free-driver                | 84% (42/50)                  | 2.68           | $0.0018     |
| Claude Code (Sonnet + Bash)‡     | 78% (32/41)                  | many           | (Sonnet)    |

Under fair (drift-tolerant) grading the arms sit in the **same success band**:
each judge cell is a **majority-of-3 vote** (11/141 cells split — the residual
judge noise; a few grades of spread sit within it), exact-substring columns are
deterministic, and free-driving cells are graded against a **per-cell ground
truth** captured seconds after each answer (drift window: seconds, not minutes). The routing tree's advantage is **~2× fewer LLM calls, ~6× lower
cost, and determinism/replay** — not a success gap. (‡ AXI's `cli` harness,
exactly, as a cross-check — same-time on the 10-task matrix, rep-major under its
$4 Anthropic cap.)

## How it works — the hybrid tree

The LLM is spent only where there's a genuine choice — the DRT contract:

- A branch has a `route` mode. The **root is `llm`** (pick the capability). Each
  capability is a deterministic **`sequence`** the executor walks with **zero router
  calls** (fetch → reduce → terminal `done`). Determinism is not only a leaf property —
  a *branch* is deterministic wherever the next step is forced.
- A **hybrid capability** mixes both: `issue_triage` fetches an issue (via GraphQL,
  **no labels**), makes a genuine LLM `classify` decision (bug vs feature) over the
  title+body, then runs a deterministic reduce.

Everything below the routing decision is **journaled and replayable**: LLM decisions are
served from the journal (vendor nondeterminism quarantined), `deterministic` leaves are
re-executed and byte-asserted, `replay_only` (network) leaves are served from the journal
with zero network. Exploration is seeded epsilon-greedy (ChaCha8), rolls journaled. Leaves
are real MCP stdio servers (`leaf-wrap --manifest <json>`), verifiable with any stock MCP
client, sandboxed (binary allowlist, scrubbed env, working-dir jail, timeout).

## What the benchmark actually shows (honest findings)

- **Reduce-at-the-boundary is AXI's own winning idea.** AXI's 100% arm (`gh-axi`) pre-computes
  a count + small projection — exactly what our tree does. AXI's `cli`/`mcp` arms dump raw
  JSON and score lower. Our tree and our fused control both sit on the reducing side; the
  tree's edge over the free-driver is cost/calls/determinism, **not** raw-vs-reduced.
- **The tree's real advantage is pinning the semantics in code.** Before the task prompts
  spelled out ordering/counting rules, the free-driver added an unrequested `is:open` filter,
  took `.[:10]` without a `mergedAt` sort, and invented a different passed/failed counting
  policy on each repeat (23/2, 23/7, 33/5 on the same pinned PR). Pinning the semantics in
  the prompts fixed the interpretable rows; the tree never needed the prose — its reduces ARE
  the semantics. What still fails exact-substring for any live arm is irreducible at the
  seconds-wide per-cell grading window: formatting differences, reduce-semantics
  differences, and all-or-nothing surfaces that churn within seconds on a repo this active.
- **Propagation earns its keep in exactly one place, and it's real.** On a genuinely-ambiguous
  issue (#93120, "[Bug/Feature]"), `classify` goes **feature under `last_k`/`full` but BUG
  under `decayed`, 5/5 vs 5/5** (frozen *and* live): `decayed`'s lossy summary drops the body's
  configurability (feature) signal. It's the one place the retained-context policy changes a
  genuine semantic decision.

See [`docs/results.md`](docs/results.md) for the full tables and [`docs/SUMMARY.md`](docs/SUMMARY.md)
for the narrative.

## Quick Start

```sh
# Prereqs: Rust, `gh` (authenticated), `jq`. For live/billed phases, drop a key into a
# gitignored .env (provider auto-selected by which key is present):
cp .env.example .env      # then set OPENAI_API_KEY=...

cargo build --release
./target/release/leaf-bench preflight              # key, gh auth, CLIs, model reachable
./target/release/leaf-bench lint                   # static DRT invariant checks (offline, no spend)

# Note: the raw run artifacts and recorded GitHub cassettes (bench/artifacts/runs*,
# bench/cassette*) are NOT shipped -- they contain third-party GitHub data. The
# published tables are in docs/results.md; regenerate the artifacts locally:
./target/release/leaf-bench report                 # rebuilds docs/results.md from local artifacts

# Re-run from scratch:
./target/release/leaf-bench ground-truth           # capture the pinned world (live gh, free)
./target/release/leaf-bench run --condition full   # one tree condition (frozen)
./target/release/leaf-bench run --live             # live matrix (real GitHub, same-time GT)
./target/release/leaf-bench judge-grade            # AXI-faithful LLM-judge diagnostic
```

Budget is capped and metered per run (`bench/bench.toml`, `leaf-bench budget`).

## Workspace

| crate | what |
| ----- | ---- |
| `leaf-traits` | `Codec` / `Propagation` / `Transport` trait seams + settled impls (TOON codec, …) |
| `leaf-core`   | hybrid tree/branch executor (`route: llm\|sequence`), `Journal`, `Replay` |
| `leaf-mcp`    | MCP stdio client + `leaf-wrap` sandboxed CLI-wrapper server |
| `leaf-llm`    | routing layer (provider auto-selected from `.env`), seeded epsilon-greedy, pricing |
| `leaf-bench`  | benchmark harness + falsification experiments |

## Docs

- [`docs/extending.md`](docs/extending.md) — **how to get more tools**: add a task, a capability, a leaf, or a whole domain tree.
- [`docs/results.md`](docs/results.md) — efficiency, routing accuracy, the live fair comparison, methodology.
- [`docs/SUMMARY.md`](docs/SUMMARY.md) — the narrative + the honest findings.
- [`docs/design.md`](docs/design.md) — runtime architecture, replay semantics, security model.
- [`docs/priors.md`](docs/priors.md) — AXI methodology/numbers, prior-art positioning.
- [`docs/prompt-changes.md`](docs/prompt-changes.md) — how the hybrid tree superseded the prompt-only fixes.

## No mistakes

Extensions ship through the **drt-no-mistakes** gate — an agent skill (format:
[kunchenguid/axi](https://github.com/kunchenguid/axi); protocol:
[kunchenguid/no-mistakes](https://github.com/kunchenguid/no-mistakes)) that holds an
authoring agent to the same discipline as the runtime: intent first, ten static
invariant checks, then the `leaf-bench` pipeline in fixed order. Two ways to get it:

- **In-repo (works now):** point your agent's skills directory at `.agents/skills/`
  (e.g. symlink `.claude/skills` to it for Claude Code) and invoke `/drt-no-mistakes`.
  Canonical file:
  [`.agents/skills/drt-no-mistakes/SKILL.md`](.agents/skills/drt-no-mistakes/SKILL.md).
- **Installable (once this repo is published):**
  `npx skills add <owner>/<repo> --skill drt-no-mistakes`.

`scripts/skill-check.sh` keeps the skill's invariant headings byte-identical to
[`docs/extending.md`](docs/extending.md)'s checklist.

## License

MIT
