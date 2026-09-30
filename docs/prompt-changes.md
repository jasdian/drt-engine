# Prompt changes — semantics and effect

The runtime's routing behaviour is steered entirely by PROMPT TEXT: the router prompt, the
tree's capability descriptions, and the control's system prompt. No change to the routing
algorithm, the leaves, the data, the ground truth, or the grading. This documents each prompt
rule, what it means, the failure it prevents, and the measured effect — and why prompt-only
changes were enough.

## Router prompt (`crates/leaf-llm/src/prompt.rs`, `PROMPT_V1`)

Four rules, each governing a different routing decision:

| # | rule (abbreviated) | decision it governs | failure it prevents |
|---|---|---|---|
| 1 | choose the immediate next step; don't skip a preparatory step because a candidate NAME resembles the goal | **sequencing** (prerequisite order) | jumping to a "reduce/answer"-named leaf before the fetch that feeds it (name-lure) |
| 2 | never re-pick a candidate whose result is already the most recent output | **anti-degeneracy** | infinite loops; re-transforming an already-reduced object (field clobber) |
| 3 | finish as soon as the finished result is present; don't fetch/transform again | **termination** | over-fetching after the answer already exists |
| 4 | a raw fetch is NOT a finished result — run the available reduce/tally/count first; never finish right after a raw fetch | **termination** (what "done" means) | premature-done: picking "finish" after the fetch, so the answer is raw JSON, not the reduced object |

**Rule 4 is the load-bearing one.** The LLM's default heuristic is "if the data I need is
visible in context, I'm done." In a fetch→reduce pipeline that is wrong: the visible raw fetch
is not the answer — it needs the deterministic reduction. Rule 4 corrects that false heuristic.

Measured: on the general tree WITHOUT reduce-before-finish, the router picked "finish"
immediately after the GraphQL fetch on `merged_pr_ci_audit` (and similar), so the graded answer
was the raw JSON, not the `{pr1_title, …, green_count}` object → fail. WITH rule 4 (and the
matching capability descriptions below), the router runs fetch→reduce→done → the reduced object
→ pass. It is a semantic clarification of "done", not a logic or capability change.

Rules 1–3 are the same class of guidance for the other common routing errors (skip-a-step,
loop, over-run). None carries any task's answer, id, or number, so none can make a wrong answer
pass — the independent routing-accuracy metric (99%) confirms the router genuinely selects the
correct capability.

## Tree capability descriptions (`bench/general-tree.json`)

- Each capability branch's routing goal is phrased **"Use when the task asks about X"** — a
  SELECTION hint mapping the prompt to the right capability (routing accuracy 99%).
- Each leaf is tagged **"REQUIRED first step"** (fetch) / **"REQUIRED reduce step — the
  operation is NOT done until this runs"** (reduce), and each sub-branch's "finish" reads
  **"choose ONLY after the reduce has produced the final object; NEVER right after a raw
  fetch."** This is rule 4's reduce-before-finish semantic, localised to each capability.

## Control system prompt (`crates/leaf-bench/src/control.rs`)

Aligned to AXI's grader rules: **"ALWAYS call the tools for real data — never hallucinate;
answer COMPLETELY and specifically with the actual titles/authors/numbers/counts; report the
not-found result for a missing resource."** Semantic: forces the free-driving baseline to fetch
(not guess), give a complete answer, and handle error-recovery — a stronger, fairer baseline
than the vague earlier prompt.

## How the changes differ

They are not one kind of edit; each targets a different failure mode of an LLM navigating a
tool tree:

- **Selection** (capability "Use when…"): pick the right operation for the goal.
- **Sequencing** (rule 1; "REQUIRED first step"): do the prerequisite before the thing that
  depends on it.
- **Termination** (rules 3 & 4; reduce-before-finish): decide when the operation is actually
  done — the subtle case being that a visible raw fetch is NOT the finished answer.
- **Anti-degeneracy** (rule 2): don't repeat a completed step.
- **Baseline quality** (control prompt): make the free-driver fetch real data and answer fully.

## Why "just the prompt" was enough

The routing algorithm, leaves, data, ground truth, and grading are unchanged. The gains come
from encoding the STRUCTURE the deterministic pipeline needs — which capability, the
fetch→reduce→done order, and "a raw fetch is not the answer" — as natural-language rules the
router follows. The largest single win was the termination semantic (rule 4): correcting the
LLM's "data present ⇒ done" heuristic to "reduced object present ⇒ done." Because the rules
carry no task-specific answers or ids, they steer routing without being able to rig a result;
the runtime holds at 100% routing / ~100% end-to-end, live and frozen.

## Superseded by the hybrid tree (structural, not prompt)

Rules 1–4 were a *prompt* workaround for a tree where the LLM routed EVERY step, including the
forced `fetch → reduce → finish` sequence inside each capability. That design paid an LLM call
to rubber-stamp each forced step (8–12 calls/task) and needed rule 4 to stop the router
finishing on a raw fetch.

The hybrid tree removes the workaround at its root. A branch now carries a `route` mode: the
root is `llm` (pick the capability — the one genuine choice), and each capability is a
deterministic `sequence` the executor walks with zero router calls, ending in a `terminal`
done. The forced order is now ENFORCED by the executor, not coaxed by a prompt — so **rule 4 no
longer does anything on a forced pipeline** (there is no LLM decision there to mis-fire), and a
forced task costs **1 LLM call**, not 8–12. The prompt rules still apply, but only where a
genuine decision remains: the root routing, and the one hybrid capability's `classify` branch
(`issue_triage`: fetch the issue → LLM decides bug vs feature → deterministic reduce). This is
the sharper form of the same idea — the LLM's job is the choice; the structure is the code's.
