//! System-prompt builder. 1:1 port of `packages/agent/src/harness/system-prompt.ts`.
//!
//! Renders a discovered skill catalog into a discoverable block the model can scan to decide
//! which skill to invoke. The actual skill bodies are loaded on demand via the per-skill
//! invocation block (see [`crate::harness::skills::format_skill_invocation`]).

use super::types::Skill;

const SKILL_BLOCK_PREAMBLE: &[&str] = &[
    "The user has provided skills they want you to use whenever the user request can be solved with their help.",
    "Below is a list of skills with their unique names and descriptions of what they do.",
    "Use the `Skill` tool to invoke a skill by name when applicable.",
    "When a skill file references a relative path, resolve it against the skill directory (parent of SKILL.md / dirname of the path) and use that absolute path in tool commands.",
];

/// Render the skill catalog into a `<skills>` block for inclusion in the system prompt. Returns
/// the empty string when `skills` is empty so the surrounding prompt stays clean.
pub fn format_skills_for_system_prompt(skills: &[Skill]) -> String {
    if skills.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    out.push_str("<skills>\n");
    for line in SKILL_BLOCK_PREAMBLE {
        out.push_str(line);
        out.push('\n');
    }
    out.push('\n');
    for skill in skills {
        out.push_str(&format!(
            "- name: {}\n  description: {}\n",
            skill.name, skill.description
        ));
    }
    out.push_str("</skills>");
    out
}

#[cfg(test)]
mod tests {
    use super::super::types::SkillSource;
    use super::*;

    fn mk(name: &str, desc: &str) -> Skill {
        Skill {
            name: name.into(),
            description: desc.into(),
            file_path: format!("/skills/{name}/SKILL.md"),
            content: "body".into(),
            disable_model_invocation: false,
            source: SkillSource::User,
        }
    }

    #[test]
    fn empty_when_no_skills() {
        assert_eq!(format_skills_for_system_prompt(&[]), "");
    }

    #[test]
    fn renders_each_skill_one_line() {
        let out = format_skills_for_system_prompt(&[mk("alpha", "first"), mk("beta", "second")]);
        assert!(out.starts_with("<skills>\n"));
        assert!(out.contains("- name: alpha\n  description: first\n"));
        assert!(out.contains("- name: beta\n  description: second\n"));
        assert!(out.ends_with("</skills>"));
    }
}
