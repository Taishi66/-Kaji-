use std::collections::HashMap;
use std::path::Path;

use anyhow::{anyhow, Result};
use kaji_core::facts::{slugify, CreatedBy, Fact, FactIndex, FactStore, FactType};

use crate::context_mgmt::compact_messages;
use crate::conversation::message::Message;
use crate::kaji::{fact_index_path, project_facts_dir, user_facts_dir};
use crate::session::redact_text;
use crate::slash_commands::{recipe_slash_command, skill_slash_command};

use super::Agent;

pub const COMPACT_TRIGGERS: &[&str] =
    &["/compact", "Please compact this conversation", "/summarize"];

pub struct CommandDef {
    pub name: &'static str,
    pub description: &'static str,
}

static COMMANDS: &[CommandDef] = &[
    CommandDef {
        name: "prompts",
        description: "List available prompts, optionally filtered by extension",
    },
    CommandDef {
        name: "prompt",
        description: "Execute a prompt or show its info with --info",
    },
    CommandDef {
        name: "compact",
        description: "Compact the conversation history",
    },
    CommandDef {
        name: "clear",
        description: "Clear the conversation history",
    },
    CommandDef {
        name: "skills",
        description: "List installed skills and other available sources",
    },
    CommandDef {
        name: "doctor",
        description: "Check that your Kaji setup is working",
    },
    CommandDef {
        name: "goal",
        description: "Set a goal the agent must satisfy before finishing, or clear with /goal off",
    },
    CommandDef {
        name: "grind",
        description:
            "Set a goal the agent pursues relentlessly until max_turns, or clear with /grind off",
    },
    CommandDef {
        name: "status",
        description: "Show session status: model, provider, mode, and token usage",
    },
    CommandDef {
        name: "remember",
        description: "Save a durable note to memory",
    },
];

pub struct ParsedSlashCommand<'a> {
    pub command: &'a str,
    pub params_str: &'a str,
}

pub fn parse_slash_command(message_text: &str) -> Option<ParsedSlashCommand<'_>> {
    let mut trimmed = message_text.trim();

    if COMPACT_TRIGGERS.contains(&trimmed) {
        trimmed = COMPACT_TRIGGERS[0];
    }

    if !trimmed.starts_with('/') {
        return None;
    }

    let command_str = trimmed.strip_prefix('/').unwrap_or(trimmed);
    let (command, params_str) = command_str
        .split_once(' ')
        .map(|(cmd, p)| (cmd, p.trim()))
        .unwrap_or((command_str, ""));

    Some(ParsedSlashCommand {
        command,
        params_str,
    })
}

pub fn list_commands() -> &'static [CommandDef] {
    COMMANDS
}

pub fn is_known_slash_command(message_text: &str, working_dir: Option<&Path>) -> bool {
    let Some(parsed) = parse_slash_command(message_text) else {
        return false;
    };

    COMMANDS
        .iter()
        .any(|command| command.name == parsed.command)
        || recipe_slash_command::get_recipe_for_command(parsed.command).is_some()
        || skill_slash_command::list_commands(working_dir)
            .into_iter()
            .any(|command| command.name.eq_ignore_ascii_case(parsed.command))
}

fn is_clear_goal_param(params_str: &str) -> bool {
    matches!(params_str, "off" | "clear" | "none")
}

/// Whether a slash command should kick off an agent turn instead of just
/// returning a confirmation. Setting a `/goal` or `/grind` (with a description,
/// not the query or `off` forms) makes the agent start pursuing it immediately.
pub fn command_starts_turn(message_text: &str) -> bool {
    let Some(parsed) = parse_slash_command(message_text) else {
        return false;
    };
    matches!(parsed.command, "goal" | "grind")
        && !parsed.params_str.is_empty()
        && !is_clear_goal_param(parsed.params_str)
}

/// Longest description kept for a `/remember` fact. The description is the line
/// rendered in the generated `MEMORY.md`; the body holds the note in full.
const REMEMBER_DESCRIPTION_MAX_CHARS: usize = 120;

