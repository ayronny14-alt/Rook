#[allow(dead_code)]
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, RwLock};
use tracing::{debug, info, warn};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillDefinition {
    pub name: String,
    pub description: String,
    pub instructions: String,
    pub triggers: Vec<String>,
}

#[derive(Clone)]
pub struct SkillsRegistry {
    skills: Arc<RwLock<HashMap<String, SkillDefinition>>>,
}

impl SkillsRegistry {
    pub fn new() -> Self {
        Self {
            skills: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn register(&self, skill: SkillDefinition) {
        let name = skill.name.clone();
        info!("Registering skill: {}", name);
        let mut skills = self.skills.write().unwrap_or_else(|p| p.into_inner());
        skills.insert(name, skill);
    }

    pub fn get(&self, name: &str) -> Option<SkillDefinition> {
        let skills = self.skills.read().unwrap_or_else(|p| p.into_inner());
        skills.get(name).cloned()
    }

    pub fn list(&self) -> Vec<SkillDefinition> {
        let skills = self.skills.read().unwrap_or_else(|p| p.into_inner());
        skills.values().cloned().collect()
    }

    pub fn find_by_trigger(&self, trigger: &str) -> Vec<SkillDefinition> {
        let skills = self.skills.read().unwrap_or_else(|p| p.into_inner());
        skills
            .values()
            .filter(|s| s.triggers.iter().any(|t| trigger.contains(t.as_str())))
            .cloned()
            .collect()
    }

    /// Scan a directory for `.yaml`, `.yml`, or `.json` skill definition files,
    /// parse each into a [`SkillDefinition`], and register them.  Returns the
    /// number of skills successfully loaded.
    pub fn load_from_dir(&self, dir: &Path) -> usize {
        if !dir.exists() {
            debug!("Skills directory does not exist: {}", dir.display());
            return 0;
        }

        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(e) => {
                warn!("Cannot read skills directory {}: {}", dir.display(), e);
                return 0;
            }
        };

        let mut count = 0usize;
        for entry in entries.flatten() {
            let path = entry.path();
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();

            if !matches!(ext.as_str(), "yaml" | "yml" | "json") {
                continue;
            }

            let content = match std::fs::read_to_string(&path) {
                Ok(c) => c,
                Err(e) => {
                    warn!("Cannot read skill file {}: {}", path.display(), e);
                    continue;
                }
            };

            let skill: Result<SkillDefinition, String> = if ext == "json" {
                serde_json::from_str(&content).map_err(|e| e.to_string())
            } else {
                serde_yaml::from_str(&content).map_err(|e| e.to_string())
            };

            match skill {
                Ok(s) => {
                    debug!("Loaded skill '{}' from {}", s.name, path.display());
                    self.register(s);
                    count += 1;
                }
                Err(e) => warn!("Failed to parse skill file {}: {}", path.display(), e),
            }
        }

        info!("Loaded {} skill(s) from {}", count, dir.display());
        count
    }

    /// Execute a skill by name.  `args` is a JSON object whose keys are
    /// substituted into `{{key}}` placeholders in the skill's `instructions`
    /// field.  Returns a JSON object with the resolved instructions and
    /// metadata suitable for injecting into an LLM system prompt.
    pub async fn execute(&self, name: &str, args: serde_json::Value) -> Result<serde_json::Value> {
        let skill = self
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("Skill not found: {}", name))?;

        debug!("Executing skill '{}' with args: {:?}", name, args);

        // Substitute {{key}} placeholders with matching arg values.
        let mut instructions = skill.instructions.clone();
        if let Some(obj) = args.as_object() {
            for (key, val) in obj {
                let placeholder = format!("{{{{{}}}}}", key);
                let value_str = match val {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                instructions = instructions.replace(&placeholder, &value_str);
            }
        }

        Ok(serde_json::json!({
            "skill": skill.name,
            "description": skill.description,
            "instructions": instructions,
            "args": args,
            "status": "ready",
        }))
    }

    pub fn remove(&self, name: &str) -> bool {
        let mut skills = self.skills.write().unwrap_or_else(|p| p.into_inner());
        skills.remove(name).is_some()
    }

    pub fn count(&self) -> usize {
        let skills = self.skills.read().unwrap_or_else(|p| p.into_inner());
        skills.len()
    }
}
