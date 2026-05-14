//! Skills system.
//!
//! TS pi's coding-agent supports `<workspace>/.pi/skills/*.md` files that get
//! injected into the system prompt or invoked via slash commands. We mirror
//! that surface with a tiny loader:
//!
//! - Each `.md` file becomes a `Skill { name, body }`.
//! - Optional YAML/TOML frontmatter (between `---` lines) provides metadata.
//!   Supported fields: `description`, `enabled`, `trigger`.
//! - Skills with `trigger: always` are concatenated into the system prompt.
//! - Skills with `trigger: command` register as `/skill-<name>` slash commands.
//! - Skills with `trigger: keyword` are appended when the user prompt contains
//!   any of the configured keywords.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Skill {
    pub name: String,
    pub body: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default = "default_trigger")]
    pub trigger: SkillTrigger,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub keywords: Vec<String>,
}

fn default_trigger() -> SkillTrigger {
    SkillTrigger::Always
}

fn default_enabled() -> bool {
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SkillTrigger {
    Always,
    Command,
    Keyword,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SkillSet {
    skills: Vec<Skill>,
}

impl SkillSet {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn load_workspace(root: &Path) -> Self {
        let mut out = SkillSet::default();
        let dir = root.join(".pi").join("skills");
        if let Ok(entries) = fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("md") {
                    continue;
                }
                if let Some(skill) = parse_skill_file(&path) {
                    out.skills.push(skill);
                }
            }
        }
        out
    }

    pub fn skills(&self) -> &[Skill] {
        &self.skills
    }

    /// Skills that always inject into the system prompt.
    pub fn always_prompt(&self) -> String {
        let mut out = String::new();
        for skill in &self.skills {
            if !skill.enabled || skill.trigger != SkillTrigger::Always {
                continue;
            }
            out.push_str("\n\n# Skill: ");
            out.push_str(&skill.name);
            out.push('\n');
            out.push_str(skill.body.trim());
        }
        out
    }

    /// Skills triggered by keyword match against the user prompt.
    pub fn keyword_match(&self, prompt: &str) -> Vec<&Skill> {
        let lowered = prompt.to_ascii_lowercase();
        self.skills
            .iter()
            .filter(|skill| {
                skill.enabled
                    && skill.trigger == SkillTrigger::Keyword
                    && skill
                        .keywords
                        .iter()
                        .any(|kw| lowered.contains(&kw.to_ascii_lowercase()))
            })
            .collect()
    }

    /// Skills registered as `/skill-<name>` commands.
    pub fn command_skills(&self) -> Vec<&Skill> {
        self.skills
            .iter()
            .filter(|skill| skill.enabled && skill.trigger == SkillTrigger::Command)
            .collect()
    }
}

fn parse_skill_file(path: &PathBuf) -> Option<Skill> {
    let raw = fs::read_to_string(path).ok()?;
    let stem = path.file_stem().and_then(|s| s.to_str())?.to_string();
    let (frontmatter, body) = split_frontmatter(&raw);

    let mut skill = Skill {
        name: stem,
        body: body.trim().to_string(),
        description: None,
        trigger: SkillTrigger::Always,
        enabled: true,
        keywords: Vec::new(),
    };

    if let Some(frontmatter) = frontmatter {
        if let Ok(meta) = toml::from_str::<FrontmatterMeta>(&frontmatter) {
            if let Some(name) = meta.name {
                skill.name = name;
            }
            skill.description = meta.description;
            if let Some(trigger) = meta.trigger {
                skill.trigger = trigger;
            }
            if let Some(enabled) = meta.enabled {
                skill.enabled = enabled;
            }
            if let Some(keywords) = meta.keywords {
                skill.keywords = keywords;
            }
        }
    }
    Some(skill)
}

#[derive(Debug, Deserialize)]
struct FrontmatterMeta {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    trigger: Option<SkillTrigger>,
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    keywords: Option<Vec<String>>,
}

fn split_frontmatter(text: &str) -> (Option<String>, &str) {
    if !text.starts_with("---") {
        return (None, text);
    }
    let body_start = match text[3..].find("\n---\n") {
        Some(idx) => idx + 3 + 5,
        None => return (None, text),
    };
    let frontmatter = text[3..body_start - 5].trim().to_string();
    (Some(frontmatter), &text[body_start..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_frontmatter() {
        let raw = "---\ntrigger = \"keyword\"\nkeywords = [\"sql\"]\n---\n# heading\nbody";
        let (fm, body) = split_frontmatter(raw);
        assert!(fm.is_some());
        assert!(body.contains("# heading"));
    }

    #[test]
    fn loads_skills_from_directory() {
        let dir = tempfile::tempdir().unwrap();
        let skills_dir = dir.path().join(".pi").join("skills");
        std::fs::create_dir_all(&skills_dir).unwrap();
        std::fs::write(
            skills_dir.join("sql.md"),
            "---\ntrigger = \"keyword\"\nkeywords = [\"sql\"]\n---\nUse parameterized queries.",
        )
        .unwrap();
        std::fs::write(
            skills_dir.join("always.md"),
            "---\ntrigger = \"always\"\n---\nBe terse.",
        )
        .unwrap();
        let set = SkillSet::load_workspace(dir.path());
        assert_eq!(set.skills().len(), 2);
        assert!(set.always_prompt().contains("Be terse"));
        let matches = set.keyword_match("draft a SQL query");
        assert_eq!(matches.len(), 1);
    }
}
