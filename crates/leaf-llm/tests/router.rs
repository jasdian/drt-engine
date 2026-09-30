//! LlmRouter tests against a canned local HTTP server (no network, no key)
//! plus one #[ignore]d live smoke test.

use leaf_core::{RouteRequest, Router};
use leaf_llm::{EpsilonGreedy, LlmClient, LlmRouter, Provider, SONNET_4_6};
use leaf_traits::CandidateInfo;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// One-shot HTTP/1.1 mock: serves the given JSON bodies (in order, one per
/// connection) on an ephemeral port. Returns (base_url, join_handle).
async fn mock_server(bodies: Vec<String>) -> (String, tokio::task::JoinHandle<u32>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        let mut served = 0u32;
        for body in bodies {
            let (mut sock, _) = match listener.accept().await {
                Ok(x) => x,
                Err(_) => break,
            };
            // Read the request until the end of headers + declared body length.
            let mut buf = vec![0u8; 65536];
            let mut read = 0usize;
            loop {
                let n = sock.read(&mut buf[read..]).await.unwrap_or(0);
                if n == 0 {
                    break;
                }
                read += n;
                let text = String::from_utf8_lossy(&buf[..read]);
                if let Some(header_end) = text.find("\r\n\r\n") {
                    let content_length = text
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                        })
                        .unwrap_or(0);
                    if read >= header_end + 4 + content_length {
                        break;
                    }
                }
            }
            let resp = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = sock.write_all(resp.as_bytes()).await;
            let _ = sock.shutdown().await;
            served += 1;
        }
        served
    });
    (format!("http://{addr}"), handle)
}

