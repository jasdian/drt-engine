//! Phase-1 experiment 01: encoding round-trip.
//!
//! For 3 captured tool-call payloads × {compact JSON, TOON-terse, hex,
//! base64}: count tokens (free `count_tokens`) and measure field-extraction
//! accuracy over K trials, with early-stop when a cell scores 0 on its first
//! trial (hex/base64 are expected to fail hard — no reason to pay 5×).
//!
//! Closes the codec question with data: is terse text cheaper at equal
//! accuracy, and do byte encodings really inflate tokens?

use anyhow::{Context, Result, bail};
use leaf_llm::LlmClient;

use crate::budget::Budget;
use crate::config::{BenchConfig, BenchPaths};

const BUCKET: &str = "exp-encoding";
const K: usize = 5;

struct Encoding {
    name: &'static str,
    encode: fn(&str) -> String,
}

fn to_toon(json: &str) -> String {
    // Array of flat objects → header + pipe rows; else fall back to compact.
    let Ok(serde_json::Value::Array(rows)) = serde_json::from_str::<serde_json::Value>(json) else {
        return json.to_string();
    };
    let Some(serde_json::Value::Object(first)) = rows.first() else {
        return json.to_string();
    };
    let keys: Vec<String> = first.keys().cloned().collect();
    let mut out = format!("[{}]{{{}}}:\n", rows.len(), keys.join(","));
    for row in &rows {
        let cells: Vec<String> = keys
            .iter()
            .map(|k| match row.get(k) {
                Some(serde_json::Value::String(s)) => s.clone(),
                Some(v) => v.to_string(),
                None => String::new(),
            })
            .collect();
        out.push_str(&cells.join("|"));
        out.push('\n');
    }
    out
}