/// Split an optional leading type keyword (`decision:`, `gotcha:`,
/// `preference:`, `reference:`) off a `/remember` note. Without one the note is
/// a preference, so it stays in the user scope: nothing lands in the repo
/// without the user saying so.
pub(crate) fn parse_remember_note(args: &str) -> (FactType, String) {
    let note = args.trim();
    if let Some((keyword, rest)) = note.split_once(':') {
        if let Some(fact_type) = FactType::parse(&keyword.trim().to_lowercase()) {
            return (fact_type, rest.trim().to_string());
        }
    }
    (FactType::Preference, note.to_string())
}

fn is_project_scoped(fact_type: FactType) -> bool {
    fact_type != FactType::Preference
}

/// Build the fact a `/remember` note writes. A project-scoped fact is redacted
/// first, so neither its slug nor its description can carry a secret into the
/// repo; a user-scoped one keeps the note verbatim.
fn remembered_fact(fact_type: FactType, note: &str, session_id: &str, date: &str) -> Fact {
    let body = if is_project_scoped(fact_type) {
        redact_text(note).0
    } else {
        note.to_string()
    };
    let description: String = body
        .lines()
        .next()
        .unwrap_or_default()
        .chars()
        .take(REMEMBER_DESCRIPTION_MAX_CHARS)
        .collect();
    let mut slug = slugify(&body);
    if slug.is_empty() {
        slug = slugify(&format!("note-{session_id}"));
    }

    Fact {
        fact_type,
        slug,
        description,
        date: date.to_string(),
        session: session_id.to_string(),
        created_by: CreatedBy::User,
        body,
    }
}

impl Agent {
    pub async fn execute_command(
        &self,
        message_text: &str,
        session_id: &str,
    ) -> Result<Option<Message>> {
        let Some(parsed) = parse_slash_command(message_text) else {
            return Ok(None);
        };

        let command = parsed.command;
        let params_str = parsed.params_str;

        let params: Vec<&str> = if params_str.is_empty() {
            vec![]
        } else {
            params_str.split_whitespace().collect()
        };

        match command {
            "prompts" => self.handle_prompts_command(&params, session_id).await,
            "prompt" => self.handle_prompt_command(&params, session_id).await,
            "compact" => self.handle_compact_command(session_id).await,
            "clear" => self.handle_clear_command(session_id).await,
            "skills" => self.handle_skills_command(session_id).await,
            "doctor" => Ok(Some(crate::doctor::run(self, session_id).await?)),
            "status" => self.handle_status_command(session_id).await,
            "goal" => self.handle_goal_command(params_str).await,
            "grind" => self.handle_grind_command(params_str).await,
            "remember" => self.handle_remember_command(params_str, session_id).await,
            _ => {
                if let Some(message) = self
                    .handle_recipe_command(command, params_str, session_id)
                    .await?
                {
                    #[cfg(feature = "telemetry")]
                    crate::posthog::emit_custom_slash_command_used();
                    return Ok(Some(message));
                }

                self.handle_skill_command(command, params_str, session_id)
                    .await
            }
        }
    }

    async fn handle_compact_command(&self, session_id: &str) -> Result<Option<Message>> {
        let manager = self.config.session_manager.clone();
        let session = manager.get_session(session_id, true).await?;
        let conversation = session
            .conversation
            .ok_or_else(|| anyhow!("Session has no conversation"))?;

        let model_config = self.model_config_for_session(session_id).await?;
        let compaction = compact_messages(
            self.provider().await?.as_ref(),
            &model_config,
            session_id,
            &conversation,
            true, // is_manual_compact
        )
        .await?;

        manager
            .replace_conversation(session_id, &compaction.conversation)
            .await?;

        self.update_session_metrics(
            session_id,
            session.schedule_id,
            &compaction.usage,
            Some(compaction.retained_context_tokens),
        )
        .await?;

        Ok(Some(user_only_assistant_text("Compaction complete")))
    }

    async fn handle_clear_command(&self, session_id: &str) -> Result<Option<Message>> {
        use crate::conversation::Conversation;

        let manager = self.config.session_manager.clone();
        manager
            .replace_conversation(session_id, &Conversation::default())
            .await?;

        manager
            .update(session_id)
            .usage(kaji_providers::conversation::token_usage::Usage::new(
                Some(0),
                Some(0),
                Some(0),
            ))
            .apply()
            .await?;

        Ok(Some(user_only_assistant_text("Conversation cleared")))
    }

