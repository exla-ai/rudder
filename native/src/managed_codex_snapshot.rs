//! One selected Codex OAuth ACCESS token. JWT claims are refusal/deadline DATA,
//! never a signature check, refresh lease, server identity or auth receipt.
use anyhow::{ensure, Result};
use serde::Deserialize;

const TOKEN_LIMIT: usize = 48 * 1024;
const SNAPSHOT_LIMIT: usize = 65532;
const MAX_FUTURE_SECONDS: u64 = 24 * 60 * 60;

#[cfg(test)]
thread_local! {static ERASED:std::cell::Cell<usize>=const {std::cell::Cell::new(0)};}

#[derive(Deserialize)]
#[serde(transparent)]
struct Secret(String);
impl Drop for Secret {
    fn drop(&mut self) {
        for byte in unsafe { self.0.as_bytes_mut() } { unsafe { std::ptr::write_volatile(byte, 0); } }
        std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
        #[cfg(test)] ERASED.with(|count|count.set(count.get()+usize::from(self.0.as_bytes().iter().all(|byte|*byte==0))));
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all="camelCase")]
struct Wire {
    format_version: u32, kind: String, profile_id: String, backend: String, provider: String,
    access_token: Secret, chatgpt_account_id: String, expires_at_ms: u64,
    chatgpt_plan_type: Option<String>,
}
#[derive(Deserialize)]
struct Claims {
    exp: u64,
    #[serde(rename="https://api.openai.com/auth")]
    auth: AccountClaims,
}
#[derive(Deserialize)]
struct AccountClaims { chatgpt_account_id: String, chatgpt_plan_type: Option<String> }

/// No Clone, Debug or Serialize: only the verified original host auth connection
/// may borrow these bytes. This owner contains no refresh token or ID token.
pub(crate) struct CodexOAuthAccessSnapshotV1 {
    profile:String, access: Secret, account: String, plan: Option<String>, expires_at_ms: u64,
}
fn text(value: &str, limit: usize) -> bool {
    !value.is_empty() && value.len() <= limit && !value.chars().any(char::is_control)
}
struct ErasedBytes(Vec<u8>);
impl Drop for ErasedBytes {fn drop(&mut self) {
    for byte in &mut self.0 {unsafe {std::ptr::write_volatile(byte,0);}}
    std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
}}
fn decode(segment: &str) -> Result<Secret> {
    ensure!(!segment.is_empty() && segment.len() <= (16*1024*4+2)/3 && segment.len() % 4 != 1, "invalid selected Codex access snapshot");
    let mut bytes = ErasedBytes(Vec::with_capacity(segment.len() * 3 / 4));
    let (mut bits, mut count) = (0u32, 0u32);
    for byte in segment.bytes() {
        let value = match byte { b'A'..=b'Z'=>byte-b'A',b'a'..=b'z'=>byte-b'a'+26,b'0'..=b'9'=>byte-b'0'+52,b'-'=>62,b'_'=>63,
            _=>anyhow::bail!("invalid selected Codex access snapshot") };
        bits = (bits << 6) | value as u32; count += 6;
        if count >= 8 { count -= 8; bytes.0.push((bits >> count) as u8); bits &= (1 << count) - 1; }
    }
    ensure!(bytes.0.len() <= 16*1024 && bits == 0, "invalid selected Codex access snapshot");
    // Decode directly into a zeroizing owner before JSON can fail.
    match String::from_utf8(std::mem::take(&mut bytes.0)) { Ok(text)=>Ok(Secret(text)), Err(error)=>{
        let _bytes=ErasedBytes(error.into_bytes()); anyhow::bail!("invalid selected Codex access snapshot") } }
}
impl CodexOAuthAccessSnapshotV1 {
    pub(crate) fn from_frame(frame: &[u8], selected_profile: &str, now_ms: u64) -> Result<Self> {
        ensure!(frame.len() >= 4, "invalid selected Codex access snapshot");
        let size=u32::from_be_bytes(frame[..4].try_into().unwrap()) as usize;
        ensure!(size > 0 && size <= SNAPSHOT_LIMIT && frame.len()==size+4, "invalid selected Codex access snapshot");
        let wire: Wire=serde_json::from_slice(&frame[4..]).map_err(|_|anyhow::anyhow!("invalid selected Codex access snapshot"))?;
        ensure!(wire.format_version==1 && wire.kind=="CodexOAuthAccessSnapshotV1" && wire.profile_id==selected_profile
            && wire.backend=="codex" && wire.provider=="openai-codex" && text(selected_profile,256)
            && text(&wire.chatgpt_account_id,256) && wire.chatgpt_plan_type.as_deref().is_none_or(|p|text(p,128))
            && text(&wire.access_token.0,TOKEN_LIMIT), "invalid selected Codex access snapshot");
        let mut parts=wire.access_token.0.split('.');
        let header=parts.next().unwrap_or(""); let payload=parts.next().unwrap_or(""); let signature=parts.next().unwrap_or("");
        ensure!(parts.next().is_none() && !header.is_empty() && !signature.is_empty()
            && header.bytes().chain(signature.bytes()).all(|b|b.is_ascii_alphanumeric()||b==b'-'||b==b'_'), "invalid selected Codex access snapshot");
        let decoded=decode(payload)?;
        let claims: Claims=serde_json::from_str(&decoded.0).map_err(|_|anyhow::anyhow!("invalid selected Codex access snapshot"))?;
        let expiry=claims.exp.checked_mul(1000).ok_or_else(||anyhow::anyhow!("invalid selected Codex access snapshot"))?;
        ensure!(expiry==wire.expires_at_ms && expiry > now_ms && expiry-now_ms <= MAX_FUTURE_SECONDS*1000
            && claims.auth.chatgpt_account_id==wire.chatgpt_account_id && claims.auth.chatgpt_plan_type==wire.chatgpt_plan_type,
            "invalid selected Codex access snapshot");
        Ok(Self {profile:wire.profile_id,access:wire.access_token,account:wire.chatgpt_account_id,plan:wire.chatgpt_plan_type,expires_at_ms:expiry})
    }
    pub(crate) fn profile(&self)->&str {&self.profile}
    pub(crate) fn access(&self)->&str { &self.access.0 }
    pub(crate) fn account(&self)->&str { &self.account }
    pub(crate) fn plan(&self)->Option<&str> { self.plan.as_deref() }
    pub(crate) fn expires_at_ms(&self)->u64 { self.expires_at_ms }
}

#[cfg(test)]
#[path="managed_codex_snapshot_tests.rs"]
mod tests;
