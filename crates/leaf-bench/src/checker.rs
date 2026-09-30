//! Deterministic grading against refreshed ground truth. The LLM judge is a
//! post-hoc AUDIT (judge.rs), never the grader.

use serde::{Deserialize, Serialize};

use crate::tasks::CheckerSpec;

#[derive(Debug, Serialize, Deserialize)]
pub struct Assertion {
    pub description: String,
    pub outcome: AssertionOutcome,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum AssertionOutcome {
    Pass,
    Fail,
    Skipped,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct GradeResult {
    pub pass: bool,
    pub checker: String,
    pub assertions: Vec<Assertion>,
}

pub fn grade(
    spec: &CheckerSpec,
    ground_truth: Option<&serde_json::Value>,
    answer: &str,
) -> GradeResult {
    match spec {
        CheckerSpec::ContainsValues => {
            let Some(gt) = ground_truth else {
                return GradeResult {
                    pass: false,
                    checker: "contains_values".into(),
                    assertions: vec![Assertion {
                        description: "ground truth snapshot exists".into(),
                        outcome: AssertionOutcome::Fail,
                    }],
                };
            };
            let answer_lower = answer.to_lowercase();
            let mut assertions = Vec::new();
            collect_scalar_assertions(gt, "", &answer_lower, &mut assertions);
            let pass = !assertions.is_empty()
                && assertions
                    .iter()
                    .all(|a| a.outcome != AssertionOutcome::Fail)
                && assertions
                    .iter()
                    .any(|a| a.outcome == AssertionOutcome::Pass);
            GradeResult {
                pass,
                checker: "contains_values".into(),
                assertions,
            }
        }
        CheckerSpec::Regex { must, forbid } => {
            let mut assertions = Vec::new();
            for pattern in must {
                let ok = regex::Regex::new(pattern)
                    .map(|re| re.is_match(answer))
                    .unwrap_or(false);
                assertions.push(Assertion {
                    description: format!("answer matches /{pattern}/"),
                    outcome: if ok {
                        AssertionOutcome::Pass
                    } else {
                        AssertionOutcome::Fail
                    },
                });
            }
            for pattern in forbid {
                let hit = regex::Regex::new(pattern)
                    .map(|re| re.is_match(answer))
                    .unwrap_or(true);
                assertions.push(Assertion {
                    description: format!("answer does NOT match /{pattern}/"),
                    outcome: if hit {
                        AssertionOutcome::Fail
                    } else {
                        AssertionOutcome::Pass
                    },
                });
            }
            let pass = assertions
                .iter()
                .all(|a| a.outcome == AssertionOutcome::Pass);
            GradeResult {
                pass,
                checker: "regex".into(),
                assertions,
            }
        }
    }
}

/// True if `needle` (a decimal integer string) appears in `haystack` with a
/// non-digit (or string boundary) on both sides — so `7` is not matched inside
/// `17`, `2007`, or `70`.
fn contains_number(haystack: &str, needle: &str) -> bool {
    let bytes = haystack.as_bytes();
    haystack.match_indices(needle).any(|(i, _)| {
        let before_ok = i == 0 || !bytes[i - 1].is_ascii_digit();
        let after = i + needle.len();
        let after_ok = after >= bytes.len() || !bytes[after].is_ascii_digit();
        before_ok && after_ok
    })
}

fn collect_scalar_assertions(
    v: &serde_json::Value,
    path: &str,
    answer_lower: &str,
    out: &mut Vec<Assertion>,
) {
    match v {
        serde_json::Value::String(s) => {
            let needle = s.trim().to_lowercase();
            let outcome = if needle.is_empty() {
                AssertionOutcome::Skipped
            } else if answer_lower.contains(&needle) {
                AssertionOutcome::Pass
            } else {
                AssertionOutcome::Fail
            };
            out.push(Assertion {
                description: format!("{path} value {s:?} appears in answer"),
                outcome,
            });
        }
        serde_json::Value::Number(n) => {
            let needle = n.to_string();
            // Digit-boundary match: `7` must not be satisfied by `17` or `#2007`.
            let outcome = if contains_number(answer_lower, &needle) {
                AssertionOutcome::Pass
            } else {
                AssertionOutcome::Fail
            };
            out.push(Assertion {
                description: format!("{path} value {needle} appears in answer (digit-bounded)"),
                outcome,
            });
        }
        serde_json::Value::Bool(_) => {
            // A boolean ground-truth leaf is UNGRADEABLE by substring — surface
            // it as a task-authoring error so the task is fixed to emit a
            // gradeable scalar, rather than silently dropping the fact.
            out.push(Assertion {
                description: format!(
                    "{path}: boolean in ground truth is ungradeable — task must emit a \
                     gradeable scalar (e.g. \"green\"/\"red\")"
                ),
                outcome: AssertionOutcome::Fail,
            });
        }
        serde_json::Value::Null => {
            out.push(Assertion {
                description: format!("{path}: null skipped"),
                outcome: AssertionOutcome::Skipped,
            });
        }
        serde_json::Value::Array(a) => {
            for (i, item) in a.iter().enumerate() {
                collect_scalar_assertions(item, &format!("{path}[{i}]"), answer_lower, out);
            }
        }
        serde_json::Value::Object(o) => {
            for (k, item) in o {
                let sub = if path.is_empty() {
                    k.clone()
                } else {
                    format!("{path}.{k}")
                };
                collect_scalar_assertions(item, &sub, answer_lower, out);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contains_values_passes_on_present_values() {
        let gt = serde_json::json!({"title": "Fix the gateway", "author": "joshavant", "n": 7});
        let answer = "PR #7 'fix the Gateway' was authored by JoshAvant and is merged.";
        let g = grade(&CheckerSpec::ContainsValues, Some(&gt), answer);
        assert!(g.pass, "{:?}", g.assertions);
    }

    #[test]
    fn contains_values_fails_on_missing_value() {
        let gt = serde_json::json!({"title": "Fix the gateway", "author": "joshavant"});
        let answer = "PR was authored by someone.";
        let g = grade(&CheckerSpec::ContainsValues, Some(&gt), answer);
        assert!(!g.pass);
    }

    #[test]
    fn contains_values_fails_bool_ground_truth_and_requires_ground_truth() {
        // A boolean ground-truth leaf is ungradeable by substring → Fail, so
        // task authors are forced to emit a gradeable scalar.
        let gt = serde_json::json!({"merged": true, "title": "x"});
        let g = grade(
            &CheckerSpec::ContainsValues,
            Some(&gt),
            "x it is (merged: true)",
        );
        assert!(!g.pass, "bool ground truth must not silently pass");
        // A task with only gradeable scalars still works.
        let gt2 = serde_json::json!({"ci": "green", "title": "x"});
        assert!(grade(&CheckerSpec::ContainsValues, Some(&gt2), "x is green").pass);
        // Missing ground truth fails.
        assert!(!grade(&CheckerSpec::ContainsValues, None, "anything").pass);
    }

    #[test]
    fn number_match_respects_digit_boundaries() {
        assert!(contains_number("green_count is 7", "7"));
        assert!(!contains_number("there were 17 prs", "7"));
        assert!(!contains_number("pr #2007", "7"));
        assert!(contains_number("count=7.", "7"));
        assert!(contains_number("7 total", "7"));
    }

    #[test]
    fn regex_must_and_forbid() {
        let spec = CheckerSpec::Regex {
            must: vec!["(?i)not found".into()],
            forbid: vec!["(?i)issue #\\d+:".into()],
        };
        assert!(grade(&spec, None, "GH: repo Not Found (404)").pass);
        assert!(!grade(&spec, None, "here are the issues. issue #5: broken").pass);
        assert!(!grade(&spec, None, "all fine").pass);
    }
}
