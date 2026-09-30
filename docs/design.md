# Design — leaf runtime architecture

The pattern this runtime implements is named **DRT — Deterministic Routing Tree** (definition
in the [README](../README.md)). Ground truth is the code; this document describes the runtime
architecture. All five crates are implemented. Context and pre-registered predictions live in `docs/priors.md` — not repeated here.

## Overview

The leaf runtime is a deterministic-execution runtime for LLM-agent tool subtrees: a run is
**a seed, a tree, an input, and a journal — nothing else** (`leaf-core/src/lib.rs:3-6`). The LLM
is used *only* to route among tree candidates; every mechanical decision — argument construction,
seed derivation, history rendering, sandboxing — is deterministic code. Every run appends to an
append-only JSONL journal that can be replayed: recorded LLM decisions are served back, local
deterministic leaves are genuinely re-executed and byte-compared, and network leaves are served
from the journal with zero I/O. Leaves are real MCP stdio servers (any stock MCP client can drive
them), carrying an explicit per-tool determinism contract. Single binary weight: no server, no
database.

```
leaf-traits   serde/serde_json/thiserror only — vocabulary + swappable seams:
              DeterminismClass, HistoryItem, LeafOutput | Codec | Propagation | Transport
   ▲      ▲
leaf-core     + tokio, sha2 — Clock, sha256 digest, Journal, Tree/ArgsTemplate/derive_seed,
   ▲          Router trait (+ScriptedRouter), Executor run loop, Replay comparator
   │  ▲   ▲
   │  │  leaf-mcp   + rmcp — StdioMcpTransport (client), WrapManifest, sandbox,
   │  │             WrapServer + `leaf-wrap` bin (server). dev-deps leaf-core (tests only)
   │  │   ▲
leaf-llm      + reqwest(rustls), rand_chacha/rand_core, tiktoken — provider-neutral Router impl.
   ▲      ▲
leaf-bench    depends on all four — benchmark harness + falsification experiments.
```

Workspace: edition 2024, resolver 3, MIT (`Cargo.toml`). Dependency rationale:
`rmcp = "1"` (features `client`, `transport-io`, `transport-child-process`) owns the MCP wire;
`reqwest` is rustls-only with default features off (leaf-llm only); `rand_chacha` + `rand_core`
with no `rand` facade (version-stable stream, no ambient entropy path); `sha2` for all digests;
`thiserror` v2 per-crate error enums; `tempfile` dev-only. Traits use native
`impl Future` returns (edition-2024 async-fn-in-trait), so no `async-trait` dep.

## Crate-by-crate

### leaf-traits — vocabulary + seams

The three traits exist so demoted design hypotheses stay swappable *experiments*, not beliefs
baked into the executor; "exactly the impls named in the design exist — do not add more"
(`src/lib.rs:3-6`).

- **`DeterminismClass`** (`types.rs:9-14`): `Deterministic` (replay re-executes, byte-asserts) vs
  `ReplayOnly` (replay serves from journal). serde `snake_case` — this enum is journaled.
- **`HistoryItem`** (`types.rs:19-34`): `RunInput | Decision | LeafOutput` — the unit of run
  history fed to Propagation/Codec. `CandidateInfo` (`types.rs:38-45`): stable menu id `c0..cN`,
  name, and a *tree-authored* (trusted, never leaf output) description.
