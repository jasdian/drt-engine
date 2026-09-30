# DRT — summary

DRT (Deterministic Routing Tree): a deterministic-execution runtime for LLM agents, plus a benchmark comparing it to AXI's
`bench-github` GitHub baseline. The LLM makes only ROUTING decisions; a **hybrid** branching
tree of deterministic "leaf" plugins (MCP-over-stdio, wrapping `gh` and `jq`) owns every
argument and reduction. The encoding the LLM reads is terse — the TOON codec for leaf
outputs and terse `key: value` for the run input — never JSON.

## The hybrid tree — determinism is not only a leaf property

A branch carries a `route` mode. The **root is `llm`** (pick the capability — the one genuine
choice). Each capability is a deterministic **`sequence`** the executor walks with zero router
calls (fetch → reduce → terminal `done`). So the LLM is spent only where there is a real
decision:

- A **forced pipeline** (view a PR, count issues, list open issues, …) costs **1 LLM call** —
  the root routing decision. The forced fetch→reduce→done order is enforced by the executor,
  not coaxed by a prompt.
- The **hybrid capability** (`issue_triage`, exercised by two task instances) is
  `fetch → classify(LLM) → reduce`: it fetches an issue (via GraphQL, no labels), makes a genuine
  second decision (bug vs feature) about the title+body, then runs a deterministic reduce. **2 LLM
  calls.**

Mean: **1.20 LLM calls/task** (an earlier all-LLM-routed tree spent 8–12 per task rubber-stamping
forced steps). Sequence picks emit no journal decision and are reproduced from the tree on replay;
only genuine LLM decisions replay from the journal.

## Results (10 tasks × 5 repeats, `gpt-5.4-mini`)

Task #10 (`repo_topics`) is the extending guide's worked example, added end-to-end through the
drt-no-mistakes gate; adding it changed the shared tree, and `config_hash` pins every run to the
tree digest, so the whole matrix re-ran (old numbers and a new matrix cannot silently mix).

| metric | last_k | decayed | full |
|---|---|---|---|
| routing accuracy (independent) | 50/50 | 50/50 | 50/50 |
| end-to-end (exact-substring, frozen) | 100% | 90% | 100% |
| LLM-calls/task | 1.20 | 1.20 | 1.20 |