    async fn handle_skills_command(&self, session_id: &str) -> Result<Option<Message>> {
        let working_dir = self
            .config
            .session_manager
            .get_session(session_id, false)
            .await
            .ok()
            .map(|s| s.working_dir);
        let output = skill_slash_command::format_installed_skills(working_dir.as_deref());
        Ok(Some(Message::assistant().with_text(output)))
    }

    async fn handle_status_command(&self, session_id: &str) -> Result<Option<Message>> {
        let provider = self.provider().await?;
        let model_config = self.model_config_for_session(session_id).await?;
        let context_limit = provider
            .get_context_limit(&model_config)
            .await
            .unwrap_or_else(|_| model_config.context_limit());

        let kaji_mode = self.kaji_mode().await;

        let metadata = self
            .config
            .session_manager
            .get_session(session_id, false)
            .await
            .ok();

        // `usage` is the current context-window usage (reset by /compact);
        // `accumulated_usage` is the lifetime sum across all responses. The context
        // percentage must use the former, or it inflates and pegs at 100% in any
        // long or post-compaction session.
        let context_tokens = metadata
            .as_ref()
            .and_then(|s| s.usage.total_tokens)
            .unwrap_or(0)
            .max(0) as usize;
        let lifetime_tokens = metadata
            .as_ref()
            .and_then(|s| s.accumulated_usage.total_tokens)
            .unwrap_or(0)
            .max(0) as usize;

        let context_pct = if context_limit > 0 {
            let pct = ((context_tokens as f64 / context_limit as f64) * 100.0).round() as usize;
            format!("{}%", pct.min(100))
        } else {
            "N/A".to_string()
        };

        let text = format!(
            "**Session status**\n\n\
             - Model: {}\n\
             - Provider: {}\n\
             - Mode: {}\n\
             - Tokens (lifetime): {}\n\
             - Context: {} / {} tokens ({})",
            model_config.model_name,
            provider.get_name(),
            kaji_mode,
            lifetime_tokens,
            context_tokens,
            context_limit,
            context_pct,
        );

        Ok(Some(user_only_assistant_text(text)))
    }

    async fn handle_prompts_command(
        &self,
        params: &[&str],
        session_id: &str,
    ) -> Result<Option<Message>> {
        let extension_filter = params.first().map(|s| s.to_string());

        let prompts = self.list_extension_prompts(session_id).await;

        if let Some(filter) = &extension_filter {
            if !prompts.contains_key(filter) {
                let error_msg = format!("Extension '{}' not found", filter);
                return Ok(Some(Message::assistant().with_text(error_msg)));
            }
        }

        let filtered_prompts: HashMap<String, Vec<String>> = prompts
            .into_iter()
            .filter(|(ext, _)| extension_filter.as_ref().is_none_or(|f| f == ext))
            .map(|(extension, prompt_list)| {
                let names = prompt_list.into_iter().map(|p| p.name).collect();
                (extension, names)
            })
            .collect();

        let mut output = String::new();
        if filtered_prompts.is_empty() {
            output.push_str("No prompts available.\n");
        } else {
            output.push_str("Available prompts:\n\n");
            for (extension, prompt_names) in filtered_prompts {
                output.push_str(&format!("**{}**:\n", extension));
                for name in prompt_names {
                    output.push_str(&format!("  - {}\n", name));
                }
                output.push('\n');
            }
        }

        Ok(Some(Message::assistant().with_text(output)))
    }

