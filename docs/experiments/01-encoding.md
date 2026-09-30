# Experiment 01: encoding round-trip

**Question** — which wire encoding of a tool-call payload minimizes tokens without breaking model comprehension?

**Prediction (pre-registered)** — terse text (TOON-style) beats compact JSON on tokens at equal extraction accuracy; hex and base64 inflate tokens ~2–3× AND crater accuracy (BPE fragments arbitrary bytes to ~2 bytes/token vs ~4 for terse English).

**Method** — 3 captured `gh`/`jq` payloads × {compact JSON, TOON-terse header+rows, hex-of-JSON-bytes, base64-of-JSON-bytes}. Token counts via the router model's tokenizer (Anthropic `count_tokens`, or tiktoken `o200k_base` for OpenAI). Field-extraction accuracy over K=5 trials with early-stop when a cell scores 0/N on its first trial (byte encodings are expected to fail hard). Model: gpt-5.4-mini.

## Results

```
p1.json json — 1780 bytes, 507 tokens (0.28 tok/byte), 20/20 correct over 5 trial(s)
p1.json toon — 1375 bytes, 434 tokens (0.32 tok/byte), 20/20 correct over 5 trial(s)
p1.json hex — 3560 bytes, 1650 tokens (0.46 tok/byte), 15/20 correct over 5 trial(s)
p1.json base64 — 2376 bytes, 1544 tokens (0.65 tok/byte), 17/20 correct over 5 trial(s)
p2.json json — 1241 bytes, 406 tokens (0.33 tok/byte), 25/25 correct over 5 trial(s)
p2.json toon — 727 bytes, 296 tokens (0.41 tok/byte), 25/25 correct over 5 trial(s)
p2.json hex — 2482 bytes, 1171 tokens (0.47 tok/byte), 25/25 correct over 5 trial(s)
p2.json base64 — 1656 bytes, 1061 tokens (0.64 tok/byte), 25/25 correct over 5 trial(s)
p3.json json — 629 bytes, 197 tokens (0.31 tok/byte), 25/25 correct over 5 trial(s)
p3.json toon — 629 bytes, 197 tokens (0.31 tok/byte), 25/25 correct over 5 trial(s)
p3.json hex — 1258 bytes, 590 tokens (0.47 tok/byte), 20/25 correct over 5 trial(s)
p3.json base64 — 840 bytes, 535 tokens (0.64 tok/byte), 25/25 correct over 5 trial(s)
```

## Decision

The runtime's leaf projection format is the encoding with the lowest token count among those with extraction accuracy ≥ 24/25 (see the table). Encodings that failed the accuracy bar are excluded WITH DATA, not on taste.

## Reproduce

```
leaf-bench ground-truth   # captures bench/fixtures/p{1,2,3}.json
leaf-bench exp encoding
```