- **Routing accuracy = 150/150 (100%)** — the LLM's actual job, scored against an independently
  authored `expected_capability` label (NOT the tree's own output). Holds live too (100%).
- **Two hybrid tasks make a genuine SECOND LLM decision** — `classify` an issue as bug vs feature
  from its title+body. Labels are stripped at the fetch (GraphQL field selection), so it cannot
  shortcut on a label.
- **Propagation earns its keep in exactly one place — and it is real, not noise.** The three
  policies are identical on the 8 forced tasks and on the CLEAR issue (#104040 → feature under all).
  On the **genuinely-ambiguous** issue (#93120, titled "[Bug/Feature]", body asks to make retries
  configurable), the classify goes **feature under `last_k`/`full` but BUG under `decayed`,
  consistently 5/5 vs 5/5** — `decayed`'s lossy summary drops the body's configurability (feature)
  signal, leaving the retry-failure (bug) signal. That is a deterministic propagation effect on a
  genuine semantic decision; it is why `decayed` scores below the others (its call disagrees with the
  feature reference), while the judge — told either call is acceptable — passes all. Honest result:
  propagation matters only when a genuine decision depends on body content the lossy policy discards
  — rare here, real when it happens.
- **~$0.0002/task**, routing-only (large context in, a ~2-token `cN` out).

## The fair comparison (LIVE, same model): tree vs a fused free-driver

Earlier I overstated one thing: I framed the tree beating a raw free-driving control on the
large-result task as "free-driving can't scale." That was wrong — the failure was a **tool-shape**
artifact (the old control drove fine-grained fetch leaves with no way to reduce before the raw
1650-issue payload hit `gpt-5.4-mini`'s 200k-token limit), the same failure AXI's own `mcp` arm
hits (its search result was 449 KB). AXI's *winning* arm — `gh-axi`, 100% — REDUCES at the tool
boundary, exactly like our tree.

So the control is now a **fused free-driver**: it drives `gh_api_reduce` / `gh_graphql_reduce`
tools that fetch AND apply a model-supplied jq reduction in one call — AXI's `gh-axi`/code-mode
reduce-at-the-boundary idea, given to a free-driver. With tool shape held equal, the only
difference from the tree is orchestration.

| metric (live, `gpt-5.4-mini`) | tree | fused free-driver |
|---|---|---|
| exact-substring | 100% (50/50) | 70% (35/50) |
| AXI-faithful judge | 86% (43/50) | 84% (42/50) |
| LLM-calls/task | 1.20 | 2.68 |
| cost/task | $0.0003 | $0.0018 |

Under the fair (drift-tolerant) judge the tree (86%), the fused free-driver (84%), and a real
Claude Code agent (Sonnet + Bash, AXI's `cli` harness — 78%, 32/41, per-cell same-time on the
10-task matrix) are statistically indistinguishable. Each judge cell is a majority-of-3 vote;
11/141 cells split their votes, which is the residual judge noise a few grades of spread sit
inside (exact-substring is deterministic).

That convergence is the finding — and it took holding tool shape equal AND three rounds of
making the grading arm-neutral to get there, every round driven by auditing the failed rows:

- **Grade substance, not tree-ese.** `nonexistent_repo`'s checker became a semantic regex (any
  honest not-found/404 report passes, not just the tree's literal stderr); `repo_topics` emits
  per-topic scalars instead of a joined string; `view_pr` is a regex with a not-merged forbid
  list. The first two mistake classes are now `leaf-bench lint` warnings (`gt-error-surface`,
  `gt-joined-list`).
- **Pin the question's semantics in the prompt.** Before the prompts spelled out
  ordering/counting rules (`merged_pr_ci_audit` sorts by `mergedAt`; `pr_then_checks` defines
  passed/failed as SUCCESS/FAILURE rollup conclusions), each free-driver repeat invented its
  own counting policy (23/2, 23/7, 33/5 on the same pinned PR). The tree never needed the
  prose — its reduces ARE the semantics.
- **Grade against a same-time world.** Free-driving cells (fused control, Claude Code) are
  graded against a **per-cell ground truth** captured seconds after each answer; per-cell
  grading lifted cc's exact-substring from 50% to 78% (`list_open_issues` 0/5 → 4/4: those
  "failures" were the world moving between a minutes-old GT capture and the run).

What still fails exact-substring after all three is irreducible at a seconds-wide grading
window — thousands-separator formatting, reduce-semantics differences, and the all-or-nothing
20-assertion surface of `merged_pr_ci_audit` churning within seconds — and it is precisely
what the judge column absorbs. **The routing tree's advantage is ~2× fewer LLM calls, ~6×
lower cost, and determinism/replay — not a success gap, and not a raw-vs-reduced trick.**
(The fused control no longer blows the token limit on the large-result task; tool shape is
equal by construction.)

## Ground-truth corrections (found by auditing)

- `merged_pr_ci_audit`: the tree ordered merged PRs by `UPDATED_AT` (GraphQL has no
  merge-time sort), returning the wrong set of 10. Fixed to over-fetch and sort by `mergedAt`.
- `pr_then_checks` (32 passed) and `bug_triage_search` (1650 total, all states) confirmed
  correct against GitHub semantics.

## Phase-1 experiments

- **[01-encoding](experiments/01-encoding.md)**: TOON-terse encoding uses 14–27% fewer tokens
  than compact JSON at equal extraction accuracy — the reason the LLM-facing encoding is
  terse, not JSON.
- **[02-determinism](experiments/02-determinism.md)** / **[03-ceiling](experiments/03-decomposition-ceiling.md)**.

## Caveats

- 10-task subset, single model (`gpt-5.4-mini`), self-run. Raw artifacts were produced under
  `bench/artifacts/`; `leaf-bench report` regenerates the tables.
- End-to-end grading is exact-substring against the tree's own jq output, so it is not the
  tree's headline; routing accuracy (independent label) and LLM-calls/task are. The live fair
  comparison + AXI-faithful judge remove the tree-defined-GT and drift confounds.

## Files

- **[results.md](results.md)** — efficiency, routing accuracy, the live fair comparison, methodology.
- **[design.md](design.md)** — runtime architecture, replay semantics, security model.
- **[priors.md](priors.md)** — AXI methodology and numbers, prior-art positioning.
- **[prompt-changes.md](prompt-changes.md)** — the prompt rules and why the hybrid tree (structural)
  supersedes the load-bearing one (reduce-before-finish) for forced pipelines.
