# Experiment 02: determinism-of-world probe

**Question** — which CLI tools may honestly declare `deterministic: true` (replay re-executes and byte-asserts) vs `replay_only` (journaled replay mandatory)?

**Prediction (pre-registered)** — pure/local tools 10/10 byte-identical; network tools not; therefore classes {pure, fs_stable, network}, with network ⇒ journal+replay mandatory.

**Method** — each probe run 10× with a scrubbed, pinned env (`LC_ALL=C`, `TZ=UTC`; gh additionally gets HOME for auth), stdout hashed with SHA-256, identical-to-first counted. Zero LLM calls; spend $0.00.

## Results

| probe | byte-identical | expectation | note |
|---|---|---|---|
| jq (pure transform) | 10/10 | 10/10 identical (pure) | 1 distinct output(s) |
| sha256sum (filesystem, fixture) | 10/10 | 10/10 identical (fs_stable) | 1 distinct output(s) |
| ls -1 (filesystem, directory) | 10/10 | 10/10 within a session; not across sessions | 1 distinct output(s) |
| curl example.com (network, stable page) | 10/10 | body likely stable; still network class | 1 distinct output(s) |
| gh api repos/openclaw/openclaw (network, live repo) | 10/10 | <10/10 (updated_at, counters drift) | 1 distinct output(s) |

## Decision

- `jq`, `sha256sum` (fixture-pinned): may declare `deterministic` — replay re-executes them live and byte-asserts.
- `ls`: deterministic only while the directory is unchanged — treat as `fs_stable`; leaves over mutable directories must declare `replay_only`.
- `curl`, `gh`: `replay_only`, regardless of how stable a particular endpoint looks today. Grading must assert only against snapshots, never live values.

These classes are encoded in the leaf manifests (`bench/artifacts/manifests/*.json`).

## Reproduce

```
cargo run -p leaf-bench -- exp determinism
```