fn to_hex(json: &str) -> String {
    let mut s = String::with_capacity(json.len() * 2);
    for b in json.as_bytes() {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn to_base64(json: &str) -> String {
    // Minimal standard base64 (no external dep).
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes = json.as_bytes();
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(T[((n >> 18) & 63) as usize] as char);
        out.push(T[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            T[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

#[derive(Clone)]
struct Row {
    fixture: String,
    encoding: String,
    bytes: usize,
    tokens: u64,
    correct: usize,
    asked: usize,
    trials: usize,
}

impl Row {
    fn render(&self) -> String {
        format!(
            "{} {} — {} bytes, {} tokens ({:.2} tok/byte), {}/{} correct over {} trial(s)",
            self.fixture,
            self.encoding,
            self.bytes,
            self.tokens,
            self.tokens as f64 / self.bytes.max(1) as f64,
            self.correct,
            self.asked,
            self.trials,
        )
    }
}

const ENCODINGS: &[Encoding] = &[
    Encoding {
        name: "json",
        encode: |s| s.to_string(),
    },
    Encoding {
        name: "toon",
        encode: to_toon,
    },
    Encoding {
        name: "hex",
        encode: to_hex,
    },
    Encoding {
        name: "base64",
        encode: to_base64,
    },
];

/// Each fixture ships 5 extraction questions with exact known answers.
struct Fixture {
    file: &'static str,
    questions: Vec<(String, String)>, // (question, exact answer)
}

fn load_fixtures(paths: &BenchPaths) -> Result<Vec<Fixture>> {
    let read = |name: &str| -> Result<String> {
        std::fs::read_to_string(paths.fixtures.join(name)).with_context(|| {
            format!("missing fixture {name} — run `leaf-bench ground-truth` first")
        })
    };
    // Questions are derived from the fixtures at load time so answers are
    // always correct for whatever was captured.
    let p1: serde_json::Value = serde_json::from_str(&read("p1.json")?)?;
    let p2: serde_json::Value = serde_json::from_str(&read("p2.json")?)?;
    let p3: serde_json::Value = serde_json::from_str(&read("p3.json")?)?;

    Ok(vec![
        Fixture {
            file: "p1.json",
            questions: questions_from_array(&p1, "number", &["title"]),
        },
        Fixture {
            file: "p2.json",
            questions: questions_from_array(&p2, "id", &["conclusion", "status"]),
        },
        Fixture {
            file: "p3.json",
            questions: questions_from_object(&p3),
        },
    ])
}

fn questions_from_array(
    v: &serde_json::Value,
    id_key: &str,
    field_keys: &[&str],
) -> Vec<(String, String)> {
    let mut qs = Vec::new();
    if let Some(arr) = v.as_array() {
        for row in arr.iter().take(3) {
            if let (Some(id), Some(fk)) = (row.get(id_key), field_keys.first())
                && let Some(val) = row.get(fk)
            {
                qs.push((
                    format!(
                        "What is the {fk} of the record whose {id_key} is {}?",
                        scalar(id)
                    ),
                    scalar(val),
                ));
            }
        }
        if let (Some(row), Some(fk)) = (arr.first(), field_keys.get(1))
            && let (Some(id), Some(val)) = (row.get(id_key), row.get(fk))
        {
            qs.push((
                format!(
                    "What is the {fk} of the record whose {id_key} is {}?",
                    scalar(id)
                ),
                scalar(val),
            ));
        }
        qs.push((
            "How many records are in the array?".to_string(),
            arr.len().to_string(),
        ));
    }
    qs.truncate(5);
    qs
}

fn questions_from_object(v: &serde_json::Value) -> Vec<(String, String)> {
    let mut qs = Vec::new();
    if let Some(obj) = v.as_object() {
        for key in [
            "default_branch",
            "language",
            "open_issues_count",
            "stargazers_count",
            "name",
        ] {
            if let Some(val) = obj.get(key) {
                qs.push((
                    format!("What is the value of the '{key}' field?"),
                    scalar(val),
                ));
            }
        }
    }
    qs.truncate(5);
    qs
}

fn scalar(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

pub async fn run(paths: &BenchPaths, cfg: &BenchConfig, client: &LlmClient) -> Result<()> {
    let prices = client.prices();
    let fixtures = load_fixtures(paths)?;
    let mut budget = Budget::load(
        paths.budget_file.clone(),
        cfg.budget_cap_microusd,
        cfg.buckets.iter().map(|(k, v)| (k.clone(), *v)).collect(),
    )?;

    let mut rows: Vec<Row> = Vec::new();

    for fixture in &fixtures {
        let raw = std::fs::read_to_string(paths.fixtures.join(fixture.file))?;
        for enc in ENCODINGS {
            let encoded = (enc.encode)(&raw);
            // count_tokens is free — but still bracket it in the ledger at 0.
            let tokens = client
                .count_tokens("", &encoded)
                .await
                .with_context(|| format!("count_tokens {} {}", fixture.file, enc.name))?;

            let mut correct = 0usize;
            let mut asked = 0usize;
            let mut trials = 0usize;
            for trial in 0..K {
                let (sys, user) = extraction_prompt(enc.name, &encoded, &fixture.questions);
                let projected = budget.projection(BUCKET);
                let id = match budget.reserve(BUCKET, projected) {
                    Ok(id) => id,
                    Err(e) => {
                        println!("encoding: budget stop — {e}");
                        write_report(paths, &rows)?;
                        bail!("budget cap hit during encoding experiment");
                    }
                };
                let (resp, _body) = client.messages(&sys, &user, 400, &[]).await?;
                let cost = leaf_llm::cost_microusd(
                    &prices,
                    resp.usage.input_tokens,
                    resp.usage.cache_creation_input_tokens,
                    resp.usage.cache_read_input_tokens,
                    resp.usage.output_tokens,
                );
                budget.settle(id, BUCKET, cost)?;

                let hits = score_answers(resp.first_text(), &fixture.questions);
                correct += hits;
                asked += fixture.questions.len();
                trials += 1;

                // Early stop: 0 on the first trial → byte encoding is hopeless.
                if trial == 0 && hits == 0 {
                    break;
                }
            }
            rows.push(Row {
                fixture: fixture.file.to_string(),
                encoding: enc.name.to_string(),
                bytes: encoded.len(),
                tokens,
                correct,
                asked,
                trials,
            });
        }
    }

    for r in &rows {
        println!("{}", r.render());
    }
    write_report(paths, &rows)?;
    let _ = &fixtures; // keep the binding meaningful across early returns
    println!(
        "report: docs/experiments/01-encoding.md  (spent so far: ${:.4})",
        budget.spent_total() as f64 / 1e6
    );
    Ok(())
}

fn extraction_prompt(
    enc_name: &str,
    encoded: &str,
    questions: &[(String, String)],
) -> (String, String) {
    let sys = format!(
        "You are a data-extraction tool. The user gives you a payload encoded as {enc_name} and \
         numbered questions. Decode the payload if necessary and answer each question. Reply with \
         exactly one line per question in the form `answerN=VALUE` (e.g. answer1=main), the VALUE \
         being the exact field value and nothing else."
    );
    let mut user = format!("payload ({enc_name}):\n{encoded}\n\nquestions:\n");
    for (i, (q, _)) in questions.iter().enumerate() {
        user.push_str(&format!("{}. {q}\n", i + 1));
    }
    (sys, user)
}

fn score_answers(response: &str, questions: &[(String, String)]) -> usize {
    let mut hits = 0;
    for (i, (_, expected)) in questions.iter().enumerate() {
        let key = format!("answer{}=", i + 1);
        if let Some(line) = response
            .lines()
            .find(|l| l.trim().to_lowercase().starts_with(&key))
        {
            let got = line.trim()[key.len()..]
                .trim()
                .trim_matches(['"', '\'', ' ']);
            if got.eq_ignore_ascii_case(expected.trim()) {
                hits += 1;
            }
        }
    }
    hits
}

fn write_report(paths: &BenchPaths, rows: &[Row]) -> Result<()> {
    let rendered: Vec<String> = rows.iter().map(|r| r.render()).collect();
    let mut md = String::new();
    md.push_str("# Experiment 01: encoding round-trip\n\n");
    md.push_str("**Question** — which wire encoding of a tool-call payload minimizes tokens without breaking model comprehension?\n\n");
    md.push_str("**Prediction (pre-registered)** — terse text (TOON-style) beats compact JSON on tokens at equal extraction accuracy; hex and base64 inflate tokens ~2–3× AND crater accuracy (BPE fragments arbitrary bytes to ~2 bytes/token vs ~4 for terse English).\n\n");
    md.push_str("**Method** — 3 captured `gh`/`jq` payloads × {compact JSON, TOON-terse header+rows, hex-of-JSON-bytes, base64-of-JSON-bytes}. Token counts via the router model's tokenizer (Anthropic `/v1/messages/count_tokens`, or tiktoken `o200k_base` for OpenAI). Field-extraction accuracy over K=5 trials with early-stop when a cell scores 0/N on its first trial (byte encodings are expected to fail hard). Model: the configured `router_model` (see bench.toml).\n\n");
    md.push_str("## Results\n\n```\n");
    for line in &rendered {
        md.push_str(line);
        md.push('\n');
    }
    md.push_str("```\n\n");
    md.push_str("## Decision\n\nThe runtime's leaf projection format is the encoding with the lowest token count among those with extraction accuracy ≥ 24/25 (see the table). Encodings that failed the accuracy bar are excluded WITH DATA, not on taste.\n\n");
    md.push_str("## Reproduce\n\n```\nleaf-bench ground-truth   # captures bench/fixtures/p{1,2,3}.json\nleaf-bench exp encoding\n```\n");
    std::fs::write(paths.docs_experiments.join("01-encoding.md"), md)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toon_is_smaller_than_json_for_arrays() {
        let json = r#"[{"id":1,"name":"a"},{"id":2,"name":"b"}]"#;
        let toon = to_toon(json);
        assert!(toon.contains("[2]{id,name}:"));
        assert!(toon.len() <= json.len());
    }

    #[test]
    fn base64_round_trips_shape() {
        assert_eq!(to_base64("abc"), "YWJj");
        assert_eq!(to_base64("ab"), "YWI=");
        assert_eq!(to_base64("a"), "YQ==");
    }

    #[test]
    fn hex_doubles_length() {
        assert_eq!(to_hex("A"), "41");
        assert_eq!(to_hex("AB").len(), 4);
    }

    #[test]
    fn score_matches_exact_case_insensitive() {
        let qs = vec![
            ("q1".to_string(), "main".to_string()),
            ("q2".to_string(), "42".to_string()),
        ];
        assert_eq!(score_answers("answer1=main\nanswer2=42", &qs), 2);
        assert_eq!(score_answers("answer1=MAIN\nanswer2=wrong", &qs), 1);
        assert_eq!(score_answers("no structure here", &qs), 0);
    }
}