    async fn handle_prompt_command(
        &self,
        params: &[&str],
        session_id: &str,
    ) -> Result<Option<Message>> {
        if params.is_empty() {
            return Ok(Some(
                Message::assistant().with_text("Prompt name argument is required"),
            ));
        }

        let prompt_name = params[0].to_string();
        let is_info = params.get(1).map(|s| *s == "--info").unwrap_or(false);

        if is_info {
            let prompts = self.list_extension_prompts(session_id).await;
            let mut prompt_info = None;

            for (extension, prompt_list) in prompts {
                if let Some(prompt) = prompt_list.iter().find(|p| p.name == prompt_name) {
                    let mut output = format!("**Prompt: {}**\n\n", prompt.name);
                    if let Some(desc) = &prompt.description {
                        output.push_str(&format!("Description: {}\n\n", desc));
                    }
                    output.push_str(&format!("Extension: {}\n\n", extension));

                    if let Some(args) = &prompt.arguments {
                        output.push_str("Arguments:\n");
                        for arg in args {
                            output.push_str(&format!("  - {}", arg.name));
                            if let Some(desc) = &arg.description {
                                output.push_str(&format!(": {}", desc));
                            }
                            output.push('\n');
                        }
                    }

                    prompt_info = Some(output);
                    break;
                }
            }

            return Ok(Some(Message::assistant().with_text(
                prompt_info.unwrap_or_else(|| format!("Prompt '{}' not found", prompt_name)),
            )));
        }

        let mut arguments = HashMap::new();
        for param in params.iter().skip(1) {
            if let Some((key, value)) = param.split_once('=') {
                let value = value.trim_matches('"');
                arguments.insert(key.to_string(), value.to_string());
            }
        }

        let arguments_value = serde_json::to_value(arguments)
            .map_err(|e| anyhow!("Failed to serialize arguments: {}", e))?;

        match self
            .get_prompt(session_id, &prompt_name, arguments_value)
            .await
        {
            Ok(prompt_result) => {
                for (i, prompt_message) in prompt_result.messages.into_iter().enumerate() {
                    let msg = Message::from(prompt_message);

                    let expected_role = if i % 2 == 0 {
                        rmcp::model::Role::User
                    } else {
                        rmcp::model::Role::Assistant
                    };

                    if msg.role != expected_role {
                        let error_msg = format!(
                            "Expected {:?} message at position {}, but found {:?}",
                            expected_role, i, msg.role
                        );
                        return Ok(Some(Message::assistant().with_text(error_msg)));
                    }

                    self.config
                        .session_manager
                        .clone()
                        .add_message(session_id, &msg)
                        .await?;
                }

                let last_message = self
                    .config
                    .session_manager
                    .get_session(session_id, true)
                    .await?
                    .conversation
                    .ok_or_else(|| anyhow!("No conversation found"))?
                    .last()
                    .cloned()
                    .ok_or_else(|| anyhow!("No messages in conversation"))?;

                Ok(Some(last_message))
            }
            Err(e) => Ok(Some(
                Message::assistant().with_text(format!("Error getting prompt: {}", e)),
            )),
        }
    }

    async fn handle_recipe_command(
        &self,
        command: &str,
        params_str: &str,
        session_id: &str,
    ) -> Result<Option<Message>> {
        match recipe_slash_command::resolve_command(command, params_str) {
            Ok(None) => Ok(None),
            Ok(Some((recipe, prompt))) => {
                self.apply_recipe_components(recipe.response.clone(), true)
                    .await;
                self.config
                    .session_manager
                    .update(session_id)
                    .recipe(Some(recipe))
                    .apply()
                    .await?;
                Ok(Some(Message::user().with_text(prompt)))
            }
            Err(text) => Ok(Some(Message::assistant().with_text(text))),
        }
    }

    async fn handle_skill_command(
        &self,
        command: &str,
        params_str: &str,
        session_id: &str,
    ) -> Result<Option<Message>> {
        let working_dir = self
            .config
            .session_manager
            .get_session(session_id, false)
            .await
            .ok()
            .map(|session| session.working_dir);

        match skill_slash_command::resolve_command(command, params_str, working_dir.as_deref()) {
            Ok(None) => Ok(None),
            Ok(Some(prompt)) => Ok(Some(Message::user().with_text(prompt))),
            Err(text) => Ok(Some(Message::assistant().with_text(text))),
        }
    }

    async fn handle_goal_command(&self, params_str: &str) -> Result<Option<Message>> {
        if params_str.is_empty() {
            let current = self.get_goal().await;
            let text = match current {
                Some(goal) => format!("Current goal: {goal}"),
                None => "No goal set. Use `/goal <description>` to set one.".to_string(),
            };
            return Ok(Some(Message::assistant().with_text(text)));
        }

        if is_clear_goal_param(params_str) {
            self.set_goal(None).await;
            return Ok(Some(
                Message::assistant().with_text("Goal cleared. The agent will finish normally."),
            ));
        }

        let goal = params_str.to_string();
        self.set_goal(Some(goal.clone())).await;
        Ok(Some(Message::assistant().with_text(format!(
            "Goal set. The agent will verify this goal is met before finishing:\n\n> {goal}"
        ))))
    }

