//! Deployment scope (spec/reeve/11-fleet-model.md §11.4): the
//! operator-facing target of a deploy or a rollout — the hierarchy level
//! a stack is deployed to, never a numbered layer path.
//!
//! A scope maps to one or more authoring layer dirs (§11.1 taxonomy:
//! `00-all` / `10-fleet.<name>` / `20-site.<name>` / `30-type.<name>` /
//! `40-device.<id>`). The operator sees "deploy nginx to Site plant-a";
//! the store sees a normal authoring commit into `layers/20-site.plant-a`
//! (§11.4). The words "layer"/"revision" never surface to the operator
//! (§11.5) — [`Scope::label`] is the human phrasing.

use serde::{Deserialize, Serialize};

/// The base layer every device inherits (§11.1).
pub const LAYER_ALL: &str = "00-all";

/// A deploy/rollout target (§11.4). `kind` is the JSON discriminator:
/// `{"kind":"all"}`, `{"kind":"fleet","name":"north"}`, …,
/// `{"kind":"devices","ids":["dev-1","dev-2"]}`.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Scope {
    /// The base layer — every device (`layers/00-all`).
    All,
    /// A logical fleet group (`layers/10-fleet.<name>`).
    Fleet { name: String },
    /// A physical site (`layers/20-site.<name>`).
    Site { name: String },
    /// A device-type/hardware class (`layers/30-type.<name>`).
    #[serde(rename = "type")]
    Type { name: String },
    /// Explicit device list (`layers/40-device.<id>` each).
    Devices { ids: Vec<String> },
}

impl Scope {
    /// The authoring layer dir name(s) this scope writes into (§11.1
    /// taxonomy). Each is validated against the D11 layer-dir grammar
    /// ([`crate::tree::validate_layer_dir`]) so a bad name is a 422, not
    /// a malformed tree path.
    pub fn layers(&self) -> Result<Vec<String>, String> {
        let dirs = match self {
            Scope::All => vec![LAYER_ALL.to_string()],
            Scope::Fleet { name } => vec![format!("10-fleet.{name}")],
            Scope::Site { name } => vec![format!("20-site.{name}")],
            Scope::Type { name } => vec![format!("30-type.{name}")],
            Scope::Devices { ids } => {
                if ids.is_empty() {
                    return Err("a devices scope needs at least one device id".to_string());
                }
                ids.iter().map(|id| format!("40-device.{id}")).collect()
            }
        };
        for d in &dirs {
            crate::tree::validate_layer_dir(d)?;
        }
        Ok(dirs)
    }

    /// Human phrasing (§11.5: operator-facing copy, never a layer path).
    pub fn label(&self) -> String {
        match self {
            Scope::All => "All devices".to_string(),
            Scope::Fleet { name } => format!("Fleet {name}"),
            Scope::Site { name } => format!("Site {name}"),
            Scope::Type { name } => format!("Type {name}"),
            Scope::Devices { ids } => match ids.as_slice() {
                [one] => format!("Device {one}"),
                many => format!("{} devices", many.len()),
            },
        }
    }
}

/// Recover the scope a layer dir represents, for history/deploy
/// provenance (the inverse of [`Scope::layers`], best-effort). `None`
/// for a dir outside the §11.1 taxonomy. The numeric prefix is ignored
/// (only the label carries meaning — D12).
pub fn scope_of_layer_dir(dir: &str) -> Option<Scope> {
    let label = dir.split_once('-').map_or(dir, |(prefix, rest)| {
        if !prefix.is_empty() && prefix.bytes().all(|b| b.is_ascii_digit()) {
            rest
        } else {
            dir
        }
    });
    if label == "all" {
        return Some(Scope::All);
    }
    if let Some(n) = label.strip_prefix("fleet.") {
        return Some(Scope::Fleet { name: n.to_string() });
    }
    if let Some(n) = label.strip_prefix("site.") {
        return Some(Scope::Site { name: n.to_string() });
    }
    if let Some(n) = label.strip_prefix("type.") {
        return Some(Scope::Type { name: n.to_string() });
    }
    if let Some(n) = label.strip_prefix("device.") {
        return Some(Scope::Devices { ids: vec![n.to_string()] });
    }
    None
}

/// A one-phrase description of a scope narrowed by an optional tag
/// cohort (§11.5 rollout status "describe scope in words").
pub fn describe(scope: &Scope, tags: &std::collections::BTreeMap<String, String>) -> String {
    let base = scope.label();
    if tags.is_empty() {
        return base;
    }
    let pairs = tags
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(", ");
    format!("{base} tagged {pairs}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_maps_to_taxonomy_layers() {
        assert_eq!(Scope::All.layers().unwrap(), vec!["00-all"]);
        assert_eq!(
            Scope::Fleet { name: "north".into() }.layers().unwrap(),
            vec!["10-fleet.north"]
        );
        assert_eq!(
            Scope::Site { name: "plant-a".into() }.layers().unwrap(),
            vec!["20-site.plant-a"]
        );
        assert_eq!(
            Scope::Type { name: "hmi".into() }.layers().unwrap(),
            vec!["30-type.hmi"]
        );
        assert_eq!(
            Scope::Devices { ids: vec!["d1".into(), "d2".into()] }
                .layers()
                .unwrap(),
            vec!["40-device.d1", "40-device.d2"]
        );
        assert!(Scope::Devices { ids: vec![] }.layers().is_err());
        // Illegal names surface as authoring errors, not tree paths.
        assert!(Scope::Site { name: "../evil".into() }.layers().is_err());
    }

    #[test]
    fn layer_dir_round_trips_to_scope() {
        assert!(matches!(scope_of_layer_dir("00-all"), Some(Scope::All)));
        assert!(matches!(
            scope_of_layer_dir("10-fleet.north"),
            Some(Scope::Fleet { name }) if name == "north"
        ));
        assert!(matches!(
            scope_of_layer_dir("40-device.dev-1"),
            Some(Scope::Devices { ids }) if ids == ["dev-1"]
        ));
        assert!(scope_of_layer_dir("packages").is_none());
    }

    #[test]
    fn labels_and_descriptions_are_human() {
        assert_eq!(Scope::Site { name: "plant-a".into() }.label(), "Site plant-a");
        assert_eq!(
            Scope::Devices { ids: vec!["a".into(), "b".into(), "c".into()] }.label(),
            "3 devices"
        );
        let tags = std::collections::BTreeMap::from([("env".to_string(), "prod".to_string())]);
        assert_eq!(
            describe(&Scope::Fleet { name: "north".into() }, &tags),
            "Fleet north tagged env=prod"
        );
    }
}
