//! Slash-command completion for the TUI input box.
//!
//! Previously this wrapped `rustyline`'s `Completer`/`Hinter` traits. The full-screen TUI owns
//! its own input widget (`tui-textarea`), so this is now a plain matcher: given the current
//! input line it returns the slash commands whose names share the typed prefix. The app renders
//! those as a completion popup above the input and cycles/accepts them on Tab.

use crate::commands::{Registry, skill_shortcuts};
use pie_agent_core::Skill;

/// Precomputed, sorted, de-duplicated list of `/command` strings (canonical names + aliases).
#[derive(Clone, Debug, Default)]
pub struct SlashCompleter {
    commands: Vec<String>,
}

impl SlashCompleter {
    #[allow(dead_code)]
    pub fn from_registry(registry: &Registry) -> Self {
        Self::from_registry_and_skills(registry, &[])
    }

    pub fn from_registry_and_skills(registry: &Registry, skills: &[Skill]) -> Self {
        let mut commands = Vec::new();
        for command in registry.commands() {
            commands.push(format!("/{}", command.name()));
            for alias in command.aliases() {
                commands.push(format!("/{alias}"));
            }
        }
        commands.extend(
            skill_shortcuts(skills, registry)
                .into_iter()
                .map(|shortcut| shortcut.command),
        );
        commands.sort();
        commands.dedup();
        Self { commands }
    }

    /// Completions for the current input. Returns matching `/command` strings when `line` is a
    /// bare slash token (`/`, `/he`, …) with no whitespace yet; otherwise empty.
    pub fn matches(&self, line: &str) -> Vec<String> {
        let Some(token) = slash_token(line) else {
            return Vec::new();
        };
        let matches: Vec<String> = self
            .commands
            .iter()
            .filter(|c| c.starts_with(token))
            .cloned()
            .collect();
        // Nothing left to complete when the only match is what the user already typed.
        if matches.len() == 1 && matches[0] == token {
            return Vec::new();
        }
        matches
    }
}

/// Extract the slash token at the start of `line` (after leading whitespace). Returns `None`
/// unless the trimmed line begins with `/` and contains no interior whitespace (i.e. the user
/// is still typing the command name, not its arguments).
fn slash_token(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    if !trimmed.starts_with('/') {
        return None;
    }
    if trimmed[1..].contains(char::is_whitespace) {
        return None;
    }
    Some(trimmed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skill(name: &str, disabled: bool) -> Skill {
        Skill {
            name: name.into(),
            description: format!("description for {name}"),
            file_path: format!("/tmp/{name}/SKILL.md"),
            content: "SECRET SKILL BODY".into(),
            disable_model_invocation: disabled,
            source: pie_agent_core::SkillSource::User,
        }
    }

    fn completer() -> SlashCompleter {
        SlashCompleter::from_registry(&Registry::with_builtins())
    }

    #[test]
    fn lists_commands_and_aliases_for_bare_slash() {
        let m = completer().matches("/");
        assert!(m.contains(&"/help".to_string()));
        assert!(m.contains(&"/quit".to_string()));
        assert!(m.contains(&"/q".to_string()));
    }

    #[test]
    fn filters_by_prefix() {
        let m = completer().matches("/thi");
        assert_eq!(m, vec!["/thinking".to_string()]);

        let m = completer().matches("/goal-s");
        assert_eq!(m, vec!["/goal-start".to_string()]);
    }

    #[test]
    fn no_completion_once_argument_typed() {
        assert!(completer().matches("/skill test").is_empty());
        assert!(completer().matches("hello").is_empty());
    }

    #[test]
    fn exact_unique_match_is_not_offered() {
        // Already fully typed and unique — nothing left to complete.
        assert!(completer().matches("/thinking").is_empty());
    }

    #[test]
    fn includes_enabled_skill_commands_and_hides_disabled_or_conflicting() {
        let registry = Registry::with_builtins();
        let completer = SlashCompleter::from_registry_and_skills(
            &registry,
            &[
                skill("db9", false),
                skill("hidden-skill", true),
                skill("help", false),
            ],
        );

        let m = completer.matches("/d");
        assert!(m.contains(&"/db9".to_string()));
        assert!(
            !completer
                .matches("/hidden")
                .contains(&"/hidden-skill".to_string())
        );
        assert!(!completer.matches("/help").contains(&"/help".to_string()));
    }
}
