//! Fixed Root-only qualification signer. No generic signing method or engine
//! operation is added. Original registration key never reaches Native/Node.
use super::*;
use super::super::runtime_image::{RootRuntimeParent, qualification::{self, Envelope, RequestBody, AckBody, NativeExportData}};
const REQUEST: &[u8] = b"rudder_runtime_image_qualification_ready_v1\0";
const RESPONSE: &[u8] = b"rudder_runtime_image_qualification_ack_v1\0";

impl RootRegistration {
    pub(in crate::root_storage_bootstrap) fn qualification_original_registration_current(&self,runtime:&RootRuntimeParent) -> Result<()> {
        runtime.original()?.remaining()?;self.completed_runtime_registration()?;self.current()?;runtime.original()?.remaining()?;Ok(())
    }
    pub(in crate::root_storage_bootstrap) fn sign_original_runtime_qualification(&self,
        runtime: &RootRuntimeParent, export: &NativeExportData) -> Result<Envelope<RequestBody>> {
        self.qualification_original_registration_current(runtime)?;
        let registration = self.completed_runtime_registration()?;
        runtime.check_original_qualification_export(export, &registration, &self.config)?;
        self.current()?;
        let selected = runtime.selected_qualification_executables_data()?;
        // Sample the SAME original enable budget immediately before signing.
        // The expired registration/startup budget is not reused or renewed.
        let remaining = u32::try_from(runtime.original()?.remaining()?.as_millis())?;
        let sdk=super::super::runtime_image::validate_enable_clock_role(&runtime.original()?.constraint(),&self.config)?;
        ensure!((1..=if sdk{240_000}else{30_000}).contains(&remaining), "qualification original remaining bound differs");
        let body = RequestBody { version: 1, kind: "runtime_image_qualification_ready".into(),
            qualification_id: export.scope.qualification_id.clone(), qualification_nonce: export.scope.qualification_nonce.clone(),
            registration_binding: registration.ack.binding.clone(), root_nonce: self.nonce.clone(),
            registration_proof_sha256: registration.ack.proof_sha256.clone(), scope: export.scope.clone(),
            observation: export.observation.clone(), selected_executables: selected, remaining_milliseconds: remaining };
        let packet = Envelope { mac: sign(&self.key.key.0, REQUEST, &body)?, body };
        ensure!(bytes(&packet)?.len() <= qualification::LIMIT, "qualification signed packet bound");
        self.current()?; runtime.original()?.remaining()?; Ok(packet)
    }
    pub(in crate::root_storage_bootstrap) fn verify_original_runtime_qualification_ack(&self,
        runtime: &RootRuntimeParent, request: &Envelope<RequestBody>, ack: &Envelope<AckBody>) -> Result<()> {
        self.qualification_original_registration_current(runtime)?;
        let registered = self.completed_runtime_registration()?;
        runtime.original()?.remaining()?;
        // Revalidate the genuine current Root owners as well as this request's
        // exact original export. A valid historical MAC alone cannot qualify.
        runtime.check_original_qualification_export(&NativeExportData { version: 1,
            kind: "original-runtime-qualification-export-v1".into(), scope: request.body.scope.clone(),
            observation: request.body.observation.clone() }, &registered, &self.config)?;
        verify(&self.key.key.0, REQUEST, &Signed { body: request.body.clone(), mac: request.mac.clone() })?;
        verify(&self.key.key.0, RESPONSE, &Signed { body: ack.body.clone(), mac: ack.mac.clone() })?;
        check_ack_data(&request.body, &ack.body)?;
        self.current()?; runtime.original()?.remaining()?; Ok(())
    }
}

// Exact DATA checks are separately testable; this alone issues no owner or ACK.
fn check_ack_data(request: &RequestBody, ack: &AckBody) -> Result<()> {
    ensure!(request.version == 1 && request.kind == "runtime_image_qualification_ready"
        && (1..=30_000).contains(&request.remaining_milliseconds)
        && ack.version == 1 && ack.kind == "runtime_image_qualification_ack"
        && ack.qualification_id == request.qualification_id && ack.qualification_nonce == request.qualification_nonce
        && ack.request_sha256 == hex(&Sha256::digest(bytes(request)?))
        && ack.registration_proof_sha256 == request.registration_proof_sha256
        && serde_json::to_value(&ack.original_enable_constraint)? == request.observation["original_enable_constraint"],
        "qualification ACK differs from original request");
    ensure!(ack.result.is_some() != ack.error.is_some(), "qualification ACK result/error shape differs");
    if let Some(error) = &ack.error {
        ensure!(["invalid_request", "unconfigured", "identity_changed", "expired", "conflict", "busy", "effect_unknown", "unavailable"].contains(&error.as_str()), "qualification ACK error unsupported");
        anyhow::bail!("qualification did not publish; original outcome retained");
    }
    let result = ack.result.as_ref().context("qualification actual published result absent")?;
    unhex::<32>(result.role_binding_sha256.as_bytes())?;
    ensure!(result.scope == "immutable-image-runtime-role-only" && result.state == "published"
        && result.image == request.scope.image && result.inventory == request.scope.inventory
        && result.manifest == request.scope.manifest && result.installer_input == request.scope.installer_input,
        "qualification published role differs from original immutable image"); Ok(())
}

#[cfg(test)]
#[path="root_registration_qualification_tests.rs"]
mod tests;

#[path="root_registration_compiler.rs"]
mod compiler_sdk;
