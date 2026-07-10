//! B8: the shipped example self-update package MUST be an
//! ApplicationDescription valid per margo-package
//! (spec/reeve/08-packaging.md §10.5), and its vendored update
//! descriptor MUST parse as the agent's `AgentUpdateSpec` — the two
//! halves of the "agent as a workload" contract.

use std::path::PathBuf;

fn package_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../deploy/packages/reeve-agent")
}

#[test]
fn example_package_validates_via_margo_package() {
    let pkg = margo_package::Package::load_dir(package_root())
        .expect("deploy/packages/reeve-agent must load without error-severity findings");
    assert!(
        pkg.warnings.is_empty(),
        "example package must be warning-clean: {:?}",
        pkg.warnings
    );
    assert_eq!(pkg.description.effective_id(), Some("reeve-agent"));
    assert_eq!(pkg.description.metadata.name, "reeve-agent");
    let profile = &pkg.description.deployment_profiles[0];
    assert_eq!(profile.profile_type, "compose");
    assert_eq!(profile.components[0].name, "reeve-agent");
    // The reeve extension property resolves to a real vendored file.
    let link = profile.components[0]
        .properties
        .as_ref()
        .and_then(|p| p.get("reeveAgentUpdate"))
        .and_then(|v| v.as_str())
        .expect("component carries the reeveAgentUpdate extension property");
    let resolved = pkg
        .resource_path(link)
        .expect("update descriptor link must stay inside the package");
    assert!(resolved.is_file(), "{} must exist", resolved.display());
}

#[test]
fn vendored_update_descriptor_parses_as_agent_update_spec() {
    let yaml =
        std::fs::read_to_string(package_root().join("resources/agent-update.yaml")).unwrap();
    let spec: reeve_agent::update::AgentUpdateSpec = serde_yaml_ng::from_str(&yaml)
        .expect("resources/agent-update.yaml must parse as AgentUpdateSpec");
    assert!(!spec.version.is_empty());
    assert!(
        reeve_types::reeve::manifest::is_sha256_digest(&spec.binary.digest),
        "digest must follow the sha256:<hex> grammar"
    );
}
