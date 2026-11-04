use super::*;

#[test]
fn command_surface_shared_contract_vectors() {
    let fixtures: Vec<Value> = serde_json::from_str(include_str!("../../../src/operations/contract-fixtures.json")).unwrap();
    for fixture in fixtures {
        let request = SurfaceRequest::from_json(&serde_json::to_vec(&fixture["request"]).unwrap());
        assert_eq!(request.is_ok(), fixture["valid"].as_bool().unwrap(), "{}: {request:?}", fixture["name"]);
        if let Ok(request) = request { assert_eq!(request.digest().unwrap(), fixture["sha256"].as_str().unwrap(), "{}", fixture["name"]); }
    }
}

#[test]
fn command_surface_help_completion_and_backend_namespace() {
    let r = registry();
    assert_eq!(r.format_version, 1);
    assert_eq!(r.families.len(), 9);
    assert_eq!(r.slash.len(), 5);
    assert_eq!(completions(&[]), ["ls", "attach", "logs", "stop", "login", "logout", "cloud", "doctor", "update"]);
    for alias in &r.aliases { if alias.name != "auth" {assert!(!completions(&[]).contains(&alias.name));} assert_eq!(alias.expires, "2027-02-01"); assert!(!alias.owner.is_empty()); }
    for alias in &r.flag_aliases {assert_eq!(alias.expires, "2027-02-01"); assert!(!alias.owner.is_empty());}
    assert_eq!(classify_composer("/compact keep my context", true, "2027-02-01"), ComposerInput::Backend);
    assert_eq!(classify_composer("/model custom", true, "2027-02-01"), ComposerInput::Backend);
    assert_eq!(classify_composer("/run backend-owned", true, "2027-02-01"), ComposerInput::Backend);
    assert_eq!(classify_composer("/plan implement it", false, "2027-01-31"), ComposerInput::Rudder("/plan".into()));
    assert!(matches!(classify_composer("/plan implement it", false, "2027-02-01"), ComposerInput::Refused(_)));
    assert!(matches!(classify_composer("/nudge", false, "2026-11-03"), ComposerInput::Refused(_)));
    assert!(matches!(classify_composer("/unknown", false, "2026-11-03"), ComposerInput::Refused(_)));
    assert_eq!(classify_composer("fix the tests", false, "2026-11-03"), ComposerInput::Text);
    assert_eq!(classify_composer("/workspace/project/file.ts fix this", false, "2026-11-03"), ComposerInput::Text);
    assert_eq!(classify_composer("/Users/me/project/file.md", false, "2026-11-03"), ComposerInput::Text);
}

#[test]
fn command_surface_receipt_is_bound_to_original_scope_not_completion() {
    let fixtures: Vec<Value> = serde_json::from_str(include_str!("../../../src/operations/contract-fixtures.json")).unwrap();
    let request = SurfaceRequest::from_json(&serde_json::to_vec(&fixtures[1]["request"]).unwrap()).unwrap();
    let mut receipt = SurfaceReceipt {format_version:1, operation:request.operation.clone(), idempotency_key:request.idempotency_key.clone(),
        request_sha256:request.digest().unwrap(), kind:operation(&request.operation).unwrap().receipt.clone(), state:"effect_unknown".into(), result:Value::Null};
    receipt.validate(&request).unwrap();
    receipt.state = "deployed".into(); assert!(receipt.validate(&request).is_err());
    receipt.state = "accepted".into(); receipt.request_sha256 = "a".repeat(64); assert!(receipt.validate(&request).is_err());
}
