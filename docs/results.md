# Results: DRT — a routing-only LLM over a hybrid deterministic tree

Pinned model: `gpt-5.4-mini`. N = 5 repeats × 10 tasks. The LLM makes only ROUTING decisions over one general branching tree; deterministic leaves (MCP-wrapped `gh`/`jq`) do every fetch and reduce. All numbers regenerate from `bench/artifacts/` by `leaf-bench report` — third-party reproducible.

## Efficiency: the hybrid tree spends the LLM only on genuine choices

The tree is HYBRID: the root is an LLM branch (pick the capability — the one real choice), and each capability is a DETERMINISTIC SEQUENCE the executor walks with zero router calls (`route: sequence`, terminal `done`). So a forced pipeline (fetch→reduce→done) costs **1 LLM call**; the one hybrid capability (`issue_triage`) makes a genuine second decision — classify the fetched issue — for 2. Mean across tasks: **1.20 LLM calls/task** (an earlier all-LLM-routed tree spent 8–12 per task rubber-stamping forced steps).

| task | LLM-calls/task |
|---|---|
| bug_triage_search | 1 |
| issue_triage | 2 |
| issue_triage_ambiguous | 2 |
| list_open_issues | 1 |
| merged_pr_ci_audit | 1 |
| nonexistent_repo | 1 |
| pr_then_checks | 1 |
| release_then_body | 1 |
| repo_topics | 1 |
| view_pr | 1 |
| **mean** | **1.20** |

## End-to-end (frozen tree, fixed world)

Grading: `contains_values` (every ground-truth value is a substring of the answer) against the tree's jq output, in a byte-identical replayed world. Cost is a snapshot at pinned prices. Durable metrics: routing accuracy (below) and LLM-calls/task (above). N = 5 × 10 tasks.

| condition | success (Wilson 95%) | LLM-calls/task | in:out tokens (ratio) | cost/completed-task | cost/task mean±sd |
|---|---|---|---|---|---|
| last_k | 100% (93–100%, n=50) | 1.20 | 842 in / 6 out (140:1) | $0.0003 | $0.0003 ± 0.0001 |
| decayed | 90% (79–96%, n=50) | 1.20 | 774 in / 6 out (129:1) | $0.0003 | $0.0002 ± 0.0001 |
| full | 100% (93–100%, n=50) | 1.20 | 842 in / 6 out (140:1) | $0.0003 | $0.0003 ± 0.0001 |

### Per-task (pass/N)

| task | last_k | decayed | full |
|---|---|---|---|
| bug_triage_search (investigation) | 5/5 | 5/5 | 5/5 |
| issue_triage (hybrid) | 5/5 | 5/5 | 5/5 |
| issue_triage_ambiguous (hybrid) | 5/5 | 0/5 | 5/5 |
| list_open_issues (single_step) | 5/5 | 5/5 | 5/5 |
| merged_pr_ci_audit (investigation) | 5/5 | 5/5 | 5/5 |
| nonexistent_repo (error_recovery) | 5/5 | 5/5 | 5/5 |
| pr_then_checks (multi_step) | 5/5 | 5/5 | 5/5 |
| release_then_body (multi_step) | 5/5 | 5/5 | 5/5 |
| repo_topics (single_step) | 5/5 | 5/5 | 5/5 |
| view_pr (single_step) | 5/5 | 5/5 | 5/5 |

The three propagation policies (`last_k`/`decayed`/`full`) are IDENTICAL on the forced-pipeline tasks and on the CLEAR hybrid issue — with one genuine decision the router sees only the prompt, and where a second decision is unambiguous the retained context does not change it. Propagation separates the policies in exactly ONE place: the genuinely-ambiguous `issue_triage_ambiguous` classify (see Hybrid classify decision), where `decayed`'s lossy summary drops the body signal that tips the call, flipping it from feature (`last_k`/`full`) to bug (`decayed`) consistently, 5/5. That is why `decayed` scores below the other two here. Honest reading: propagation earns its keep only when a genuine decision depends on body content the lossy policy discards — rare in this GitHub suite, real when it happens.


