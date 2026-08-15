//! `/context` — per-category weight of the next provider request.

use std::collections::HashSet;
use std::path::Path;

use anyhow::Result;
use rmcp::model::Tool;

use super::Agent;
use crate::agents::extension::ExtensionConfig;
use crate::agents::extension_manager::get_tool_owner;
use crate::token_counter::create_token_counter;

/// Token weight of each block kaji would send to the provider on the next
/// turn, plus the limit and auto-compaction threshold it is measured against.
/// Counted with the local o200k tokenizer, so the numbers approximate — never
/// replace — what the provider reports.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ContextBreakdown {
    pub system: usize,
    pub hints: usize,
    pub skills: usize,
    pub mcp: usize,
    pub tools: usize,
    pub memory: usize,
    pub messages: usize,
    pub used: usize,
    pub limit: usize,
    pub last_reported: Option<usize>,
    pub compaction_threshold_pct: u8,
}

impl ContextBreakdown {
    pub fn compact_at(&self) -> usize {
        (self.limit as u64 * self.compaction_threshold_pct as u64 / 100) as usize
    }

    pub fn free(&self) -> usize {
        self.limit.saturating_sub(self.used)
    }

    pub fn used_pct(&self) -> u8 {
        if self.limit == 0 {
            return 0;
        }
        (((self.used as f64 / self.limit as f64) * 100.0).round() as u64).min(100) as u8
    }
}

fn is_mcp_extension(config: &ExtensionConfig) -> bool {
    matches!(
        config,
        ExtensionConfig::Stdio { .. }
            | ExtensionConfig::StreamableHttp { .. }
            | ExtensionConfig::Sse { .. }
    )
}

