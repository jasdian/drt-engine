# Experiment 03: decomposition-ceiling test

**Question** — is $0.050/completed-task beatable *in principle* by a routing-only-LLM + deterministic-leaf decomposition?

**Prediction (pre-registered)** — the hand-optimal decomposition of `view_pr` (floor: ~1 routing call) is well under $0.050, and even `merged_pr_ci_audit` (ceiling: the most data-heavy AXI task) stays under it. Kill rule: if EITHER hand-optimal is ≥ $0.050, the economic headline is dead — recorded, and the project's value becomes the replay/audit capability, honestly framed.

**Method** — walk each task's reference route, reconstruct the exact router prompt at each decision (frozen PROMPT_V1 + TOON context + candidate menu), count its tokens via the router model's tokenizer (tiktoken `o200k_base` for OpenAI here), add an assumed 8 output tokens per decision (the router's `max_tokens`), price at gpt-5.4-mini's pinned table (a conservative estimate; see pricing.rs). Leaf outputs are offline stand-ins here; the live benchmark measures the real thing. Deterministic-leaf work is $0 (no LLM).

## Results

| task | routing calls | prompt tokens | assumed output tokens | hand-optimal cost |
|---|---|---|---|---|
| view_pr | 3 | 834 | 24 | $0.0003 |
| merged_pr_ci_audit | 3 | 1120 | 24 | $0.0004 |

## Decision

**Headline ALIVE.** Every hand-optimal decomposition is under $0.050/task, so a routing-only runtime *can* in principle beat the AXI headline. Whether it does in practice is the Phase-4 benchmark's job (`docs/results.md`); this experiment only establishes the ceiling is not self-defeating.

Note: this measures the runtime's OWN prompt shape (routing-only). A no-runtime control that free-drives tools pays for tool schemas + full-transcript resends on top — measured separately in `docs/results.md`.

## Reproduce

```
leaf-bench exp ceiling
```
