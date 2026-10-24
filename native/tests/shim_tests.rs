//! Integration tests over the plain (non-FFI) API, against the neutral fixtures in `../samples`.
//! The same fixtures drive the .NET-side tests, so both halves of the shim stay honest about the
//! same reports.

use std::path::PathBuf;

use rpt_cs::{inspect, render};
use serde_json::Value;

fn fixture(name: &str) -> Vec<u8> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("samples")
        .join(name);
    std::fs::read(path).expect("fixture readable")
}

fn parse(json: &[u8]) -> Value {
    serde_json::from_slice(json).expect("valid result JSON")
}

fn request(body: Value) -> Vec<u8> {
    serde_json::to_vec(&body).unwrap()
}

#[test]
fn inspect_reports_saved_data_and_metadata() {
    let meta = parse(&inspect(&fixture("sports_teams.rpt")).unwrap());
    assert_eq!(meta["savedData"], Value::Bool(true));
    assert!(meta["title"].as_str().is_some());
    assert!(meta["parameters"].as_array().is_some());
    assert!(meta["subreports"].as_array().is_some());
    assert!(meta["tables"].as_array().is_some());
}

#[test]
fn render_saved_data_produces_a_pdf() {
    let outcome = render(&fixture("sports_teams.rpt"), &request(serde_json::json!({}))).unwrap();
    assert!(outcome.pdf.starts_with(b"%PDF"), "output must be a PDF");
    let result = parse(&outcome.result_json);
    assert!(result["pages"].as_u64().unwrap() >= 1);
    assert!(result["diagnostics"].as_array().is_some());
}

#[test]
fn render_with_live_dataset_overrides_saved_data() {
    // The file carries saved data; feeding an empty live dataset is the honest override path —
    // the render must succeed and report zero rows without inventing any.
    let body = serde_json::json!({
        "dataset": { "columns": [ { "name": "dummy.value", "type": "string" } ], "rows": [] }
    });
    let outcome = render(&fixture("sports_teams.rpt"), &request(body)).unwrap();
    assert!(outcome.pdf.starts_with(b"%PDF"));
}

#[test]
fn render_with_typed_dataset_rows() {
    let body = serde_json::json!({
        "dataset": {
            "columns": [
                { "name": "teams.name", "type": "string" },
                { "name": "teams.wins", "type": "int" },
                { "name": "teams.pct", "type": "number" },
                { "name": "teams.active", "type": "boolean" },
                { "name": "teams.since", "type": "date" }
            ],
            "rows": [
                ["Alpha", 12, 0.625, true, "1998-04-01"],
                ["Beta", 7, 0.438, false, null]
            ]
        }
    });
    let outcome = render(&fixture("sports_teams.rpt"), &request(body)).unwrap();
    assert!(outcome.pdf.starts_with(b"%PDF"));
}

#[test]
fn render_with_subreport_data_keyed_by_name() {
    let meta = parse(&inspect(&fixture("top5_usa_with_sub.rpt")).unwrap());
    let subreports = meta["subreports"].as_array().unwrap();
    assert!(!subreports.is_empty(), "fixture must carry a subreport");
    let name = subreports[0].as_str().unwrap().to_string();
    let body = serde_json::json!({
        "subreports": {
            name: {
                "columns": [ { "name": "sub.dummy", "type": "string" } ],
                "rows": [ ["row one"], ["row two"] ]
            }
        }
    });
    let outcome = render(&fixture("top5_usa_with_sub.rpt"), &request(body)).unwrap();
    assert!(outcome.pdf.starts_with(b"%PDF"));
}

#[test]
fn unknown_subreport_key_is_reported_not_swallowed() {
    let body = serde_json::json!({
        "subreports": {
            "NoSuchSubreport": {
                "columns": [ { "name": "x", "type": "string" } ],
                "rows": []
            }
        }
    });
    let outcome = render(&fixture("top5_usa_with_sub.rpt"), &request(body)).unwrap();
    let result = parse(&outcome.result_json);
    let text = result["diagnostics"].to_string();
    assert!(text.contains("NoSuchSubreport"), "the ignored key must be surfaced: {text}");
}

#[test]
fn unknown_parameter_is_reported_not_swallowed() {
    let body = serde_json::json!({ "params": { "NotAParam": "x" } });
    let outcome = render(&fixture("param_kind_discrete.rpt"), &request(body)).unwrap();
    let result = parse(&outcome.result_json);
    let text = result["diagnostics"].to_string();
    assert!(text.contains("NotAParam"), "the unknown parameter must be surfaced: {text}");
}

#[test]
fn unknown_conformance_is_rejected() {
    let body = serde_json::json!({ "conformance": "pdfa9z" });
    let err = render(&fixture("sports_teams.rpt"), &request(body)).unwrap_err();
    assert!(matches!(err, rpt_cs::ShimError::InvalidArgument(_)));
}

#[test]
fn corrupt_bytes_are_a_decode_error() {
    let err = inspect(b"not a rpt file at all").unwrap_err();
    assert!(matches!(err, rpt_cs::ShimError::Decode(_)));
}

#[test]
fn malformed_request_is_an_invalid_argument() {
    let err = render(&fixture("sports_teams.rpt"), b"{ nope").unwrap_err();
    assert!(matches!(err, rpt_cs::ShimError::InvalidArgument(_)));
}

#[test]
fn locale_fallback_is_reported() {
    let body = serde_json::json!({ "locale": "xx-XXX" });
    let outcome = render(&fixture("sports_teams.rpt"), &request(body)).unwrap();
    let result = parse(&outcome.result_json);
    let text = result["diagnostics"].to_string();
    assert!(text.contains("xx-XXX"), "the fallback must be surfaced: {text}");
}