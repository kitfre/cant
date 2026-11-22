//! Bundled Steel names shared by binding projection and the runner.
use crate::bindings::SteelKeyword;
use serde::Deserialize;
use std::collections::HashSet;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Constructor {
    pub name: String,
    pub positional: Vec<String>,
    pub fields: Vec<SteelKeyword>,
    pub fixed: serde_json::Map<String, serde_json::Value>,
}

pub fn constructors() -> serde_json::Result<Vec<Constructor>> {
    serde_json::from_str(include_str!("constructors.json"))
}

pub fn native_reservations() -> HashSet<String> {
    serde_json::from_str(include_str!("native-reservations.json"))
        .expect("invalid bundled Steel reservations")
}