impl Agent {
    /// Break the next request down by category. Read-only: unlike the reply
    /// path it mirrors, nothing is persisted and no turn is ingested into
    /// memory.
    ///
    /// The prompt and tools are assembled through `prepare_tools_and_prompt`
    /// for both agent-loop paths — the state machine assembles its own inline
    /// but shares `SystemPromptBuilder`, so the numbers hold there too.
    pub async fn context_report(
        &self,
        session_id: &str,
        working_dir: &Path,
    ) -> Result<ContextBreakdown> {
        let (tools, toolshim_tools, system_prompt, model_config) = self
            .prepare_tools_and_prompt(session_id, working_dir)
            .await?;
        let counter = create_token_counter()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to create token counter: {e}"))?;

        let mcp_keys: HashSet<String> = self
            .extension_manager
            .get_extension_configs()
            .await
            .iter()
            .filter(|config| is_mcp_extension(config))
            .map(ExtensionConfig::key)
            .collect();
        let extensions_info = self
            .extension_manager
            .get_extensions_info(working_dir)
            .await;

        let skills = extensions_info
            .iter()
            .find(|info| info.name == crate::skills::EXTENSION_NAME)
            .map(|info| counter.count_tokens(&info.instructions))
            .unwrap_or(0);
        let mcp_instructions: usize = extensions_info
            .iter()
            .filter(|info| mcp_keys.contains(&info.name))
            .map(|info| counter.count_tokens(&info.instructions))
            .sum();

        let (mcp_tools, own_tools): (Vec<Tool>, Vec<Tool>) = tools
            .into_iter()
            .chain(toolshim_tools)
            .partition(|tool| get_tool_owner(tool).is_some_and(|owner| mcp_keys.contains(&owner)));
        let mcp_tools = counter.count_tokens_for_tools(&mcp_tools);
        let tools = counter.count_tokens_for_tools(&own_tools);

        let hints = counter.count_tokens(&crate::hints::load_hint_files_with_fallback(
            working_dir,
            &crate::hints::get_context_filenames(),
            &crate::hints::load_hints::build_gitignore(working_dir),
        ));

        let session = self
            .config
            .session_manager
            .get_session(session_id, true)
            .await?;
        let last_reported = session
            .usage
            .total_tokens
            .map(|total| total.max(0) as usize);
        let conversation = session
            .conversation
            .unwrap_or_else(crate::conversation::Conversation::empty);

        let memory = match crate::kaji::latest_user_instruction(conversation.messages()) {
            Some(query) => counter
                .count_tokens(&crate::kaji::splice_memory_block(
                    &system_prompt,
                    session_id,
                    &query,
                ))
                .saturating_sub(counter.count_tokens(&system_prompt)),
            None => 0,
        };

        // Under toolshim the tool schemas are rendered into the system prompt
        // itself, so they come off the system remainder to stay counted once.
        let tools_inside_prompt = if model_config.toolshim {
            mcp_tools + tools
        } else {
            0
        };
        let system = counter
            .count_tokens(&system_prompt)
            .saturating_sub(hints + skills + mcp_instructions + memory + tools_inside_prompt);

        let (provider_view, _) =
            super::reply_parts::provider_view_messages(conversation.messages());
        let messages = counter.count_chat_tokens("", provider_view.messages(), &[]);

        let limit = match self.provider().await {
            Ok(provider) => provider
                .get_context_limit(&model_config)
                .await
                .unwrap_or_else(|_| model_config.context_limit()),
            Err(_) => model_config.context_limit(),
        };
        let threshold = crate::config::Config::global()
            .get_param::<f64>("KAJI_AUTO_COMPACT_THRESHOLD")
            .unwrap_or(crate::context_mgmt::DEFAULT_COMPACTION_THRESHOLD);

        let mcp = mcp_instructions + mcp_tools;
        Ok(ContextBreakdown {
            system,
            hints,
            skills,
            mcp,
            tools,
            memory,
            messages,
            used: system + hints + skills + mcp + tools + memory + messages,
            limit,
            last_reported,
            compaction_threshold_pct: (threshold * 100.0).round().clamp(0.0, 100.0) as u8,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::{AgentConfig, KajiPlatform};
    use crate::config::{KajiMode, PermissionManager};
    use crate::conversation::message::Message;
    use crate::conversation::Conversation;
    use crate::providers::base::{stream_from_single_message, MessageStream, Provider};
    use crate::session::{SessionManager, SessionType};
    use async_trait::async_trait;
    use kaji_providers::conversation::token_usage::{ProviderUsage, Usage};
    use kaji_providers::errors::ProviderError;
    use kaji_providers::model::ModelConfig;
    use std::sync::{Arc, OnceLock};

    /// Point the KAJI memory dir at a throwaway root once per test process:
    /// `context_report` splices the shared cross-session store into the prompt
    /// exactly like a real turn, and must not read the user's own facts.
    fn isolate_memory_root() {
        static ONCE: OnceLock<()> = OnceLock::new();
        ONCE.get_or_init(|| {
            let dir = tempfile::tempdir().expect("tempdir for memory isolation");
            std::env::set_var("KAJI_MEMORY_DIR", dir.path());
            std::mem::forget(dir);
        });
    }

    struct MockProvider;

    #[async_trait]
    impl Provider for MockProvider {
        fn get_name(&self) -> &str {
            "mock"
        }

        async fn stream(
            &self,
            _model_config: &ModelConfig,
            _system: &str,
            _messages: &[Message],
            _tools: &[rmcp::model::Tool],
        ) -> Result<MessageStream, ProviderError> {
            Ok(stream_from_single_message(
                Message::assistant().with_text("ok"),
                ProviderUsage::new("mock".to_string(), Usage::default()),
            ))
        }
    }

    fn breakdown(used: usize, limit: usize, threshold: u8) -> ContextBreakdown {
        ContextBreakdown {
            system: used,
            hints: 0,
            skills: 0,
            mcp: 0,
            tools: 0,
            memory: 0,
            messages: 0,
            used,
            limit,
            last_reported: None,
            compaction_threshold_pct: threshold,
        }
    }

    #[test]
    fn compact_at_free_and_used_pct_handle_the_degenerate_cases() {
        let normal = breakdown(30_000, 200_000, 60);
        assert_eq!(normal.compact_at(), 120_000);
        assert_eq!(normal.free(), 170_000);
        assert_eq!(normal.used_pct(), 15);

        let no_limit = breakdown(1_000, 0, 60);
        assert_eq!(no_limit.compact_at(), 0);
        assert_eq!(no_limit.free(), 0);
        assert_eq!(no_limit.used_pct(), 0);

        let overflowing = breakdown(300_000, 200_000, 60);
        assert_eq!(overflowing.free(), 0);
        assert_eq!(overflowing.used_pct(), 100);
    }

    #[tokio::test]
    async fn context_report_sums_its_categories_over_a_live_session() -> Result<()> {
        isolate_memory_root();
        let data_dir = tempfile::tempdir()?;
        let working_dir = tempfile::tempdir()?;
        let session_manager = Arc::new(SessionManager::new(data_dir.path().to_path_buf()));
        let agent = Agent::with_config(AgentConfig::new(
            Arc::clone(&session_manager),
            Arc::new(PermissionManager::new(data_dir.path().to_path_buf())),
            None,
            KajiMode::default(),
            false,
            KajiPlatform::KajiCli,
        ));
        let session = session_manager
            .create_session(
                working_dir.path().to_path_buf(),
                "test-context-report".to_string(),
                SessionType::Hidden,
                KajiMode::default(),
            )
            .await?;
        let model_config = ModelConfig::new("test-model");
        agent
            .update_provider(Arc::new(MockProvider), model_config.clone(), &session.id)
            .await?;
        session_manager
            .replace_conversation(
                &session.id,
                &Conversation::new_unvalidated([
                    Message::user().with_text("refactor the parser"),
                    Message::assistant().with_text("on it"),
                    Message::user().with_text("start with the lexer"),
                ]),
            )
            .await?;

        let report = agent
            .context_report(&session.id, session.working_dir.as_path())
            .await?;

        assert_eq!(
            report.used,
            report.system
                + report.hints
                + report.skills
                + report.mcp
                + report.tools
                + report.memory
                + report.messages
        );
        assert!(
            report.messages > 0,
            "the session's messages must be counted"
        );
        assert!(report.system > 0, "the base system prompt is never empty");
        assert_eq!(report.limit, model_config.context_limit());
        assert_eq!(
            report.used_pct(),
            ((report.used as f64 / report.limit as f64) * 100.0).round() as u8
        );
        assert_eq!(report.free(), report.limit - report.used);

        Ok(())
    }
}