fn answer_body(text: &str, input_tokens: u64, output_tokens: u64) -> String {
    serde_json::json!({
        "content": [{"type": "text", "text": text}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": input_tokens, "output_tokens": output_tokens},
        "model": "claude-sonnet-4-6"
    })
    .to_string()
}

fn candidates() -> Vec<CandidateInfo> {
    vec![
        CandidateInfo {
            id: "c0".into(),
            name: "jq".into(),
            description: "run jq".into(),
        },
        CandidateInfo {
            id: "c1".into(),
            name: "done".into(),
            description: "finish".into(),
        },
    ]
}

fn request<'a>(cands: &'a [CandidateInfo], digest: &'a str) -> RouteRequest<'a> {
    RouteRequest {
        goal: "pick the next step",
        candidates: cands,
        rendered_context: "input: task\n",
        rendered_menu: "c0 jq — run jq\nc1 done — finish\n",
        prompt_digest: digest,
        decision_index: 0,
    }
}

#[tokio::test]
async fn parses_model_answer_and_records_usage() {
    let (url, _h) = mock_server(vec![answer_body("c1", 120, 3)]).await;
    let client = LlmClient::new(
        Provider::Anthropic,
        "test-key".into(),
        "claude-sonnet-4-6".into(),
    )
    .with_base_url(url);
    // epsilon 0: never explore, always call the LLM.
    let mut router = LlmRouter::new(client, EpsilonGreedy::new(1, 0), SONNET_4_6);
    let cands = candidates();
    let d = router.route(request(&cands, "digest")).await.unwrap();
    assert_eq!(d.chosen, 1);
    let llm = d.llm.expect("non-explored decision carries an llm record");
    assert_eq!(llm.response_text, "c1");
    assert_eq!(llm.input_tokens, 120);
    assert_eq!(llm.output_tokens, 3);
    // 120 in × $3/M + 3 out × $15/M = 360 + 45 = 405 µUSD... at integer µUSD
    // per-token this comes out via the MTok table:
    assert_eq!(llm.cost_microusd, 360 + 45);
    assert!(!llm.parse_fallback);
    assert_eq!(llm.request_digest.len(), 64);
    assert!(!d.exploration.unwrap().explored);
}

#[tokio::test]
async fn garbage_answer_reasks_then_falls_back_deterministically() {
    let (url, handle) = mock_server(vec![
        answer_body("I think the best choice is jq", 100, 8),
        answer_body("definitely jq!", 110, 8),
    ])
    .await;
    let client = LlmClient::new(
        Provider::Anthropic,
        "test-key".into(),
        "claude-sonnet-4-6".into(),
    )
    .with_base_url(url);
    let mut router = LlmRouter::new(client, EpsilonGreedy::new(1, 0), SONNET_4_6);
    let cands = candidates();
    let d = router.route(request(&cands, "digest")).await.unwrap();
    // Deterministic fallback: candidate 0, flagged.
    assert_eq!(d.chosen, 0);
    let llm = d.llm.unwrap();
    assert!(llm.parse_fallback);
    // Usage of BOTH attempts accumulated.
    assert_eq!(llm.input_tokens, 210);
    assert_eq!(llm.output_tokens, 16);
    assert_eq!(
        handle.await.unwrap(),
        2,
        "exactly two HTTP calls (ask + re-ask)"
    );
}

#[tokio::test]
async fn explored_decision_skips_the_llm_entirely() {
    // No mock server at all: any HTTP attempt would fail the test.
    let client = LlmClient::new(
        Provider::Anthropic,
        "test-key".into(),
        "claude-sonnet-4-6".into(),
    )
    .with_base_url("http://127.0.0.1:1"); // poison
    // epsilon 100%: always explore.
    let mut router = LlmRouter::new(client, EpsilonGreedy::new(7, 1_000_000), SONNET_4_6);
    let cands = candidates();
    let d = router.route(request(&cands, "digest")).await.unwrap();
    assert!(
        d.llm.is_none(),
        "explored decisions must not bill an LLM call"
    );
    let rec = d.exploration.unwrap();
    assert!(rec.explored);
    assert_eq!(d.chosen as u64, rec.pick.unwrap() % 2);
}

#[tokio::test]
async fn same_seed_same_exploration_choices() {
    // Acceptance criterion 3, router-level: two routers with the same seed
    // pick identical candidates with no LLM involved.
    let cands = candidates();
    let mut picks_a = Vec::new();
    let mut picks_b = Vec::new();
    for picks in [&mut picks_a, &mut picks_b] {
        let client = LlmClient::new(Provider::Anthropic, "k".into(), "claude-sonnet-4-6".into())
            .with_base_url("http://127.0.0.1:1");
        let mut router = LlmRouter::new(client, EpsilonGreedy::new(99, 1_000_000), SONNET_4_6);
        for _ in 0..10 {
            picks.push(router.route(request(&cands, "d")).await.unwrap().chosen);
        }
    }
    assert_eq!(picks_a, picks_b);
}

/// Live smoke test — run with:
///   ANTHROPIC_API_KEY=... cargo test -p leaf-llm -- --ignored
#[tokio::test]
#[ignore = "requires ANTHROPIC_API_KEY and network; bills a fraction of a cent"]
async fn live_route_and_count_tokens() {
    let key = std::env::var("ANTHROPIC_API_KEY").expect("set ANTHROPIC_API_KEY");
    let client = LlmClient::new(Provider::Anthropic, key, "claude-sonnet-4-6".into());
    let mut router = LlmRouter::new(client, EpsilonGreedy::new(1, 0), SONNET_4_6);
    let cands = vec![
        CandidateInfo {
            id: "c0".into(),
            name: "read_file".into(),
            description: "read the target file from disk".into(),
        },
        CandidateInfo {
            id: "c1".into(),
            name: "done".into(),
            description: "the task is already complete".into(),
        },
    ];
    let req = RouteRequest {
        goal: "The task: read config.json and report its version field. Nothing has been done yet.",
        candidates: &cands,
        rendered_context: "input: read config.json and report its version field\n",
        rendered_menu: "c0 read_file — read the target file from disk\nc1 done — the task is already complete\n",
        prompt_digest: "live",
        decision_index: 0,
    };
    let tokens = router.count_route_tokens(&req).await.unwrap();
    assert!(
        tokens > 50,
        "routing prompt should be non-trivial, got {tokens}"
    );
    let d = router.route(req).await.unwrap();
    assert_eq!(d.chosen, 0, "a sensible router reads the file first");
    let llm = d.llm.unwrap();
    assert!(llm.cost_microusd > 0);
    println!(
        "live: {} prompt tokens, answer '{}', cost {} µUSD",
        tokens, llm.response_text, llm.cost_microusd
    );
}
