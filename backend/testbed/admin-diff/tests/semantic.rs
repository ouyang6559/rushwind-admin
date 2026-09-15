//! Semantic comparator and envelope-judge unit tests.
//!
//! The normalization rules and the unordered-map/ordered-array walk are the
//! core of every future data-plane differential; these pin them without a
//! backend. The envelope tests pin the exact-vs-shape split.

use admin_diff::compare::{judge, semantic_diffs, Kind, Verdict};
use admin_diff::ProbeResult;

fn up(status: u16, body: &str) -> ProbeResult {
    ProbeResult {
        reachable: true,
        status: Some(status),
        body: Some(body.as_bytes().to_vec()),
        cors: None,
    }
}

#[test]
fn map_order_is_irrelevant() {
    let a = serde_json::json!({"x": 1, "y": {"b": 2, "a": 3}});
    let b = serde_json::json!({"y": {"a": 3, "b": 2}, "x": 1});
    let mut diffs = Vec::new();
    semantic_diffs(&a, &b, "$", &mut diffs);
    assert!(diffs.is_empty());
}

#[test]
fn identity_and_timestamp_keys_are_normalized() {
    let a = serde_json::json!({"id": 1, "uid": "abc", "created_by": "x", "created_at": "t1", "name": "n"});
    let b = serde_json::json!({"id": 999, "uid": "zzz", "created_by": "q", "created_at": "t2", "name": "n"});
    let mut diffs = Vec::new();
    semantic_diffs(&a, &b, "$", &mut diffs);
    assert!(diffs.is_empty());
}

#[test]
fn uuid_and_timestamp_values_are_normalized() {
    let a = serde_json::json!({"token": "123e4567-e89b-12d3-a456-426614174000", "seen": "2024-01-01T00:00:00Z"});
    let b = serde_json::json!({"token": "00000000-0000-0000-0000-000000000000", "seen": "2029-12-31T23:59:59Z"});
    let mut diffs = Vec::new();
    semantic_diffs(&a, &b, "$", &mut diffs);
    assert!(diffs.is_empty());
}

#[test]
fn real_value_divergence_is_detected() {
    let a = serde_json::json!({"name": "alpha", "items": [1, 2, 3]});
    let b = serde_json::json!({"name": "beta", "items": [1, 2, 3]});
    let mut diffs = Vec::new();
    semantic_diffs(&a, &b, "$", &mut diffs);
    assert_eq!(diffs, vec!["$.name: value diverged".to_string()]);
}

#[test]
fn array_order_is_enforced() {
    let a = serde_json::json!([1, 2, 3]);
    let b = serde_json::json!([3, 2, 1]);
    let mut diffs = Vec::new();
    semantic_diffs(&a, &b, "$", &mut diffs);
    assert_eq!(
        diffs,
        vec![
            "$[0]: value diverged".to_string(),
            "$[2]: value diverged".to_string()
        ]
    );
}

#[test]
fn missing_key_is_detected() {
    let a = serde_json::json!({"x": 1, "y": 2});
    let b = serde_json::json!({"x": 1});
    let mut diffs = Vec::new();
    semantic_diffs(&a, &b, "$", &mut diffs);
    assert_eq!(diffs, vec!["$.y: missing on rust side".to_string()]);
}

#[test]
fn envelope_exact_pins_bytes() {
    let body =
        r#"{"code":401,"reason":"UNAUTHORIZED","message":"missing bearer token","metadata":{}}"#;
    let (ok, _) = judge(
        Kind::EnvelopeExact,
        "probe",
        &up(401, body),
        &up(401, body),
        &Default::default(),
    );
    assert_eq!(ok, Verdict::Ok);
    let (bad, detail) = judge(
        Kind::EnvelopeExact,
        "probe",
        &up(401, body),
        &up(
            401,
            r#"{"code":401,"reason":"UNAUTHORIZED","message":"drifted","metadata":{}}"#,
        ),
        &Default::default(),
    );
    assert_eq!(bad, Verdict::Fail);
    assert!(detail.unwrap().contains("bytes diverged"));
}

#[test]
fn envelope_shape_ignores_message_text() {
    let (ok, _) = judge(
        Kind::EnvelopeShape,
        "probe",
        &up(
            400,
            r#"{"code":400,"reason":"CODEC","message":"go parser prose","metadata":{}}"#,
        ),
        &up(
            400,
            r#"{"code":400,"reason":"CODEC","message":"rust parser prose","metadata":{}}"#,
        ),
        &Default::default(),
    );
    assert_eq!(ok, Verdict::Ok);
    let (bad, _) = judge(
        Kind::EnvelopeShape,
        "probe",
        &up(
            400,
            r#"{"code":400,"reason":"CODEC","message":"x","metadata":{}}"#,
        ),
        &up(
            400,
            r#"{"code":400,"reason":"ABUSE","message":"x","metadata":{}}"#,
        ),
        &Default::default(),
    );
    assert_eq!(bad, Verdict::Fail);
}

#[test]
fn exemption_class_short_circuits() {
    let mut exemptions = std::collections::HashMap::new();
    exemptions.insert("head-on-get".to_string(), "recorded divergence".to_string());
    let (verdict, detail) = judge(
        Kind::Routing,
        "head-on-get",
        &up(405, ""),
        &up(401, ""),
        &exemptions,
    );
    assert_eq!(verdict, Verdict::Exempt);
    assert_eq!(detail.as_deref(), Some("recorded divergence"));
}

#[test]
fn unreachable_short_circuits() {
    let (verdict, detail) = judge(
        Kind::EnvelopeExact,
        "probe",
        &ProbeResult::down(),
        &up(401, "x"),
        &Default::default(),
    );
    assert_eq!(verdict, Verdict::Unreachable);
    assert!(detail.is_none());
}