## Routing accuracy (independent metric)

The three tree conditions navigate ONE general branching tree: the LLM's first decision routes to one of 9 capabilities from the task prompt (terse `key: value` input, terse TOON-rendered context — no JSON); deterministic leaves then run that capability's fetch+reduce with args parameterized from the task's terse `input` fields. This scores whether the LLM picked the correct capability, judged against an independently-authored `expected_capability` label (NOT the tree's own output). It is the LLM's actual job — the pipelines are deterministic.

| condition | routing correct |
|---|---|
| last_k | 50/50 (100%) |
| decayed | 50/50 (100%) |
| full | 50/50 (100%) |
| **all tree** | **150/150 (100%)** |

Per task (pooled across the three conditions):

| task | routing correct |
|---|---|
| bug_triage_search | 15/15 (100%) |
| issue_triage | 15/15 (100%) |
| issue_triage_ambiguous | 15/15 (100%) |
| list_open_issues | 15/15 (100%) |
| merged_pr_ci_audit | 15/15 (100%) |
| nonexistent_repo | 15/15 (100%) |
| pr_then_checks | 15/15 (100%) |
| release_then_body | 15/15 (100%) |
| repo_topics | 15/15 (100%) |
| view_pr | 15/15 (100%) |


## Hybrid classify decision (capture)

The hybrid capability's `classify` step is a GENUINE second LLM decision over the fetched issue's title+body — labels are stripped at the fetch (GraphQL field selection), so it cannot shortcut on a label. Which branch it picked (`f`=feature/other, `b`=bug), per propagation policy:

| task | last_k | decayed | full |
|---|---|---|---|
| issue_triage | 5f / 0b | 5f / 0b | 5f / 0b |
| issue_triage_ambiguous | 5f / 0b | 0f / 5b | 5f / 0b |

