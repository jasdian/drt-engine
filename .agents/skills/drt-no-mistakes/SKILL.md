---
name: drt-no-mistakes
description: >
  Validate DRT extensions through the deterministic gate — static invariant checks
  plus the fixed leaf-bench pipeline (build, preflight, ground-truth, live run,
  report, replay, budget) — before they run or ship. Use when adding or modifying
  anything under bench/tasks/, bench/*-tree.json, or crates/leaf-bench/src/leaves.rs,
  when the user asks to gate, validate, or ship a DRT extension, or invokes
  /drt-no-mistakes.
user-invocable: true
---

# DRT no-mistakes

`drt-no-mistakes` is the gate that keeps every extension inside the DRT contract —
the LLM picks *which*, never *how* — by holding the author to the same discipline
the runtime holds the model to. You drive it through the existing `leaf-bench`
command family (this repo's non-interactive interface) — including
`leaf-bench lint`, which mechanizes the ten static invariant checks below — in a
fixed order, before anything is spent or shipped.

## Before you start

- Read [docs/extending.md](../../../docs/extending.md) — the four granularities
  (task / capability / leaf / domain tree), the field-by-field schemas, and the
  worked `repo_topics` example you should pattern-match against.
- Read the Decisions log in [docs/design.md](../../../docs/design.md) — the ten
  invariants below are distilled from it; the log has the *why*.
- Build once: `cargo build --release`. Every `leaf-bench` command below means
  `./target/release/leaf-bench`.
- A live step needs the environment `leaf-bench preflight` checks: an API key in
  `.env`, `gh` authenticated, `jq`/`gh`/`curl` on PATH.

## Two ways to invoke

`/drt-no-mistakes` works in two modes, depending on whether the user hands you a
task along with the command:

- **Validate-only** — bare `/drt-no-mistakes`. The extension is already authored:
  diff `bench/` and `crates/leaf-bench/src/leaves.rs` against HEAD to see what is
  being gated, then run the gate pipeline below and report the outcome.
- **Task-first** — `/drt-no-mistakes <task>`, e.g.
  `/drt-no-mistakes add a capability that lists a repo's topics`. Carry out the
  extension yourself, then gate it:
  1. **Check scope.** Inspect `git status` before you change anything. Preserve
     unrelated pre-existing changes; commit only what belongs to the task.
  2. **Write the intent down first** (next section), *then* author the files per
     docs/extending.md §1–§4.
  3. **Then validate** through the full gate pipeline, in order.

Everything below applies the same way once the extension exists in the working
tree.

## Intent is required

Before touching any file, write down **what the user set out to accomplish** — the
goal behind the extension, in their own words. This is not a description of the
files you are about to change; it is the acceptance criteria.

In DRT this rule is load-bearing, not ceremony — it is the mechanism behind
invariant 9, and three task fields derive from the intent text and from nothing
else:

- `expected_capability` — which capability *should* answer this task, decided from
  the intent. Never decided by running the tree and copying its pick.
- `reference_route` — the genuine decisions that intent implies: the root pick,
  plus one index per hybrid decision (e.g. `issue_triage` is `[7, 1]`).
- `grading_hint` — what a correct answer contains, written before you have seen
  any output.

Anything inferred from a run afterwards is an untrusted hint; the written intent is
the authoritative acceptance criteria. Err on the side of completeness, not
brevity: capture the decisions and tradeoffs the user ruled in or out, and anything
they explicitly asked for that would otherwise look surprising in the diff.

## The gate pipeline

Fixed and non-configurable. Run the steps in this order. Never reorder, never skip
a step, never proceed past a red one.

| # | step | command | green means |
|---|---|---|---|
| 1 | intent | written down (section above) | the three labels derive from the user's words |
| 2 | invariants | `leaf-bench lint` (the mechanized §1–§10 checks; `--strict` promotes warnings) | exit 0 — zero `error` findings, zero spend |
| 3 | build | `cargo build --release` | compiles |
| 4 | preflight | `leaf-bench preflight` | key resolves, tasks parse, binaries on PATH, `gh` authed, model reachable |
| 5 | ground-truth | `leaf-bench ground-truth` | scripted router replays `reference_route` (zero LLM calls), snapshots GT to `bench/ground-truth/<id>.json` |
| 6 | live run | `leaf-bench run --task <id> --live` | outcome `Done`; forced pipeline = **1 billed call**, +1 per hybrid decision (~$0.0003/task) |
| 7 | report | `leaf-bench report` | `routing correct` against `expected_capability`; checker pass |
| 8 | replay | `leaf-bench run --task <id>` (no `--live`) | frozen replay, no divergence — the late honesty check on invariant 4 |
| 9 | budget | `leaf-bench budget` | within the `bench/bench.toml` caps |

Step 5 before step 6 is not a preference: `run --live` refuses to start without a
frozen ground truth for every task (`crates/leaf-bench/src/main.rs:257-264`).

## Findings protocol

Every red step yields findings. Classify each with severity
`error | warning | info` and an action, then act **by action, not severity**:

- **no-op** — informational; note it in your report and continue.
- **auto-fix** — mechanical and low-risk (a typo'd leaf id, a missing `done` node,
  a float literal, a wrong `tree_file` path): fix it yourself, at most **3
  attempts per step**, committing each fix as
  `drt-no-mistakes(<step>): <summary>`. If three attempts don't green the step,
  escalate.
- **ask-user** — the finding touches the user's intent; **0 auto-fix attempts,
  always escalate**, relaying the finding verbatim — not paraphrased, not
  pre-judged. Intent-touching fields: `prompt`, `expected_capability`,
  `reference_route`, `grading_hint`, any ground-truth snapshot, and every cap in
  `bench/bench.toml`.

After any fix, restart the gate from step 2 (invariants). A fresh pass
re-validates the current state, so already-resolved findings do not re-surface.

## 1. LLM routes; code does everything else

The router's entire output is a candidate pick (`cN`); leaf arguments come only
from `ArgsTemplate` substitution over `{input.*}`, `{last_output}`, `{source}`,
`{seed}`. No generated text ever reaches argv.

- Enforced structurally: the `Router` trait returns an index; the executor builds
  argv from the tree's templates only.
- Rule: never author a template slot whose value would come from model output, and
  never propose runtime changes that pipe router text anywhere but the decision.

## 2. Float-free

No float may appear in a template or the journal — money is micro-USD `u64`,
epsilon is ppm `u32`. Floats break byte-stable replay.

- Enforced at **run start**: `Tree::validate` →
  `ArgsTemplate::validate_no_floats` (`crates/leaf-core/src/tree.rs:145-159`),
  called before the journal opens (`crates/leaf-core/src/executor.rs:88-90`).
  Task *parse* does not validate the tree, so catch it at author time:

```sh
grep -nE ':\s*-?[0-9]+\.[0-9]' bench/general-tree.json   # heuristic; expect no output
```

- A hit is an `error` finding (action: auto-fix — rewrite as integer units).
- Mechanized: `leaf-bench lint` check `floats`.

## 3. Argv arrays, never shell strings

Leaf args are argv arrays handed to the binary with no shell in between —
*shell* injection is structurally impossible. That does **not** stop *option*
injection (`crates/leaf-mcp/src/sandbox.rs:9-16` says so explicitly): a value
starting with `-` still parses as a flag. The defense is `--` end-of-options.

- Precise rule: any template slot that **task-derived data flows into**
  (`{input.*}`, or `{last_output}`/`{source}` reaching a non-jq binary) must sit
  after a literal `--`, as `gh_api` and `curl` do
  (`crates/leaf-bench/src/leaves.rs:111,142`).
- A tree-authored program slot (jq's `{expr}`, sha256sum's `{path}`) is trusted
  under §1 — the author wrote it; the model cannot.
- Check: `grep -n 'args_template' crates/leaf-bench/src/leaves.rs` and review
  every template a task value reaches; missing `--` before such a slot is an
  `error` finding (ask-user if fixing it would change frozen cassettes).
- Mechanized: `leaf-bench lint` check `dash-dash` (the stdin, prefix-embedding,
  and trusted-slot exemptions are built in).

## 4. Declare `DeterminismClass` honestly

`Deterministic` = output is a pure function of args/stdin; replay **re-executes
and byte-asserts**. `ReplayOnly` = network or otherwise non-reproducible; replay
serves the journal with zero live calls.

- The connect-time cross-check (`crates/leaf-mcp/src/client.rs:83-96`,
  `DeterminismMismatch`) is consistency-only: in this harness both sides read the
  same `LeafDef.determinism` field, so a mislabel *passes connect*. The real
  catch is gate step 8 — a network tool mislabeled `Deterministic` diverges at
  frozen replay.
- Rule: network-, time-, or token-dependent ⇒ `ReplayOnly`, no exceptions. A red
  step-8 caused by a mislabel is fixed by relabeling to `ReplayOnly` and
  re-recording — never by fudging the journal.
- Static slice mechanized as `leaf-bench lint` check `determinism-class`
  (binary-name heuristic); the real catch remains gate step 8.

## 5. Reduces read `{source}`

`{source}` is the most recent **raw, non-jq** output; `{last_output}` is whatever
ran last. A reduce that reads `{last_output}` feeds on its own output if it is
ever re-picked; reading `{source}` makes it idempotent.

```sh
jq -e '[.. | objects | select(.node? == "leaf" and .args.input? != null)
        | select(.args.input != "{source}")] | length == 0' bench/general-tree.json
```

- Expect `true`. Any other `input` value is an `error` finding (auto-fix:
  change it to `{source}`). Run the same check against any new `tree_file`.
- Mechanized: `leaf-bench lint` check `source-reduce`.

## 6. The last leaf's output IS the answer surface

Nothing is appended after the terminal reduce — what it emits is what gets
graded. It must emit the gradeable scalars, and booleans must be mapped to
strings (`if .merged then "yes" else "no" end`): a boolean in ground truth
**hard-fails the grade** as a task-authoring error
(`crates/leaf-bench/src/checker.rs:149-160`) — but only at grade time. Catch it
at author time, right after gate step 5:

```sh
jq -e '[.. | booleans] | length == 0' bench/ground-truth/<id>.json
```

- Expect `true`. A boolean is an `error` finding (auto-fix: rewrite the terminal
  jq expr to emit strings, then re-run ground-truth).
- Mechanized: `leaf-bench lint` checks `gt-bool`, `gt-volatile` (volatile
  timestamp keys), `gt-joined-list` (a comma-joined list string is a
  format/order trap for cross-arm grading — emit per-item scalars),
  `gt-error-surface` (a raw tool error as ground truth grades only the tree —
  use a regex checker), plus `gt-coverage` for a missing snapshot.

## 7. Every capability sequence ends in `done` with `terminal: true`

Otherwise the executor runs past the end and pops defensively
(`crates/leaf-core/src/executor.rs:145-150`) — the run silently returns to root
and wastes a billed routing call. A hybrid sequence may instead end in a nested
`branch`, whose arms must themselves satisfy this rule.

```sh
jq -e '[.. | objects | select(.node? == "branch" and .route? == "sequence")
        | .candidates[-1]
        | select(((.node == "done" and .terminal == true) or .node == "branch") | not)]
       | length == 0' bench/general-tree.json
```

- Expect `true`. A violation is an `error` finding (auto-fix: append
  `{ "node": "done", "name": "finish", "description": "final answer produced", "terminal": true }`).
- Mechanized: `leaf-bench lint` check `terminal-done`.

## 8. Capability goals are selection hints

The router chooses among the sibling `goal` lines — they are the routing surface.
Phrase each as **"Use when the task asks about X"** and keep siblings
non-overlapping (see docs/prompt-changes.md for why this carries the routing
accuracy).

```sh
jq -e '(.root.candidates | map(select(.node == "branch") | .goal)) as $g
       | ($g | length) == ($g | unique | length)' bench/general-tree.json
```

- Expect `true` (no duplicate goals; an empty goal also fails review).
- The decidable slice ends there — semantic overlap is a judgement: read every
  sibling goal against the new task's prompt; **exactly one** should claim it.
  Two plausible claimants is an `error` finding (ask-user if disambiguating
  changes what the capability means).
- Mechanized (decidable slice): `leaf-bench lint` check `goal-dupes`; the
  semantic-overlap judgement stays yours.

## 9. Author `expected_capability` independently

The routing-accuracy headline is only honest if the label is independent of the
system being scored. The label comes from the written intent (see "Intent is
required") — never from the tree's own pick.

```sh
grep -L 'expected_capability' bench/tasks/*.toml   # expect no output
```

- A task without the label is silently **skipped** by routing-accuracy scoring
  (`crates/leaf-bench/src/report.rs:549`). This gate treats a missing label as an
  `error` finding, not a skip — and the label itself is intent-touching
  (ask-user to change, never auto-fix).
- Mechanized: `leaf-bench lint` checks `routing-label` (present, in range,
  targets a branch) and `route-shape` (the label agrees with
  `reference_route[0]`, the route replays through the tree consuming one entry
  per llm decision within the executor's step budget, fits `max_llm_calls`,
  and `task.leaves` covers every leaf id the tree references).

## 10. Keep leaves read-only

A mutating leaf replays fine (`ReplayOnly`), but re-recording ground truth would
mutate the world again — the answer surface stops being reproducible from the
world it is pinned against (benchmark scope).

```sh
grep -nE '"-X"|--method|--data|"mutation' crates/leaf-bench/src/leaves.rs bench/*.json
# expect no output
```

- Review rule for any new `LeafDef` or endpoint: no POST/PUT/DELETE, no gh
  `-X`/`--method` beyond GET, no curl `-d`/`--data*`/`--upload-file`, no GraphQL
  `mutation`. A hit is an `error` finding (ask-user — a mutating leaf is a scope
  change, not a fix).
- Mechanized: `leaf-bench lint` check `read-only` (mutating argv flags in leaf
  defs; `mutation` queries in graphql slots).

## Hard rules

- Never hand-edit journals, cassettes, or snapshots under `bench/artifacts/` or
  `bench/ground-truth/` to make a step green.
- Never copy `expected_capability` or `reference_route` from a run's pick (§9).
- Never mix stale `docs/results.md` numbers with a changed task matrix — re-run
  the matrix and `leaf-bench report`, or don't add the task.
- Never raise a `bench/bench.toml` cap mid-gate to un-block a run —
  `BudgetExceeded` is an ask-user finding.
- Never relabel a network leaf `Deterministic` to make replay byte-assert pass
  (§4), and never the reverse to hide a divergence.
- While the gate is red: fix forward through the pipeline; never commit around
  it, never report success past a red step.

## Encode each mistake class into the gate

There is no mistake ledger and no memory file. When a mistake escapes the gate, do
not write a note — make its class deterministic, then it is caught on every future
pass:

- a new `leaf-bench lint` check (plus its seeded-violation test) in
  `crates/leaf-bench/src/lint.rs` — the first choice for anything statically
  decidable,
- a `[checker]` `regex` `forbid` pattern for the wrong-output class,
- a new bench task that exercises the class,
- a tightened leaf/step `description` or capability `goal`,
- or a new checklist line in **both** `docs/extending.md` and this skill — then
  run `scripts/skill-check.sh` (the drift gate that keeps the ten §-headings above
  byte-identical to the checklist's bold phrases).

Precedents: upstream no-mistakes baked `go test -race ./...` into its gate config
after its #1311 concurrency-bug class; this repo re-sorted merged PRs by
`mergedAt` after the `UPDATED_AT` ground-truth bug, and made a boolean in ground
truth a hard grade-time failure instead of a silent skip.

## Provenance

Format follows [kunchenguid/axi](https://github.com/kunchenguid/axi)'s skill
(`.agents/skills/` canonical, optional per-agent skills symlink, numbered principle
sections); protocol follows
[kunchenguid/no-mistakes](https://github.com/kunchenguid/no-mistakes)'s skill (two
invocation modes, mandatory intent, findings actions `no-op`/`auto-fix`/`ask-user`
with auto-fix caps, hard anti-bypass rules). Deliberately not ported: the push
proxy, daemon, TUI, AI review step, and PR/CI automation — here *you* drive the
pipeline and `leaf-bench` is the non-interactive interface. The static pass is
mechanized: `leaf-bench lint` (`crates/leaf-bench/src/lint.rs`) runs the §-checks
as code with a seeded-violation test per check, and
`leaf-bench lint --install-hook` wires it into `.git/hooks/pre-push` — so the
static pass no longer depends on agent discipline.
