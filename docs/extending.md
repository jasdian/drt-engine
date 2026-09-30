# Extending — how to get more tools

DRT separates what the LLM does (route at genuine decision points) from what code does
(everything else). So "adding a tool" happens at four granularities, cheapest first:

| granularity | what it is | where | code change? |
|---|---|---|---|
| **task** | a new benchmark instance over existing capabilities | `bench/tasks/<id>.toml` | no |
| **capability** | a new deterministic (or hybrid) pipeline in a tree | `bench/general-tree.json` | no |
| **leaf** | a new wrapped CLI binary | `crates/leaf-bench/src/leaves.rs` | one struct |
| **domain tree** | a new domain (docker, k8s, chrome…) — the analog of `gh-axi` vs `chrome-devtools-axi` | new tree JSON + leaves + tasks | per the rows above |

A complete worked example (`repo_topics`, zero Rust changes) is at the bottom.

## 1. Add a task

A task is one TOML file in `bench/tasks/`. Every `.toml` there is auto-discovered
(`TaskManifest::load_all`, `crates/leaf-bench/src/tasks.rs`) and its leaf ids are
auto-collected — there is no registration list to edit. The schema is the `RawTask`
struct at the top of `tasks.rs`; copy `bench/tasks/view_pr.toml` (forced pipeline) or
`bench/tasks/issue_triage.toml` (hybrid) as a template.

The fields that matter:

- `prompt` — natural language; the router routes on it. It reaches the tree as terse
  `prompt: …` + `key: value` lines (never JSON), combined with `[input]`.
- `[input]` — structured entity params (`owner`, `name`, `number`, `term`, `tag`, …).
  These parameterize the shared tree via `{input.<field>}` templating, so one general
  tree serves every task instance.
- `expected_capability` — the index of the correct root capability. This is the
  **independent routing label** that routing accuracy is scored against. Author it from
  the task's intent, *never* by running the tree and copying what it picked.
- `reference_route` — the sequence of LLM-decision indices used to capture ground truth
  with a scripted router (zero LLM calls). A forced pipeline needs only the root pick,
  e.g. `[0]` — the sequence walks itself. A hybrid task lists each genuine decision:
  `issue_triage` uses `[7, 1]` (root picks capability 7, classify picks arm 1).
- `[checker]` — `contains_values` (every string/number scalar in the ground-truth JSON
  must appear in the answer; a boolean in the ground truth hard-fails the grade as an
  authoring error, so emit strings instead — see the invariants) or `regex` with
  `must`/`forbid` lists.
- `grading_hint` — an AXI-style loose "known facts" band for the LLM-judge diagnostic
  (`leaf-bench judge-grade`).
- `leaves`, `max_llm_calls` (caps *billed* router calls only), `timeout_secs`,
  `tree_file` (path relative to the task file) or inline `tree_json`.

## 2. Add a capability (a tree branch)