- **`Codec`** (`codec.rs:42-47`): renders history + candidate menus into router-prompt text.
  PROMPT RENDERING ONLY — rmcp owns the wire (`codec.rs:3-6`). `name()` is journaled. Renders
  must be byte-deterministic; that is what makes `prompt_digest` assertions meaningful. Impls:
  `JsonCodec` ("json.v1"), `ToonCodec` ("toon.v1", terse). Both route every leaf-output payload
  through `fence_untrusted` (`codec.rs:23-40`): fixed markers `<<<untrusted-output` /
  `>>>end-untrusted-output`, caller-authored label forced single-line, and any data line whose
  backslash-stripped prefix matches a marker gains one leading `\` — injective escaping, so
  fenced blocks nest without ambiguity.
- **`Propagation`** (`propagation.rs:13-18`): policy for how history flows into the next prompt —
  the surviving form of the "Markov" idea, an A/B/C experiment. `FullLog` ("full"),
  `LastK` ("last_k": RunInput always kept + deterministic `[N earlier items elided]` marker),
  `DecayedSummary` ("decayed": per-item char budget `base * (decay_num/decay_den)^age`, integer
  u128 arithmetic, floored at `min_chars`, `propagation.rs:73-82`; run input never decayed;
  truncation is char-boundary-safe with a fixed `…[truncated]` marker).
- **`Transport`** (`transport.rs:63-69`) + **`LeafHandle`** (`transport.rs:52-61`): connect to a
  `LeafSpec`, call tools with a timeout. `LeafSpec` (`transport.rs:21-35`): id, allowlisted server
  binary, args, explicit env `BTreeMap` (sorted → digest-stable serialization), cwd jail,
  `call_timeout_ms`, `expected_determinism` (cross-checked at connect). Errors include
  `NotAllowlisted` and `DeterminismMismatch` (`transport.rs:37-49`).

### leaf-core — executor, journal, replay

- **`Clock`** (`clock.rs:7-9`): all timestamps injected; `SystemClock` prod, `FixedClock` tests +
  replay. Timestamps are informational only — replay never asserts them.
- **`sha256_hex`** (`digest.rs:4-14`): the one digest primitive, golden-tested.
- **`Tree`** (`tree.rs:11-24`): plain Rust structs with serde derives. `Tree::digest()` = sha256 of
  canonical JSON — serde_json maps are BTreeMaps (sorted keys), struct fields serialize in
  declaration order, so bytes are stable (`tree.rs:21-24`). `Node` = `Branch | Leaf | Done`.
  A branch is a loop head: Leaf pick runs the tool and returns to the same branch; Branch pick
  descends; Done pick pops (or, if `terminal`, ends the whole run). **The HYBRID lever**: a
  `Branch` carries a `route` mode — `llm` (the Router picks; the only kind that bills a call and
  journals a `BranchDecision`) or `sequence` (the executor walks the candidates in cursor order
  with NO router call, reproduced from the tree on replay). A forced pipeline is a `sequence`
  (fetch→reduce→terminal-done, 0 LLM calls after the root routes to it); a hybrid capability nests
  an `llm` branch inside a sequence (fetch → classify → reduce). Determinism is not only a leaf
  property — a branch is deterministic wherever the task's next step is forced.
- **`ArgsTemplate`** (`tree.rs:89-131`): a JSON value whose string leaves may contain `{input}`,
  `{last_output}`, `{seed}`; substitution is plain string replacement over the whole value tree.
  The LLM routes; it never authors arguments (`tree.rs:79-81`). Fully substituted args are
  journaled and re-derived + asserted on replay.
- **`derive_seed(run_seed, step)`** (`tree.rs:135-142`): first 8 LE bytes of
  `sha256(run_seed.to_le_bytes() || step.to_le_bytes())`. Frozen forever — golden-pinned
  (`tree.rs:178-184`: `derive_seed(42,0) == 0x91491766afd15e19`); changing it breaks replay of
  old journals. Derived per call only iff the template uses `{seed}` (`executor.rs:236-237`).
- **`Router`** (`router.rs:76-81`): the LLM boundary. `RouteRequest` carries goal, candidates,
  rendered context, executor-computed `prompt_digest`, decision index (`router.rs:10-20`);
  `RouteDecision` carries chosen index + optional `ExplorationRecord` + optional `LlmCallRecord`
  (`router.rs:23-31`). `ExplorationRecord` (`router.rs:35-44`) is integer-only epsilon-greedy
  bookkeeping: `epsilon_ppm`, `roll_ppm = next_u64 % 1_000_000`, `explored`, raw `pick` iff
  explored. `LlmCallRecord` (`router.rs:49-64`) is the verbatim record of the ONLY
  nondeterministic component: model, sha256 of exact request bytes, verbatim response text, stop
  reason, token counts, `cost_microusd` (integer at a pinned price table), `parse_fallback`.
  `ScriptedRouter` (`router.rs:86-107`) is the zero-LLM test/bench impl.
- **`Executor`** (`executor.rs:63-298`): ties Tree, Router, Transport, Propagation, Codec, Clock,
  Journal together. Per decision: render context, build menu, compute
  `prompt_digest = sha256(goal \0 rendered \0 menu)` (`executor.rs:119-121`), route, journal
  LlmCall (iff billed) then BranchDecision, then act on the node. Leaf handles are connected
  lazily and cached per leaf id (`executor.rs:207-226`). `run_id` =
  `hex(derive_seed(seed, u32::MAX))` — deterministic, not a uuid, so replay reproduces it
  (`executor.rs:77`). Caps: `max_steps` (routing decisions) and `max_llm_calls` (billed calls
  only — exploration-skipped decisions don't count, `executor.rs:29-32`), both checked at the
  loop head after the stack-empty check so a run completing on its last allowed call is `Done`,
  not a cap outcome (`executor.rs:102-109`). Every error path still journals `RunFinished` with
  an `Error` outcome before returning (`executor.rs:135-147` et al.).
- **`Journal`** (`journal.rs`): see next section.
- **`replay`** (`replay.rs:162-253`): see Replay semantics.

### leaf-mcp — MCP client transport + the leaf-wrap server

- **`WrapManifest`** (`manifest.rs:12-37`): how one CLI binary becomes a sandboxed MCP leaf —
  tool name/description, `determinism` declaration, `binary` + its own `allowlist`,
  `args_template` (each element one argv entry, never a shell string), optional `stdin_from`
  arg key, `timeout_ms`, `workdir` jail, explicit env, and a JSON-Schema `input_schema`
  (hand-rolled checks, no jsonschema dep). JSON, not a custom DSL. `validate()` requires the
  binary to appear in its own allowlist (`manifest.rs:62-67`). `redacted()` drops `env` — it may
  hold tokens (`manifest.rs:76-82`). Example: `examples/manifests/jq.json`.
- **`sandbox`** (`sandbox.rs`): `check_allowlisted` canonicalizes the binary (symlinks resolved)
  and requires exact match against a canonicalized allowlist entry (`sandbox.rs:48-59`).
  `run_sandboxed` (`sandbox.rs:61-109`): the check uses the canonical path but execution uses the
  *manifest* path — multi-call binaries (coreutils, busybox) dispatch on argv[0], so spawning the
  canonical target would change the tool's personality (`sandbox.rs:70-72`). `env_clear()` +
  explicit env, cwd = jail, piped stdio, `kill_on_drop(true)`, `timeout_ms` enforced. Non-UTF-8
  output is lossy-converted exactly once, at this boundary (`sandbox.rs:100-103`).
- **`WrapServer`** (`wrap_server.rs`): manual rmcp `ServerHandler` exposing exactly two tools —
  the wrapped tool (determinism note appended to its description) and the reserved
  `__leaf_manifest` tool (`MANIFEST_TOOL`, `wrap_server.rs:20`) returning `manifest.redacted()`.
  Arg handling: required args enforced, unknown args rejected, scalars only
  (`wrap_server.rs:35-45,119-138`). `substitute` (`wrap_server.rs:50-81`) is single-pass:
  `{key}` placeholders resolve left-to-right and **inserted values are never rescanned**, so a
  value containing `{other_key}` lands verbatim; undeclared braces (jq programs!) pass through.
  Exit 0 → `CallToolResult::success(stdout)`; nonzero exit → `CallToolResult::error` (tool-level,
  model-visible); sandbox refusal/timeout → MCP `internal_error` (protocol-level,
  `wrap_server.rs:152-163`).
- **`leaf-wrap` bin** (`bin/leaf_wrap.rs`): `leaf-wrap --manifest <path>` serves one manifest on
  stdio. HARD RULE: stdout is the MCP wire, all logging to stderr (`leaf_wrap.rs:4-5,15-21`).
  Startup re-runs the allowlist check and refuses to serve a violating manifest
  (`leaf_wrap.rs:35-37`).
- **`StdioMcpTransport`** (`client.rs:17-99`): the single production `Transport` impl. Connect:
  canonicalize + exact-match the *server* binary against the runtime `server_allowlist` before
  spawn (`client.rs:48-57`); spawn with `env_clear` + explicit env + optional cwd; then call
  `__leaf_manifest`, parse the declared `DeterminismClass`, and fail `DeterminismMismatch` if it
  differs from `spec.expected_determinism` (`client.rs:71-95`). Calls concatenate text content
  blocks and time out per `LeafSpec` (`client.rs:106-136`). rmcp's `TokioChildProcess` kills the
  leaf process when the handle drops.

### leaf-llm — routing client + propagation

Provider-neutral `LlmClient` (Anthropic and OpenAI) producing shared neutral response types.
`LlmRouter: Router` (1) seeds a `rand_chacha` ChaCha8 stream from the run seed via `rand_core`
(no `rand` facade, no ambient entropy); (2) rolls epsilon-greedy in ppm integers **before** the
LLM call — an explored decision picks uniformly from the menu and **skips the LLM entirely** (so
`RouteDecision.llm` is `Option` and `max_llm_calls` counts only billed calls); (3) journals each
billed call as a verbatim `LlmCallRecord` with integer micro-USD cost at a pinned price table;
(4) on an unparseable answer fires a deterministic fallback and sets `parse_fallback`. Plain-text
answers, no prefill. OpenAI reasoning models (`gpt-5*`) use `max_completion_tokens` and
`reasoning_effort: none`, and reject `stop`/`minimal`. Offline token counting via tiktoken
(o200k_base). Files: `prompt.rs` (routing rules), `explore.rs` (explorer), `pricing.rs`.

### leaf-bench — benchmark harness

Subcommands: `preflight`, `ground-truth` (capture cassette + snapshots), `exp
<encoding|determinism|ceiling>`, `run [--condition C] [--task T]`, `live-control` (free-driving
control vs real GitHub, record-on-miss to a separate cassette), `cc-control` (the FAIR baseline:
`claude -p` Sonnet + Bash + live `gh`, AXI's actual harness — Anthropic-billed), `judge-audit`
(strict-judge vs checker agreement), `judge-grade` (AXI-faithful LLM judge + per-task range
hints), `report`, `budget`. The tree runs on the API (gpt-5.4-mini) so it stays seeded/replayable;
the Claude Code control runs live via the `claude` CLI. Deterministic checkers grade; costs
metered from journaled tokens; budget ledger with a hard cap. Tasks: `bench/tasks/*.toml`.

## Journal format

Append-only JSONL (`journal.rs`): one compact-JSON entry per line, flushed after every entry so a
crash leaves a valid prefix (`journal.rs:114-121`); `create_new` refuses to overwrite an existing
file (`journal.rs:109-112`). `FORMAT_VERSION = 1` (`journal.rs:19`). NO floats anywhere — money in
micro-USD `u64`, epsilon in ppm `u32`; enforced by a serialization-walk test
(`journal.rs:245-260`). Outputs are stored verbatim as JSON strings (JSON string escaping
round-trips any UTF-8 exactly — no base64).

| Entry (`type`)    | Fields | Notes |
|---|---|---|
| `run_started`     | `run_id`, `seed`, `tree_digest`, `input`, `codec`, `propagation`, `format_version`, `ts_ms` | `run_id = hex(derive_seed(seed, u32::MAX))`; `tree_digest` asserted on replay |
| `llm_call`        | `step` + flattened `LlmCallRecord` (model, `request_digest`, `response_text`, `stop_reason`, token counts, `cost_microusd`, `parse_fallback`) | emitted immediately before its `branch_decision`; only for billed calls |
| `branch_decision` | `step`, `branch`, `prompt_digest`, `candidates` (names in menu order), `chosen`, `exploration?` | `prompt_digest = sha256(goal\0context\0menu)` — the real determinism check on Codec+Propagation+history |
| `leaf_call`       | `step`, `leaf`, `tool`, `input` (fully substituted args), `seed?`, `determinism`, `output` (verbatim), `is_error`, `duration_ms` | `seed` present iff the template used `{seed}` |
| `run_finished`    | `steps`, `outcome` (`done` \| `max_steps` \| `max_llm_calls` \| `error{message}`), `total_cost_microusd`, `total_input_tokens`, `total_output_tokens`, `ts_ms` | written on every path, including errors |

## Replay semantics

`replay()` (`replay.rs:162-253`) re-runs the executor from a recorded journal with two
substitutions, writes a fresh journal, and hands both to the structural comparator. **ALL
divergence detection flows through the comparator** (`compare`, `replay.rs:256-283`): entries are
serialized to JSON, exempt fields stripped (`strip_exempt`, `replay.rs:285-291`), and walked for
the first differing field in sorted-key order (`first_diff`, `replay.rs:305-357`), reported as
`Divergence { entry_index, field, expected, actual }` with 500-char display truncation. A length
mismatch is itself a divergence (`journal.length`). Executor errors mid-replay are divergences
too — the run error is ignored and the comparator reports structurally (`replay.rs:247-249`).
Effective caps are recovered from the recorded journal so guard-terminated runs replay identically
(`replay.rs:179-199`).

| Category | Mechanism | What it covers |
|---|---|---|
| **Served from journal** | `ReplayRouter` (`replay.rs:80-92`) pops recorded `(LlmCall?, BranchDecision)` pairs — no LLM is ever called; vendor nondeterminism is quarantined by construction. `ReplayHandle::Journal` (`replay.rs:107-143`) serves `ReplayOnly` leaf outputs from a queue — zero network/subprocess (proven by a live-call counter, `replay.rs:419-463`) | LLM decisions + records; network/side-effecting leaf outputs |
| **Re-executed + asserted** | `ReplayTransport` (`replay.rs:145-158`) routes `Deterministic` leaves to the real inner transport; the comparator then byte-asserts `leaf_call.output` | pure/local tools — a changed tool version or tampered journal diverges |
| **Re-derived + asserted** | executor recomputes them during replay; comparator diffs | `prompt_digest` (Codec/Propagation/history drift), `leaf_call.input` (ArgsTemplate drift), `seed` (derivation drift), `tree_digest`, `codec`/`propagation` names, `run_id`, candidates/chosen sequence, totals, outcome |
| **Exempt (informational)** | stripped before comparison | `ts_ms`, `duration_ms` only |

## Security model

Honest scoping: this is **policy sandboxing for cooperative binaries, not containment of hostile
ones** (`sandbox.rs:12-19`). No seccomp, no namespaces, no cgroups, no rlimits — a wrapped binary
can still read `/etc/passwd`, open sockets, or fork-bomb. The threat model is **prompt-injected
arguments to a trusted tool**, and the actual defenses are structural:

- **Two allowlist layers, both canonicalize-then-exact-match**: the runtime's server allowlist
  before spawning any leaf server (`client.rs:48-57`), and the manifest's binary allowlist checked
  at leaf-wrap startup (`leaf_wrap.rs:35-37`) and again per call (`sandbox.rs:66`). Execution uses
  the manifest path (argv[0] personality for multi-call binaries); the check uses the canonical one.
- **No shell, ever**: args are argv array entries (`sandbox.rs:73-74`); template substitution is
  single-pass and never rescans inserted values (`wrap_server.rs:50-81`); unknown args are
  rejected; args must be scalars.
- **Env hygiene**: `env_clear` + explicit allowlist for both the leaf server (`client.rs:60-65`)
  and the wrapped binary (`sandbox.rs:75-76`) — no HOME, no PATH, no API keys; env is redacted
  from the `__leaf_manifest` surface (`manifest.rs:76-82`).
- **Prompt side**: every byte of leaf output entering a prompt goes through `fence_untrusted`
  with injective marker escaping (`codec.rs:23-40`); candidate descriptions are tree-authored,
  never leaf output.
- **Process lifetime**: cwd jail, `timeout_ms` + `kill_on_drop` (`sandbox.rs:81,94-107`); rmcp
  kills leaf servers on handle drop.
- **Journal integrity**: `create_new` (append-only artifacts, no overwrite); tampering is caught
  by replay, not by trust.

Upgrade path if the threat model ever includes hostile binaries: `unshare(1)`,
`systemd-run --scope`, or Landlock (`sandbox.rs:18-19`).

## Decisions log

1. **LLM routes; code does everything else.** The `Router` trait lives in leaf-core
   (`router.rs:76`); leaf-llm implements it — leaf-core never depends on reqwest or rmcp (check
   its `Cargo.toml`). Leaf inputs are deterministic `ArgsTemplate` substitution of
   `{input}/{last_output}/{seed}` (`tree.rs:89-131`); the LLM never authors arguments. Rationale:
   the entire determinism contract collapses if generated text can reach argv.
2. **Journal discipline.** Append-only JSONL, flush per entry (crash → valid prefix), NO floats
   (micro-USD u64, ppm u32 — float formatting instability cannot exist), outputs verbatim JSON
   strings (no base64 — JSON escaping round-trips UTF-8 exactly), timestamps informational-only
   via injected `Clock`. `derive_seed` = first 8 LE bytes of `sha256(seed_le || step_le)`,
   golden-pinned (`tree.rs:178-184`) — frozen because changing it breaks replay of old journals.
3. **Replay = one comparator.** All divergence detection flows through the structural journal
   comparator (strip `ts_ms`/`duration_ms`, first-diff walk) — no scattered asserts. `ReplayRouter`
   serves recorded LLM decisions (vendor nondeterminism quarantined); `ReplayTransport` re-executes
   `Deterministic` leaves live (byte-asserted via the comparator) and serves `ReplayOnly` outputs
   from the journal with zero network/subprocess. Rationale: one mechanism, one exemption list,
   one failure report shape.
4. **Codec is prompt-rendering only.** rmcp owns the JSON-RPC wire; a Codec renders history and
   menus into router-prompt text (`codec.rs:3-6`), and all untrusted leaf output is routed through
   `fence_untrusted` (escaped fence markers, injective). The `Transport` trait exists as a seam
   for replay substitution and for the demoted transport experiments — UDP/QUIC were **disproven**
   (one LLM call dwarfs a TCP handshake by ~4 orders of magnitude, `transport.rs:5-7`); the seam
   is its whole justification, and `StdioMcpTransport` is the single production impl.
5. **Propagation = 3 deterministic policies** (full / last_k / decayed) behind a trait, compared
   as an A/B/C experiment, not a belief. `DecayedSummary` is deterministic integer-ratio
   *truncation*, NOT LLM summarization — LLM summarization would put nondeterminism below the
   router, violating the core contract (`propagation.rs:6-8`).
6. **Sandbox honesty.** Policy sandboxing for cooperative binaries (allowlist canonicalize-check
   but exec by manifest path for argv[0] multi-call binaries; env_clear + explicit env; cwd jail;
   timeout + kill_on_drop) — NOT containment (no seccomp/namespaces). The real defense against
   prompt-injected args is argv-array passing + fencing. Upgrade path recorded:
   unshare/systemd-run/Landlock.
7. **Determinism contract rides `__leaf_manifest`.** MCP has no standard server-metadata surface
   (priors §3), so the contract is a reserved tool — stock clients just see two tools and lose
   nothing. Env is redacted from it. The client cross-checks declared vs expected class at connect
   and fails `DeterminismMismatch` (`client.rs:71-95`) — a network tool cannot masquerade as
   deterministic.
8. **RNG: rand_chacha ChaCha8 + rand_core only, no `rand` facade** — version-stable stream, no
   ambient-entropy path. Exploration bookkeeping is ppm integers (`ExplorationRecord`,
   `router.rs:35-44`); the roll happens BEFORE the LLM call and an explored decision skips the
   LLM entirely (`RouteDecision.llm: Option`; `max_llm_calls` counts billed calls only). The
   leaf-core owns the types and executor accounting; `LlmRouter` (leaf-llm) implements the policy.
9. **Trees are Rust structs with serde derives.** `tree_digest` = sha256 of canonical JSON —
   serde_json's map is a BTreeMap (sorted keys), so bytes are stable (`tree.rs:21-24`). No config
   DSL now; JSON-loadable later for free.

## Test map

Acceptance-criteria integration tests (`crates/leaf-mcp/tests/`, run against a real `leaf-wrap`
child process and a real `jq`):

| Criterion | Test | Proves |
|---|---|---|
| **1 — record/replay/tamper over real MCP** | `jq_replay.rs::record_replay_tamper_through_real_mcp_stdio` | jq leaf behind leaf-wrap driven by the executor over MCP stdio; replay of a Deterministic leaf genuinely re-executes and is `Identical`; a tampered `leaf_call.output` diverges at the exact entry and field |
| **4 — leaves are REAL MCP stdio servers** | `mcp_compat.rs::stock_rmcp_client_lists_and_calls_the_wrapped_tool` | a stock rmcp client (no leaf-* client code) initializes, lists `jq` + `__leaf_manifest`, calls the tool, reads the redacted determinism declaration |
| 4 (error surface) | `mcp_compat.rs::tool_error_surfaces_as_tool_error_not_protocol_failure` | invalid jq program → `is_error: true` tool result, protocol stays healthy |
| **7 — non-allowlisted binary refused** | `allowlist.rs::leaf_wrap_refuses_manifest_whose_binary_is_not_allowlisted` (server side) + `::client_refuses_non_allowlisted_server_binary` (client side) | refusal at both layers, naming the allowlist |
| 7 (contract) | `allowlist.rs::client_detects_determinism_mismatch` | connect fails `DeterminismMismatch` when declaration ≠ spec |

Key invariant unit tests: `derive_seed` golden pin (`tree.rs:178`); no-float journal guard
(`journal.rs:245`); journal crash-prefix + create-refuses-overwrite (`journal.rs:210,225`); fence
escaping incl. already-escaped markers and label breakout (`codec.rs:192-219`); replay-only leaves
served with zero live calls, proven by counter (`replay.rs:419`); wrong codec on replay diverges at
`run_started.codec` (`replay.rs:552`); replay reproduces LLM records + totals (`replay.rs:511`);
executor journals `run_finished` on error paths (`executor.rs:413`); cap semantics
(`executor.rs:390`); env scrub / timeout kill / cwd jail / stdin+argv (`sandbox.rs:140-199`);
substitution never rescans inserted values and leaves undeclared braces alone
(`wrap_server.rs:224-248`); propagation determinism + window edges (`propagation.rs:139-223`).
