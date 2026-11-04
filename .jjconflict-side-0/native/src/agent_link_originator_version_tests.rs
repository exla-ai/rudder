//! Exact actual-Codex interoperability regression. Compatibility metadata is
//! separate from the existing socket/process/thread provenance guards.
use super::*;

#[test]
fn process_wide_first_originator_does_not_disable_second_client_controls() {
    let actual = "rudder_fixture_frontend/0.166.0 (Debian 12.0.0; x86_64) dumb (rudder_agent_link; 0.1.0)";
    assert!(codex_peer_controls_supported(actual));
    assert!(codex_peer_controls_supported("codex_cli_rs/0.166.0 (Linux; x86_64)"));
    assert!(codex_peer_controls_supported("other/0.166.0"));
    assert!(codex_peer_controls_supported("rudder_agent_link/0.167.0 (macOS; arm64)"));
}

#[test]
fn only_the_server_product_version_can_meet_the_existing_minimum() {
    for value in [
        "other/0.165.9 (Linux) (rudder_agent_link; 0.166.0)",
        "other/unknown (Linux) (rudder_agent_link; 0.166.0)",
        "unknown (rudder_agent_link/0.166.0)",
        "other/0.166.0-preview (rudder_agent_link; 1.0.0)",
        "other/0.166 (rudder_agent_link; 1.0.0)",
        "other/0.166.0.1",
        "other/0.166.0/extra",
        "other/18446744073709551616.166.0",
        "/0.166.0",
        "other/",
        " other/0.166.0",
        "other/ 0.166.0",
        "\0other/0.166.0",
        "",
    ] {
        assert!(!codex_peer_controls_supported(value), "{value:?}");
    }
}

#[test]
fn client_suffix_is_not_authorization_or_a_required_identity_label() {
    assert!(codex_peer_controls_supported("different_frontend/0.166.0 (Linux) (unrelated_client; 0.1.0)"));
    assert!(!codex_peer_controls_supported("different_frontend/0.165.9 (Linux) (rudder_agent_link; 99.0.0)"));
}
