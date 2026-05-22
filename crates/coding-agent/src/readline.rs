use std::borrow::Cow;

use rustyline::completion::{Completer, Pair};
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::validate::Validator;
use rustyline::{Context, Helper, Result};

use crate::commands::Registry;

#[derive(Clone, Debug, PartialEq, Eq)]
struct SlashCompletion {
    command: String,
    display: String,
}

/// Rustyline helper for slash-command completion and inline hints.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SlashCommandHelper {
    completions: Vec<SlashCompletion>,
}

impl SlashCommandHelper {
    pub fn from_registry(registry: &Registry) -> Self {
        let mut completions = Vec::new();
        for command in registry.commands() {
            let canonical = format!("/{}", command.name());
            completions.push(SlashCompletion {
                command: canonical.clone(),
                display: display_for(&canonical, command.description(), command.usage(), None),
            });
            for alias in command.aliases() {
                let alias_command = format!("/{alias}");
                completions.push(SlashCompletion {
                    command: alias_command.clone(),
                    display: display_for(
                        &alias_command,
                        command.description(),
                        command.usage(),
                        Some(&canonical),
                    ),
                });
            }
        }
        completions.sort_by(|a, b| a.command.cmp(&b.command));
        completions.dedup_by(|a, b| a.command == b.command);
        Self { completions }
    }

    fn complete_line(&self, line: &str, pos: usize) -> Option<(usize, Vec<Pair>)> {
        let (start, prefix) = slash_prefix_at_cursor(line, pos)?;
        let matches = self
            .completions
            .iter()
            .filter(|entry| entry.command.starts_with(prefix))
            .map(|entry| Pair {
                display: entry.display.clone(),
                replacement: entry.command.clone(),
            })
            .collect::<Vec<_>>();
        Some((start, matches))
    }

    fn hint_line(&self, line: &str, pos: usize) -> Option<String> {
        let (_start, prefix) = slash_prefix_at_cursor(line, pos)?;
        if prefix.len() <= 1 {
            return None;
        }
        let mut matches = self
            .completions
            .iter()
            .filter(|entry| entry.command.starts_with(prefix));
        let first = matches.next()?;
        if matches.next().is_some() || first.command == prefix {
            return None;
        }
        Some(first.command[prefix.len()..].to_string())
    }
}

impl Completer for SlashCommandHelper {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &Context<'_>,
    ) -> Result<(usize, Vec<Self::Candidate>)> {
        Ok(self.complete_line(line, pos).unwrap_or((pos, Vec::new())))
    }
}

impl Hinter for SlashCommandHelper {
    type Hint = String;

    fn hint(&self, line: &str, pos: usize, _ctx: &Context<'_>) -> Option<Self::Hint> {
        self.hint_line(line, pos)
    }
}

impl Highlighter for SlashCommandHelper {
    fn highlight_hint<'h>(&self, hint: &'h str) -> Cow<'h, str> {
        Cow::Borrowed(hint)
    }
}

impl Validator for SlashCommandHelper {}

impl Helper for SlashCommandHelper {}

fn display_for(command: &str, description: &str, usage: &str, alias_for: Option<&str>) -> String {
    let mut display = command.to_string();
    if !usage.is_empty() {
        display.push(' ');
        display.push_str(usage);
    }
    if let Some(canonical) = alias_for {
        display.push_str("  alias for ");
        display.push_str(canonical);
    }
    if !description.is_empty() {
        display.push_str("  ");
        display.push_str(description);
    }
    display
}

fn slash_prefix_at_cursor(line: &str, pos: usize) -> Option<(usize, &str)> {
    if pos > line.len() || !line.is_char_boundary(pos) {
        return None;
    }
    let before_cursor = &line[..pos];
    let trimmed = before_cursor.trim_start();
    let start = before_cursor.len() - trimmed.len();
    if !trimmed.starts_with('/') {
        return None;
    }
    let prefix = &before_cursor[start..pos];
    if prefix[1..].chars().any(char::is_whitespace) {
        return None;
    }
    Some((start, prefix))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::Registry;

    fn helper() -> SlashCommandHelper {
        SlashCommandHelper::from_registry(&Registry::with_builtins())
    }

    fn replacements_for(line: &str) -> Vec<String> {
        helper()
            .complete_line(line, line.len())
            .map(|(_, pairs)| pairs.into_iter().map(|p| p.replacement).collect())
            .unwrap_or_default()
    }

    #[test]
    fn slash_completion_lists_registry_commands_and_aliases() {
        let replacements = replacements_for("/");
        assert!(replacements.contains(&"/help".to_string()));
        assert!(replacements.contains(&"/thinking".to_string()));
        assert!(replacements.contains(&"/quit".to_string()));
        assert!(replacements.contains(&"/q".to_string()));
    }

    #[test]
    fn slash_completion_filters_by_prefix() {
        let replacements = replacements_for("/thi");
        assert_eq!(replacements, vec!["/thinking".to_string()]);
    }

    #[test]
    fn slash_completion_returns_replacement_start_after_leading_space() {
        let (start, pairs) = helper().complete_line("   /thi", "   /thi".len()).unwrap();
        assert_eq!(start, 3);
        assert_eq!(pairs[0].replacement, "/thinking");
    }

    #[test]
    fn slash_completion_ignores_normal_prompts_and_command_arguments() {
        assert!(
            helper()
                .complete_line("hello /thi", "hello /thi".len())
                .is_none()
        );
        assert_eq!(replacements_for("/skill test"), Vec::<String>::new());
    }

    #[test]
    fn slash_completion_provides_inline_hint_for_unique_prefix() {
        assert_eq!(
            helper().hint_line("/thi", "/thi".len()).as_deref(),
            Some("nking")
        );
        assert_eq!(helper().hint_line("/", "/".len()), None);
    }
}