    async fn handle_grind_command(&self, params_str: &str) -> Result<Option<Message>> {
        if params_str.is_empty() {
            let current = self.get_grind().await;
            let text = match current {
                Some(goal) => format!("Current grind goal: {goal}"),
                None => "No grind goal set. Use `/grind <description>` to set one.".to_string(),
            };
            return Ok(Some(Message::assistant().with_text(text)));
        }

        if is_clear_goal_param(params_str) {
            self.set_grind(None).await;
            return Ok(Some(
                Message::assistant().with_text("Grind cleared. The agent will finish normally."),
            ));
        }

        let goal = params_str.to_string();
        self.set_grind(Some(goal.clone())).await;
        Ok(Some(Message::assistant().with_text(format!(
            "Grind goal set. The agent will keep working until max_turns is reached:\n\n> {goal}"
        ))))
    }

    /// Write one durable fact straight to disk, spending no LLM token, then let
    /// the usual trigger decide whether the pending journal is worth a curation
    /// run. The reply stays out of the conversation: `/remember` costs the turn
    /// nothing.
    async fn handle_remember_command(
        &self,
        params_str: &str,
        session_id: &str,
    ) -> Result<Option<Message>> {
        let working_dir = match self
            .config
            .session_manager
            .get_session(session_id, false)
            .await
        {
            Ok(session) => session.working_dir,
            Err(_) => std::env::current_dir()?,
        };

        let fact = write_remembered_note(params_str, session_id, &working_dir)?;

        if fact.is_some() {
            if let (Ok(provider), Ok(model_config)) = (
                self.provider().await,
                self.model_config_for_session(session_id).await,
            ) {
                crate::kaji::maybe_spawn_curation(
                    provider.clone(),
                    provider.get_name().to_string(),
                    model_config,
                    session_id.to_string(),
                    working_dir,
                );
            }
        }

        Ok(Some(remember_reply(fact.as_ref())))
    }
}

/// Effectful core of `/remember`, shared by both agent loops: parse the note,
/// write the fact into its scope and refresh the index. Returns `None` when the
/// note is empty, which writes nothing.
pub(crate) fn write_remembered_note(
    params_str: &str,
    session_id: &str,
    working_dir: &Path,
) -> Result<Option<Fact>> {
    let (fact_type, note) = parse_remember_note(params_str);
    if note.is_empty() {
        return Ok(None);
    }

    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let fact = remembered_fact(fact_type, &note, session_id, &today);

    let project = FactStore::new(project_facts_dir(working_dir));
    let user = FactStore::new(user_facts_dir());
    let store = if is_project_scoped(fact_type) {
        &project
    } else {
        &user
    };
    store.write(&fact)?;

    let mut index = FactIndex::open(&fact_index_path(working_dir))?;
    index.rebuild_if_stale(&[("project", &project), ("user", &user)])?;

    Ok(Some(fact))
}

pub(crate) fn remember_reply(fact: Option<&Fact>) -> Message {
    match fact {
        Some(fact) => {
            user_only_assistant_text(format!("記 1 fait mémorisé : {}", fact.file_name()))
        }
        None => user_only_assistant_text(
            "Usage: /remember [decision:|gotcha:|preference:|reference:] <note>",
        ),
    }
}