A capability is one branch appended to `root.candidates` in `bench/general-tree.json`
— insert it **before the trailing `done` node**, so existing capability indices (and
every committed task's `expected_capability`) stay stable. Its position is the index new
tasks will reference. The schema is the `Tree`/`Branch`/`Node` structs in
`crates/leaf-core/src/tree.rs`.

The forced-pipeline shape (routes in **1 LLM call** — the root pick; the executor walks
the rest with zero router calls):

```json
{
  "node": "branch",
  "name": "my_capability",
  "goal": "Use when the task asks about X…",
  "route": "sequence",
  "candidates": [
    { "node": "leaf", "name": "fetch_x", "description": "…", "leaf": "gh_api", "tool": "api",
      "args": { "endpoint": "repos/{input.owner}/{input.name}/…" } },
    { "node": "leaf", "name": "reduce_x", "description": "…", "leaf": "jq", "tool": "jq",
      "args": { "expr": "{…gradeable scalars…}", "input": "{source}" } },
    { "node": "done", "name": "finish", "description": "final answer produced", "terminal": true }
  ]
}
```

Conventions that carry the 100% routing accuracy (see `docs/prompt-changes.md`):

- Phrase `goal` as **"Use when the task asks about X"** — a selection hint. Make it
  non-overlapping with sibling capabilities; the router chooses among these lines.
- Where a step must not be skipped, say so in its `description` ("REQUIRED first step",
  "REQUIRED reduce step — the operation is NOT done until this runs"). On a forced
  sequence this is belt-and-braces (the executor enforces the order); inside an `llm`
  branch it is load-bearing.

For a **hybrid** capability — one that needs a genuine semantic decision about fetched
data — nest a `route: "llm"` branch between fetch and reduce, with one sequence arm per
outcome. The reference is `issue_triage` → `classify_issue` in `general-tree.json`:
fetch the issue, LLM classifies bug vs feature from title+body, then a deterministic
per-arm reduce. Each genuine decision costs one more LLM call (issue_triage tasks run at
2 instead of 1).

Args templating (deterministic string substitution — the LLM never authors arguments):
`{input}` (the whole terse input), `{input.<field>}`, `{last_output}` (previous leaf's
text), `{source}` (most recent **raw, non-jq** output — use it as every reduce's input so
a re-picked reduce stays idempotent), `{seed}`. Templates must be float-free
(`validate_no_floats` rejects the tree otherwise).

## 3. Add a leaf (a new wrapped CLI)

Only needed when a capability requires a binary beyond the current six (`jq`, `jq_raw`,
`gh_api`, `gh_graphql`, `curl`, `sha256sum`). Append one `LeafDef` to `defs()` in
`crates/leaf-bench/src/leaves.rs` — that is the entire change. At setup the registry
resolves the binary from PATH, writes a manifest to `bench/artifacts/manifests/<id>.json`
(mode 0600 — it may embed a token), and serves it via `leaf-wrap`, a real MCP stdio
server any stock MCP client can talk to, sandboxed (binary allowlist, scrubbed env,
working-dir jail, timeout).

```rust
LeafDef {
    id: "my_tool",              // the id trees reference via "leaf"
    tool_name: "run",           // the MCP tool name trees reference via "tool"
    description: "…",
    binary: "mytool",           // resolved from PATH at setup
    args_template: vec!["--flag", "--", "{arg}"],  // argv array; {slots} filled from the tool-call args
    stdin_from: None,           // or Some("input") to pipe that arg to stdin
    determinism: ReplayOnly,    // see below
    properties: serde_json::json!({ "arg": {"type": "string"} }),
    required: vec!["arg"],
    needs_gh_env: false,        // true = inject `gh auth token` env
    timeout_ms: 30_000,
}
```

Pick the `DeterminismClass` honestly — it is the replay contract, and the client
cross-checks the server's declaration at connect (`DeterminismMismatch` if it lies):

- `Deterministic` — output is a pure function of args/stdin (`jq`, `sha256sum`). Replay
  **re-executes** the leaf and byte-asserts the output.
- `ReplayOnly` — network or otherwise non-reproducible (`gh`, `curl`). Replay serves the
  journaled output with **zero** live calls.

Put `--` (end-of-options) before any template slot an input string flows into, as
`gh_api` and `curl` do — an untrusted value must never be parseable as a flag. Args are
argv arrays, never shell strings.

## 4. Add a domain tree

The GitHub tree (`bench/general-tree.json`, `"name": "github-agent"`) is one domain. A
new domain — the analog of AXI shipping `gh-axi` *and* `chrome-devtools-axi` — is:

1. A new tree JSON next to it (e.g. `bench/docker-tree.json`, `"name": "docker-agent"`):
   root `route: "llm"`, one branch per capability as in §2.
2. Any new leaves for the domain's CLI (§3) — keep them **read-only**: a mutating leaf
   replays fine (`ReplayOnly`) but re-recording ground truth would mutate again, and the
   run's answer surface stops being reproducible from the world.
3. Tasks pointing at it via `tree_file = "../docker-tree.json"` (§1).

Nothing else changes: executor, journal, replay, budget, and report are tree-agnostic.

## Worked example: `repo_topics`, end to end

A new capability using only existing leaves — zero Rust changes. (This example is
**committed as the benchmark's task #10**, added end-to-end through the
drt-no-mistakes gate; the matrix was re-run and the report regenerated at the same
time, because adding a task changes the shared tree and `config_hash` pins every
run to the tree digest — old numbers and a new matrix cannot silently mix. Bonus
lesson: the first draft of this example declared only `leaves = ["gh_api", "jq"]`;
`leaf-bench lint` rejected it because the shared tree also references `gh_graphql`
— a misroute could hit an unregistered leaf. The committed task declares all
three.)

**Step 1 — the branch.** Append to `root.candidates` in `bench/general-tree.json`,
before the trailing `done` (it becomes index 8):

```json
{
  "node": "branch",
  "name": "repo_topics",
  "goal": "Use when the task asks which topics a repository is tagged with (the topic list and how many).",
  "route": "sequence",
  "candidates": [
    {
      "node": "leaf",
      "name": "fetch_repo",
      "description": "REQUIRED first step: fetch the repository JSON from the REST API",
      "leaf": "gh_api",
      "tool": "api",
      "args": { "endpoint": "repos/{input.owner}/{input.name}" }
    },
    {
      "node": "leaf",
      "name": "extract_topics",
      "description": "REQUIRED reduce step — the operation is NOT done until this runs: reduce to topic count and names",
      "leaf": "jq",
      "tool": "jq",
      "args": {
        "expr": ".topics as $t | reduce range(0; $t | length) as $i ({topic_count: ($t | length)}; . + {(\"topic\\($i+1)\"): $t[$i]})",
        "input": "{source}"
      }
    },
    { "node": "done", "name": "finish", "description": "final answer produced", "terminal": true }
  ]
}
```

**Step 2 — the task.** `bench/tasks/repo_topics.toml`:

```toml
id = "repo_topics"
category = "single_step"
prompt = '''List the topics the openclaw/openclaw repository is tagged with, and how many there are.'''
max_llm_calls = 12
timeout_secs = 300
leaves = ["gh_api", "gh_graphql", "jq"]   # must cover every leaf id the SHARED tree references
tree_file = "../general-tree.json"
reference_route = [8]
expected_capability = 8
grading_hint = '''The repository's topic names and the count of topics, as GitHub reports them at run time.'''

[input]
owner = "openclaw"
name = "openclaw"

[checker]
type = "contains_values"
```

**Step 3 — run it.**

```sh
cargo build --release
./target/release/leaf-bench preflight
./target/release/leaf-bench ground-truth                  # scripted router replays reference_route (no LLM), snapshots GT
./target/release/leaf-bench run --task repo_topics --live # a new task has no cassette yet, so its first run is live
./target/release/leaf-bench report
```

Expected: the router picks `c8` in **1 LLM call**, the sequence walks
fetch → reduce → done, the answer is the reduced `{topic_count, topic1, topic2, …}` object
(per-item scalars, not a joined string — `lint`'s `gt-joined-list` warns otherwise), and the
journal under `bench/artifacts/` replays.

## Invariants checklist

Distilled from `docs/design.md` ("Decisions log") and the executor semantics — every
extension must hold these:

1. **LLM routes; code does everything else.** No generated text reaches argv — leaf args
   come only from `ArgsTemplate` substitution.
2. **Float-free.** No float may appear in a template or the journal (money is micro-USD
   `u64`, epsilon is ppm `u32`).
3. **Argv arrays, never shell strings** — and `--` before untrusted values.
4. **Declare `DeterminismClass` honestly** — it is cross-checked at connect.
5. **Reduces read `{source}`**, not `{last_output}`, so a re-picked reduce is idempotent.
6. **The last leaf's output IS the answer surface.** The terminal reduce must emit the
   gradeable scalars; map booleans to strings (`if .merged then "yes" else "no" end`) —
   a boolean in ground truth hard-fails the grade as a task-authoring error.
7. **Every capability sequence ends in `done` with `terminal: true`** — otherwise the run
   pops back to root and wastes a routing call.
8. **Capability goals are selection hints** ("Use when the task asks about X"),
   non-overlapping with siblings.
9. **Author `expected_capability` independently** from the task's intent — never by
   running the tree and copying its pick.
10. **Keep leaves read-only** (benchmark scope): mutations break reproducibility of the
    world the ground truth is pinned against.

## Verify

| after adding… | run |
|---|---|
| any change | `leaf-bench lint` (offline, free, sub-second — the mechanized invariants checklist) |
| a `[checker]` edit | `leaf-bench regrade [--dry-run]` (offline, free — re-grades every stored answer with the current checkers and prints verdict flips; never re-pay for answers to test a checker) |
| a task | `leaf-bench ground-truth` (live `gh`, free) → `leaf-bench run --task <id> --live` (first run has no cassette; ~$0.0003/task at `gpt-5.4-mini` prices) → `leaf-bench report` |
| a capability | the task row above, plus check the journal shows **1** billed call (or 1 + your hybrid decisions) and `routing correct` in the report |
| a leaf | `cargo build --release` → `leaf-bench preflight` (binary on PATH, manifest generated) → the task row above |
| a domain tree | all of the above against the new `tree_file` |

Budget note: every live run is metered and capped by `bench/bench.toml` /
`leaf-bench budget` — a new task joins that accounting automatically.

## No-mistakes gate

The checklist and the Verify table above are also installed as an agent skill —
[`.agents/skills/drt-no-mistakes/SKILL.md`](../.agents/skills/drt-no-mistakes/SKILL.md),
discoverable by agents that read `.agents/skills/` (e.g. Claude Code via a local `.claude/skills` symlink) as `/drt-no-mistakes`. The
skill wraps them in a gate protocol: intent first, the ten invariants as a static
pass, then the verify pipeline in fixed order, with findings classified
no-op / auto-fix / ask-user. Its ten §-headings quote this file's bold invariant
phrases verbatim; [`scripts/skill-check.sh`](../scripts/skill-check.sh) fails if
either side drifts — run it after editing either file.

The static slice is mechanized: `leaf-bench lint` checks invariants 2–10 over
`bench/tasks/`, every referenced `tree_file`, `bench/ground-truth/`, and the leaf
defs — run it before anything in the Verify table, or install it as a pre-push
hook with `leaf-bench lint --install-hook`.