The clear feature issue (#104040) classifies feature under every policy. The genuinely-ambiguous issue (#93120, titled "[Bug/Feature]", body asks to make retries configurable) classifies **feature under `full`/`last_k` but BUG under `decayed`, consistently (5/5 vs 5/5)** — `decayed`'s lossy summary drops the body's configurability (feature) signal, leaving the retry-failure (bug) signal. This is a real, deterministic propagation effect on a genuine semantic decision — the one place in this suite where the retained-context policy changes the outcome (and, since the reference route takes the feature branch, why `decayed` scores below `last_k`/`full` on that task under exact grading; the judge, told either call is acceptable, passes all).

## Fair comparison — LIVE, same model: routing tree vs a fused free-driver

Both are `gpt-5.4-mini` against REAL GitHub, graded on a same-time live ground truth. The `control` here is a FUSED free-driver: it drives two tools (`gh_api_reduce`, `gh_graphql_reduce`) that fetch AND apply a jq reduction the model supplies IN ONE call, returning only the reduced result — the raw payload never enters context. That is AXI's `gh-axi`/code-mode reduce-at-the-boundary idea, given to a free-driver, so the ONLY difference from the tree is orchestration (deterministic routing vs LLM free-drive), not tool shape.

| metric | tree (`full`) | fused control |
|---|---|---|
| success (exact-substring) | 50/50 (100%) | 35/50 (70%) |
| success (AXI-faithful judge) | 43/50 (86%) | 42/50 (84%) |
| LLM-calls/task | 1.20 | 2.68 |
| cost/task | $0.0003 | $0.0018 |

(Caveat the exact-substring row: the tree's 100% is tautological — it is graded against its OWN reference-route jq output — and the control's 70% is depressed by strict substring matching against that same pinned output. The AXI-faithful judge row is the fair success comparison; the exact-substring row is not a capability gap.)

Fused control per task (exact-substring vs same-time live GT):

| task | fused control |
|---|---|
| bug_triage_search | 0/5 |
| issue_triage | 5/5 |
| issue_triage_ambiguous | 5/5 |
| list_open_issues | 0/5 |
| merged_pr_ci_audit | 0/5 |
| nonexistent_repo | 5/5 |
| pr_then_checks | 5/5 |
| release_then_body | 5/5 |
| repo_topics | 5/5 |
| view_pr | 5/5 |

The fused control does NOT blow the token limit on `bug_triage_search` (the raw list never enters context — the fetch+count is fused). Two failure classes were eliminated by construction rather than left to interpretation: the task prompts now PIN the ordering/counting semantics (`merged_pr_ci_audit` says sort by `mergedAt`; `pr_then_checks` defines passed/failed as SUCCESS/FAILURE rollup conclusions, skipped/neutral excluded) — before pinning, each free-driver repeat invented its own counting policy and audited a different top-10 slice. That audit also exposed the grading window itself: the control's post-pinning top-10 matched GitHub's true list while the run-level ground truth captured minutes earlier did not — which is why free-driving cells are now graded against a per-cell ground truth captured seconds after each answer. What remains under exact-substring is irreducible at that window: formatting (a thousands separator), reduce-semantics differences, and all-or-nothing multi-assertion surfaces (`merged_pr_ci_audit`) churning within seconds. The drift-tolerant judge lifts the free-driver to a rate statistically tied with the tree (tree 43/50 vs control 42/50 under the judge is a 1-grade difference, not a win; each cell is a majority-of-3 judge vote to damp vendor sampling jitter). The honest reading: with tool shape AND question semantics held equal, the routing tree and the free-driver converge on success; the tree's real advantage is ~1 LLM call/task vs ~3, ~6× lower cost, determinism/replay, and having those semantics pinned in code rather than in prompt prose. It is NOT a raw-vs-reduced trick, and NOT a success gap.

## Claude Code control (AXI's `cli` harness, exactly)

`claude -p` (Sonnet) + generic Bash + live authenticated `gh`, one process per task, against REAL GitHub — this is exactly AXI's `cli` condition (raw `gh` in a shell). It is NOT AXI's `axi` condition, which swaps in the purpose-built `gh-axi` structured tool; the fair analog of AXI's 100% `axi` number is our TREE, not this free-driver. A Sonnet cross-check on the same tasks. Each cell is graded against a PER-CELL ground truth captured seconds after its answer (legacy cells without a `gt.json` fall back to the run-level live GT), so the drift window is seconds, not minutes. Exact-substring vs the AXI-faithful LLM judge (range hints):

| task | exact-substring (live GT) | AXI judge (range hints) |
|---|---|---|
| bug_triage_search | 1/5 (20%) | 3/5 (60%) |
| issue_triage | 4/4 (100%) | 4/4 (100%) |
| issue_triage_ambiguous | 3/4 (75%) | 4/4 (100%) |
| list_open_issues | 4/4 (100%) | 1/4 (25%) |
| merged_pr_ci_audit | 1/4 (25%) | 0/4 (0%) |
| nonexistent_repo | 4/4 (100%) | 4/4 (100%) |
| pr_then_checks | 4/4 (100%) | 4/4 (100%) |
| release_then_body | 3/4 (75%) | 4/4 (100%) |
| repo_topics | 4/4 (100%) | 4/4 (100%) |
| view_pr | 4/4 (100%) | 4/4 (100%) |
| **overall** | **32/41 (78%)** | **32/41 (78%)** |

Read the AXI-judge column: a live agent is competent but exact-substring fails it on **formatting** (a thousands separator, a reworded status) and on **seconds-scale churn** of all-or-nothing multi-assertion surfaces — per-cell grading removed the minutes-scale drift class outright (those cells' "failures" were just the world moving between an old GT capture and the run). This is the credible comparison — our routing runtime (cheap `gpt-5.4-mini`, 100% routing) vs a real Claude Code agent (Sonnet), both live. It is a diagnostic (Anthropic-billed, separate from the OpenAI budget; fewer reps).

## AXI published numbers (positioning)

AXI `bench-github` (Claude Sonnet 4.6, 17 live tasks, `claude -p` + Bash, binary LLM judge with range hints):

| AXI condition | how it accesses data | success | cost/task |
|---|---|---|---|
| axi (gh-axi) | reduces at the tool boundary (count + first-N) | 100% | $0.050 |
| mcp-with-code-mode | reduces in a script | 84% | $0.101 |
| cli (raw gh) | dumps raw JSON into context | 86% | $0.054 |
| mcp-no-toolsearch | dumps near-raw JSON (blew the token limit) | 87% | $0.148 |
| mcp-with-toolsearch | MCP tools | 82% | $0.147 |

The honest pairing: AXI's own winning arm — `gh-axi`, 100% — is a REDUCE-AT-THE-TOOL-BOUNDARY design (a pre-computed count plus a small projection). That is exactly what our tree does (bounded fetch + deterministic jq reduce). Our **fused control** (below) gives a free-driver the SAME reduce-at-boundary primitive — the analog of AXI's `gh-axi`/`code-mode` arms. AXI's `cli`/`mcp` arms dump raw JSON into context and score lower (its `mcp` search result was 449 KB — it exceeded the model's token limit). So 'reduce at the boundary vs dump raw' is a within-AXI axis; our tree and our fused control both sit on the reducing side. Not decimal-comparable across studies — compare direction and magnitude.

## Methodology

- Frozen world: network leaves resolve from one cassette snapshot; runs are fail-closed (a cassette miss errors, no live call), so all tree conditions share a byte-identical world. The `jq` leaf runs live. The LIVE sections run against real GitHub: tree cells replay the same recorded world their invocation ground truth captured (cassette-consistent by construction), while free-driving cells (fused control, Claude Code) are graded against a PER-CELL ground truth captured seconds after each answer (`gt.json` in the cell dir, its capture journaled alongside) — the drift window is seconds, not minutes.
- Grading: routing accuracy (independent `expected_capability` label) is the primary metric — the LLM's actual job. End-to-end uses `contains_values` (substring vs the tree's jq snapshot); the AXI-faithful LLM judge with range hints is a diagnostic (majority-of-3 votes per cell, damping vendor sampling jitter).
- Cost: every `/v1/messages` usage block metered at pinned prices (`input×$3 + cache_write×$3.75 + cache_read×$0.30 + output×$15` per MTok); judge excluded (matches AXI). `decayed` is a deterministic summarizer (no extra LLM calls).
- Replay: the deterministic sequence picks emit no journal decision and are reproduced from the tree on replay; only LLM branch decisions replay from the journal (see `replay_reproduces_deterministic_sequence_picks`).

## Limitations

- 10-task subset; single model (`gpt-5.4-mini`, not Sonnet); self-run. Raw artifacts (journals, usage, grades) were produced under `bench/artifacts/` and are not shipped (they embed third-party GitHub data); regenerate with `leaf-bench`.
- Exact-substring grading against the tree's own jq output: the tree passes it by construction, so end-to-end is not the tree's headline (routing accuracy + LLM-calls/task are). The live fair comparison + AXI-faithful judge remove the tree-defined-GT and drift confounds.
- The hybrid `issue_triage` tasks fetch the issue via GraphQL with explicit field selection (number/title/body/state/author, NO labels), so the `classify` decision is a genuine title+body judgement, not a label lookup. The clear issue (#104040) is graded (feature); the ambiguous one (#93120) is deliberately NOT graded on its bug-vs-feature call (both defensible — see the classify capture) — its reference takes the feature branch, so `decayed` (which classifies it bug) scores 0 there under exact grading while the judge, told either call is acceptable, passes it.

## Budget

Global cap $12.00. Per-bucket spend in `bench/budget.jsonl`; `leaf-bench budget` for the tally.