fn user_only_assistant_text(text: impl Into<String>) -> Message {
    Message::assistant().with_text(text).user_only()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversation::message::MessageContent;

    #[test]
    fn parse_slash_command_splits_on_literal_space() {
        let parsed = parse_slash_command("/speckit.plan hello world").unwrap();

        assert_eq!(parsed.command, "speckit.plan");
        assert_eq!(parsed.params_str, "hello world");
    }

    #[test]
    fn parse_slash_command_does_not_split_on_tab_or_newline() {
        let parsed = parse_slash_command("/speckit.plan\thello").unwrap();
        assert_eq!(parsed.command, "speckit.plan\thello");
        assert_eq!(parsed.params_str, "");

        let parsed = parse_slash_command("/speckit.plan\nhello").unwrap();
        assert_eq!(parsed.command, "speckit.plan\nhello");
        assert_eq!(parsed.params_str, "");
    }

    #[test]
    fn command_starts_turn_only_for_goal_and_grind_with_description() {
        assert!(command_starts_turn("/goal make all tests pass"));
        assert!(command_starts_turn("/grind keep refactoring"));

        // Query and clear forms must not start a turn.
        assert!(!command_starts_turn("/goal"));
        assert!(!command_starts_turn("/goal off"));
        assert!(!command_starts_turn("/goal clear"));
        assert!(!command_starts_turn("/goal none"));
        assert!(!command_starts_turn("/grind"));
        assert!(!command_starts_turn("/grind off"));

        // Other commands and plain prompts never start a turn here.
        assert!(!command_starts_turn("/compact"));
        assert!(!command_starts_turn("just a normal message"));
    }

    #[test]
    fn user_only_assistant_text_is_durable_text_not_system_notification() {
        let message = user_only_assistant_text("Conversation cleared");

        assert!(message.metadata.user_visible);
        assert!(!message.metadata.agent_visible);
        assert_eq!(message.role, rmcp::model::Role::Assistant);
        assert!(matches!(
            message.content.as_slice(),
            [MessageContent::Text(text)] if text.text == "Conversation cleared"
        ));
    }

    #[test]
    fn status_is_registered_as_a_builtin_command() {
        assert!(list_commands()
            .iter()
            .any(|command| command.name == "status"));
    }

    #[test]
    fn remember_is_registered_as_a_builtin_command() {
        assert!(list_commands()
            .iter()
            .any(|command| command.name == "remember"));
    }

    #[test]
    fn remember_note_parses_optional_type_prefix() {
        assert!(matches!(
            parse_remember_note("gotcha: rm est aliasé").0,
            FactType::Gotcha
        ));
        assert!(matches!(
            parse_remember_note("juste une note").0,
            FactType::Preference
        ));
        assert_eq!(parse_remember_note("decision: choix A").1, "choix A");
    }

    #[test]
    fn remember_note_keeps_a_colon_that_is_not_a_type() {
        let (fact_type, note) = parse_remember_note("note: garder tel quel");
        assert!(matches!(fact_type, FactType::Preference));
        assert_eq!(note, "note: garder tel quel");
    }

    #[test]
    fn remembered_fact_is_authored_by_the_user() {
        let note = "prefer ripgrep over grep";
        let fact = remembered_fact(FactType::Preference, note, "s-1", "2026-08-22");

        assert_eq!(fact.created_by, CreatedBy::User);
        assert_eq!(fact.fact_type, FactType::Preference);
        assert_eq!(fact.session, "s-1");
        assert_eq!(fact.date, "2026-08-22");
        assert_eq!(fact.body, note);
        assert_eq!(fact.description, note);
        assert_eq!(fact.slug, "prefer-ripgrep-over-grep");
    }

    #[test]
    fn remembered_fact_description_is_the_first_line_capped() {
        let note = format!("{}\nseconde ligne", "a".repeat(200));
        let fact = remembered_fact(FactType::Gotcha, &note, "s-1", "2026-08-22");

        assert_eq!(fact.description.chars().count(), 120);
        assert!(!fact.description.contains("seconde"));
        assert!(fact.body.contains("seconde ligne"), "the body keeps it all");
    }

    #[test]
    fn remembered_fact_falls_back_to_a_session_slug() {
        let fact = remembered_fact(FactType::Reference, "«»", "sess-42", "2026-08-22");

        assert_eq!(fact.slug, "note-sess-42");
        assert!(kaji_core::facts::validate_slug(&fact.slug));
    }

    #[test]
    fn remembered_fact_redacts_the_project_scope_only() {
        let note = "le token est ghp_abcdefghijklmnopqrstuvwxyz1234567890";

        let project = remembered_fact(FactType::Decision, note, "s-1", "2026-08-22");
        assert!(!project
            .body
            .contains("ghp_abcdefghijklmnopqrstuvwxyz1234567890"));
        assert!(!project
            .description
            .contains("ghp_abcdefghijklmnopqrstuvwxyz1234567890"));

        let user = remembered_fact(FactType::Preference, note, "s-1", "2026-08-22");
        assert_eq!(
            user.body, note,
            "a user-scoped fact keeps the note verbatim"
        );
    }
}
