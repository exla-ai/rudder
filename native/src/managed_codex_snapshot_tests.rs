use super::*;
use serde_json::{json,Value};
const NOW:u64=1_800_000_000_000;
fn b64(bytes:&[u8])->String {
    const T:&[u8]=b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out=String::new();let(mut bits,mut count)=(0u32,0u32);
    for b in bytes {bits=(bits<<8)|*b as u32;count+=8;while count>=6 {count-=6;out.push(T[((bits>>count)&63)as usize]as char);}}
    if count>0 {out.push(T[((bits<<(6-count))&63)as usize]as char);}out
}
fn claims()->Value {json!({"exp":NOW/1000+3600,"https://api.openai.com/auth":{"chatgpt_account_id":"account-fixture","chatgpt_plan_type":"plus"}})}
fn token(claims:Value)->String {format!("e30.{}.c2ln",b64(claims.to_string().as_bytes()))}
fn wire()->Value {json!({"formatVersion":1,"kind":"CodexOAuthAccessSnapshotV1","profileId":"selected","backend":"codex","provider":"openai-codex",
    "accessToken":token(claims()),"chatgptAccountId":"account-fixture","expiresAtMs":NOW+3_600_000,"chatgptPlanType":"plus"})}
fn frame(value:Value)->Vec<u8> {let bytes=value.to_string().into_bytes();let mut frame=(bytes.len()as u32).to_be_bytes().to_vec();frame.extend(bytes);frame}
fn refuses(wire:Value) {assert!(CodexOAuthAccessSnapshotV1::from_frame(&frame(wire),"selected",NOW).is_err());}
#[test]
fn selected_snapshot_binds_access_account_plan_and_actual_expiry_and_erases_owner() {
    let before=ERASED.with(|count|count.get());
    let owner=CodexOAuthAccessSnapshotV1::from_frame(&frame(wire()),"selected",NOW).unwrap();
    assert_eq!(owner.account(),"account-fixture");assert_eq!(owner.plan(),Some("plus"));assert_eq!(owner.expires_at_ms(),NOW+3_600_000);
    assert!(owner.access().starts_with("e30."));drop(owner);
    assert!(ERASED.with(|count|count.get())>=before+2,"decoded claims and original access owner erased");
}
#[test]
fn closed_snapshot_refuses_unknown_refresh_id_token_duplicate_and_foreign_profile() {
    for name in ["refreshToken","idToken","lease","expires","accountId"] {let mut w=wire();w[name]=json!("forbidden");refuses(w);}
    let w=frame(wire());assert!(CodexOAuthAccessSnapshotV1::from_frame(&w,"another",NOW).is_err());
    let raw=wire().to_string().replacen('{',"{\"profileId\":\"selected\",",1);let mut w=(raw.len()as u32).to_be_bytes().to_vec();w.extend(raw.bytes());
    assert!(CodexOAuthAccessSnapshotV1::from_frame(&w,"selected",NOW).is_err());
}
#[test]
fn jwt_expiry_is_exact_bounded_data_and_never_file_mtime() {
    for exp in [json!(null),json!(true),json!(1.5),json!(-1),json!(NOW/1000),json!(u64::MAX),json!(NOW/1000+MAX_FUTURE_SECONDS+1)] {
        let mut c=claims();c["exp"]=exp;let mut w=wire();w["accessToken"]=json!(token(c));refuses(w);
    }
    let mut w=wire();w["expiresAtMs"]=json!(NOW+3_600_001);refuses(w);
    let mut c=claims();c.as_object_mut().unwrap().remove("exp");let mut w=wire();w["accessToken"]=json!(token(c));refuses(w);
}
#[test]
fn multiday_access_preserves_exact_jwt_expiry_with_fourteen_day_sanity_bound() {
    for seconds in [53*60*60,14*24*60*60] {
        let expiry=NOW+seconds*1000;
        let mut c=claims();c["exp"]=json!(expiry/1000);
        let mut w=wire();w["accessToken"]=json!(token(c));w["expiresAtMs"]=json!(expiry);
        let snapshot=CodexOAuthAccessSnapshotV1::from_frame(&frame(w.clone()),"selected",NOW).unwrap();
        assert_eq!(snapshot.expires_at_ms(),expiry);
        assert_eq!(snapshot.account(),"account-fixture");
        w["expiresAtMs"]=json!(expiry+1);refuses(w);
    }
    let expiry=NOW+(14*24*60*60+1)*1000;
    let mut c=claims();c["exp"]=json!(expiry/1000);
    let mut w=wire();w["accessToken"]=json!(token(c));w["expiresAtMs"]=json!(expiry);
    refuses(w);
}
#[test]
fn jwt_selected_account_and_optional_plan_cannot_be_substituted() {
    for (name,value) in [("chatgptAccountId",json!("other")),("chatgptPlanType",json!("pro")),("provider",json!("openai")),("backend",json!("claude")),("kind",json!("OAuthV1"))] {
        let mut w=wire();w[name]=value;refuses(w);
    }
    let mut c=claims();c["https://api.openai.com/auth"].as_object_mut().unwrap().remove("chatgpt_plan_type");
    let mut w=wire();w["accessToken"]=json!(token(c));w.as_object_mut().unwrap().remove("chatgptPlanType");
    assert!(CodexOAuthAccessSnapshotV1::from_frame(&frame(w),"selected",NOW).unwrap().plan().is_none());
}
#[test]
fn handoff_refuses_partial_trailing_oversize_and_malformed_token_frames() {
    let good=frame(wire());for size in 0..good.len() {assert!(CodexOAuthAccessSnapshotV1::from_frame(&good[..size],"selected",NOW).is_err());}
    let mut trailing=good;trailing.push(0);assert!(CodexOAuthAccessSnapshotV1::from_frame(&trailing,"selected",NOW).is_err());
    for bad in ["e30.e30=.sig","e30.?.sig","e30.e30.sig.extra","e30.e30.\n","e30.Z.sig"] {let mut w=wire();w["accessToken"]=json!(bad);refuses(w);}
    let mut w=wire();w["accessToken"]=json!("x".repeat(TOKEN_LIMIT+1));refuses(w);
    let mut c=claims();c["padding"]=json!("x".repeat(16*1024));let mut w=wire();w["accessToken"]=json!(token(c));refuses(w);
}
