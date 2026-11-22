//! Pluggable guest language trait so Steel is first-class and additional languages
//! (Python, Lua, Janet, TypeScript, etc.) can be added without modifying host core.

use crate::capability::CapabilityMeta;
use crate::runner_process::RunnerCommand;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct LanguageIdentity {
    pub id: &'static str,
    pub language: &'static str,
    pub engine: &'static str,
    pub program_format: &'static str,
    pub result_kind: &'static str,
    pub console_prefix: &'static str,
}

pub trait GuestLanguage: Send + Sync {
    fn identity(&self) -> LanguageIdentity;
    fn runner_command(&self) -> Result<RunnerCommand, String>;
    fn project_bindings(&self, metas: &[CapabilityMeta]) -> Value;
    fn binding_name(&self, metas: &[CapabilityMeta], meta: &CapabilityMeta) -> String;
    fn signature(&self, metas: &[CapabilityMeta], meta: &CapabilityMeta) -> String;
    fn tool_description(&self, metas: &[CapabilityMeta]) -> String;
    fn source_description(&self) -> &'static str;
    fn inputs_description(&self) -> &'static str;
    fn format_metadata(&self, value: &Value) -> String;
    fn post_describe(&self, name: &str, value: Value, metas: &[CapabilityMeta]) -> Value;
}

#[derive(Default)]
pub struct LanguageRegistry {
    languages: HashMap<String, Arc<dyn GuestLanguage>>,
}

impl LanguageRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, lang: Arc<dyn GuestLanguage>) {
        self.languages.insert(lang.identity().id.to_string(), lang);
    }

    pub fn get(&self, id: &str) -> Option<Arc<dyn GuestLanguage>> {
        self.languages.get(id).cloned()
    }

    pub fn ids(&self) -> Vec<String> {
        let mut ids: Vec<_> = self.languages.keys().cloned().collect();
        ids.sort();
        ids
    }
}
