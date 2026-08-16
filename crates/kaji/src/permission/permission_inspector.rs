use crate::agents::platform_extensions::MANAGE_EXTENSIONS_TOOL_NAME_COMPLETE;
use crate::agents::types::SharedProvider;
use crate::config::permission::PermissionLevel;
use crate::config::{KajiMode, PermissionManager};
use crate::conversation::message::{Message, ToolRequest};
use crate::permission::grants::{call_allowed_by_any, GrantRule, Spec};
use crate::permission::permission_judge::{detect_read_only_requests, PermissionCheckResult};
use crate::tool_inspection::{InspectionAction, InspectionResult, ToolInspector};
use anyhow::Result;
use async_trait::async_trait;
use rmcp::model::{JsonObject, Tool};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};

/// Permission Inspector that handles tool permission checking
pub struct PermissionInspector {
    pub permission_manager: Arc<PermissionManager>,
    provider: SharedProvider,
    session_manager: Arc<crate::session::SessionManager>,
    readonly_tools: RwLock<HashSet<String>>,
    session_grants: RwLock<HashMap<String, HashSet<GrantRule>>>,
}

fn cache_non_readonly_decision(
    permission_manager: &PermissionManager,
    candidate: &ToolRequest,
    is_readonly: bool,
) {
    if is_readonly {
        return;
    }
    if let Ok(tool_call) = &candidate.tool_call {
        permission_manager
            .update_smart_approve_permission(&tool_call.name, PermissionLevel::AskBefore);
    }
}

impl PermissionInspector {
    pub fn new(
        permission_manager: Arc<PermissionManager>,
        provider: SharedProvider,
        session_manager: Arc<crate::session::SessionManager>,
    ) -> Self {
        Self {
            permission_manager,
            provider,
            session_manager,
            readonly_tools: RwLock::new(HashSet::new()),
            session_grants: RwLock::new(HashMap::new()),
        }
    }

    /// Grants a call for the lifetime of the process, scoped to one session. Never persisted.
    pub fn grant_for_session(&self, session_id: &str, tool_name: &str, spec: Option<&Spec>) {
        self.session_grants
            .write()
            .unwrap()
            .entry(session_id.to_string())
            .or_default()
            .insert(GrantRule::new(tool_name, spec.cloned()));
    }

    fn session_allows(
        &self,
        session_id: &str,
        tool_name: &str,
        arguments: Option<&JsonObject>,
    ) -> bool {
        let session_grants = self.session_grants.read().unwrap();
        let Some(rules) = session_grants.get(session_id) else {
            return false;
        };
        let rules: Vec<GrantRule> = rules.iter().cloned().collect();
        call_allowed_by_any(&rules, tool_name, arguments)
    }

    // readonly_tools is per-agent to avoid concurrent session clobbering; write-annotated
    // tools are cached globally via PermissionManager.
    pub fn apply_tool_annotations(&self, tools: &[Tool]) {
        let mut readonly_annotated = HashSet::new();
        for tool in tools {
            let Some(anns) = &tool.annotations else {
                continue;
            };
            if anns.read_only_hint == Some(true) {
                readonly_annotated.insert(tool.name.to_string());
            }
        }
        *self.readonly_tools.write().unwrap() = readonly_annotated;
        self.permission_manager.apply_tool_annotations(tools);
    }

    pub fn is_readonly_annotated_tool(&self, tool_name: &str) -> bool {
        self.readonly_tools.read().unwrap().contains(tool_name)
    }

    /// Process inspection results into permission decisions
    /// This method takes all inspection results and converts them into a PermissionCheckResult
    /// that can be used by the agent to determine which tools to approve, deny, or ask for approval
    pub fn process_inspection_results(
        &self,
        remaining_requests: &[ToolRequest],
        inspection_results: &[InspectionResult],
    ) -> PermissionCheckResult {
        use crate::tool_inspection::apply_inspection_results_to_permissions;

        // Start with permission inspector's decisions as the baseline
        let mut permission_check_result = PermissionCheckResult {
            approved: vec![],
            needs_approval: vec![],
            denied: vec![],
        };

        // Apply permission inspector results first (baseline behavior)
        let permission_results: Vec<_> = inspection_results
            .iter()
            .filter(|result| result.inspector_name == "permission")
            .collect();

        for request in remaining_requests {
            // Find the permission decision for this request
            if let Some(permission_result) = permission_results
                .iter()
                .find(|result| result.tool_request_id == request.id)
            {
                match permission_result.action {
                    InspectionAction::Allow => {
                        permission_check_result.approved.push(request.clone());
                    }
                    InspectionAction::Deny => {
                        permission_check_result.denied.push(request.clone());
                    }
                    InspectionAction::RequireApproval(_) => {
                        permission_check_result.needs_approval.push(request.clone());
                    }
                }
            } else {
                // If no permission result found, default to needs approval for safety
                permission_check_result.needs_approval.push(request.clone());
            }
        }

        // Apply security and other inspector results as overrides
        let non_permission_results: Vec<_> = inspection_results
            .iter()
            .filter(|result| result.inspector_name != "permission")
            .cloned()
            .collect();

        if !non_permission_results.is_empty() {
            permission_check_result = apply_inspection_results_to_permissions(
                permission_check_result,
                &non_permission_results,
            );
        }

        permission_check_result
    }
}

#[async_trait]
impl ToolInspector for PermissionInspector {
    fn name(&self) -> &'static str {
        "permission"
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn inspect(
        &self,
        session_id: &str,
        tool_requests: &[ToolRequest],
        _messages: &[Message],
        kaji_mode: KajiMode,
    ) -> Result<Vec<InspectionResult>> {
        let mut results = Vec::new();
        let permission_manager = &self.permission_manager;
        let mut llm_detect_candidates: Vec<&ToolRequest> = Vec::new();

        for request in tool_requests {
            if let Ok(tool_call) = &request.tool_call {
                let tool_name = &tool_call.name;
                let arguments = tool_call.arguments.as_ref();

                let action = match kaji_mode {
                    KajiMode::Chat => continue,
                    KajiMode::Auto => InspectionAction::Allow,
                    KajiMode::Approve | KajiMode::SmartApprove => {
                        // 1. Denials, then session grants, then persisted grants
                        if permission_manager.is_call_denied(tool_name, arguments) {
                            InspectionAction::Deny
                        } else if self.session_allows(session_id, tool_name, arguments)
                            || permission_manager.is_call_allowed(tool_name, arguments)
                        {
                            InspectionAction::Allow
                        } else if permission_manager.asks_before(tool_name) {
                            InspectionAction::RequireApproval(None)
                        // 2. Check for a read-only annotation in SmartApprove mode
                        } else if kaji_mode == KajiMode::SmartApprove
                            && self.is_readonly_annotated_tool(tool_name)
                        {
                            InspectionAction::Allow
                        // 3. Special case for extension management
                        } else if tool_name == MANAGE_EXTENSIONS_TOOL_NAME_COMPLETE {
                            InspectionAction::RequireApproval(Some(
                                "Extension management requires approval for security".to_string(),
                            ))
                        // 4. Defer to LLM detection (SmartApprove, uncached or legacy cached allow)
                        } else if kaji_mode == KajiMode::SmartApprove
                            && matches!(
                                permission_manager.get_smart_approve_permission(tool_name),
                                None | Some(PermissionLevel::AlwaysAllow)
                            )
                        {
                            llm_detect_candidates.push(request);
                            continue;
                        // 5. Default: require approval for unknown tools
                        } else {
                            InspectionAction::RequireApproval(None)
                        }
                    }
                };

                let reason = match &action {
                    InspectionAction::Allow => {
                        if kaji_mode == KajiMode::Auto {
                            "Auto mode - all tools approved".to_string()
                        } else if self.is_readonly_annotated_tool(tool_name) {
                            "Tool annotated as read-only".to_string()
                        } else {
                            "User permission allows this tool".to_string()
                        }
                    }
                    InspectionAction::Deny => "User permission denies this tool".to_string(),
                    InspectionAction::RequireApproval(_) => {
                        if tool_name == MANAGE_EXTENSIONS_TOOL_NAME_COMPLETE {
                            "Extension management requires user approval".to_string()
                        } else {
                            "Tool requires user approval".to_string()
                        }
                    }
                };

                results.push(InspectionResult {
                    tool_request_id: request.id.clone(),
                    action,
                    reason,
                    confidence: 1.0, // Permission decisions are definitive
                    inspector_name: self.name().to_string(),
                    finding_id: None,
                });
            }
        }

        // LLM-based read-only detection for deferred SmartApprove candidates
        if !llm_detect_candidates.is_empty() {
            let detected_request_ids: HashSet<String> = match self.provider.lock().await.clone() {
                Some(provider) => detect_read_only_requests(
                    provider,
                    &self.session_manager,
                    session_id,
                    llm_detect_candidates.to_vec(),
                )
                .await
                .into_iter()
                .collect(),
                None => Default::default(),
            };

            for candidate in &llm_detect_candidates {
                let is_readonly = detected_request_ids.contains(&candidate.id);

                cache_non_readonly_decision(permission_manager, candidate, is_readonly);

                results.push(InspectionResult {
                    tool_request_id: candidate.id.clone(),
                    action: if is_readonly {
                        InspectionAction::Allow
                    } else {
                        InspectionAction::RequireApproval(None)
                    },
                    reason: if is_readonly {
                        "LLM detected as read-only".to_string()
                    } else {
                        "Tool requires user approval".to_string()
                    },
                    confidence: 1.0, // Permission decisions are definitive
                    inspector_name: self.name().to_string(),
                    finding_id: None,
                });
            }
        }

        Ok(results)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::CallToolRequestParams;
    use rmcp::object;
    use std::sync::Arc;
    use test_case::test_case;
    use tokio::sync::Mutex;

    async fn inspect_tool(
        mode: KajiMode,
        smart_approved: bool,
        user_permission: Option<PermissionLevel>,
        smart_approve_cache: Option<PermissionLevel>,
    ) -> (InspectionAction, Option<PermissionLevel>) {
        let pm = Arc::new(PermissionManager::new(tempfile::tempdir().unwrap().keep()));
        if let Some(level) = user_permission {
            pm.update_user_permission("tool", level);
        }
        if let Some(level) = smart_approve_cache {
            pm.update_smart_approve_permission("tool", level);
        }
        let session_manager = Arc::new(crate::session::SessionManager::new(
            tempfile::tempdir().unwrap().keep(),
        ));
        let inspector =
            PermissionInspector::new(Arc::clone(&pm), Arc::new(Mutex::new(None)), session_manager);
        if smart_approved {
            *inspector.readonly_tools.write().unwrap() = ["tool".to_string()].into_iter().collect();
        }
        let req = ToolRequest {
            id: "req".into(),
            tool_call: Ok(CallToolRequestParams::new("tool").with_arguments(object!({}))),
            metadata: None,
            tool_meta: None,
        };
        let mut results = inspector
            .inspect(kaji_test_support::TEST_SESSION_ID, &[req], &[], mode)
            .await
            .unwrap();

        (
            results.remove(0).action,
            pm.get_smart_approve_permission("tool"),
        )
    }

    #[test_case(KajiMode::Auto, false, None, InspectionAction::Allow; "auto_allows")]
    #[test_case(KajiMode::SmartApprove, true, None, InspectionAction::Allow; "smart_approve_annotation_allows")]
    #[test_case(KajiMode::SmartApprove, false, Some(PermissionLevel::AlwaysAllow), InspectionAction::RequireApproval(None); "smart_approve_ignores_legacy_cached_allow")]
    #[test_case(KajiMode::SmartApprove, false, Some(PermissionLevel::AskBefore), InspectionAction::RequireApproval(None); "smart_approve_cached_ask")]
    #[test_case(KajiMode::SmartApprove, false, None, InspectionAction::RequireApproval(None); "smart_approve_unknown_defers")]
    #[test_case(KajiMode::Approve, false, None, InspectionAction::RequireApproval(None); "approve_requires_approval")]
    #[test_case(KajiMode::Approve, false, Some(PermissionLevel::AlwaysAllow), InspectionAction::RequireApproval(None); "approve_ignores_cache")]
    #[test_case(KajiMode::Approve, true, None, InspectionAction::RequireApproval(None); "approve_ignores_annotation")]
    #[tokio::test]
    async fn test_inspect_action(
        mode: KajiMode,
        smart_approved: bool,
        cache: Option<PermissionLevel>,
        expected: InspectionAction,
    ) {
        let (action, _) = inspect_tool(mode, smart_approved, None, cache).await;
        assert_eq!(action, expected);
    }

    #[test_case(PermissionLevel::AlwaysAllow, InspectionAction::Allow; "explicit_allow")]
    #[test_case(PermissionLevel::AskBefore, InspectionAction::RequireApproval(None); "explicit_ask")]
    #[test_case(PermissionLevel::NeverAllow, InspectionAction::Deny; "explicit_deny")]
    #[tokio::test]
    async fn smart_approve_preserves_user_permission_over_legacy_cache(
        user_permission: PermissionLevel,
        expected: InspectionAction,
    ) {
        let (action, cache) = inspect_tool(
            KajiMode::SmartApprove,
            false,
            Some(user_permission),
            Some(PermissionLevel::AlwaysAllow),
        )
        .await;

        assert_eq!(action, expected);
        assert_eq!(cache, Some(PermissionLevel::AlwaysAllow));
    }

    #[tokio::test]
    async fn smart_approve_rejudges_legacy_cached_allow() {
        let (action, cache) = inspect_tool(
            KajiMode::SmartApprove,
            false,
            None,
            Some(PermissionLevel::AlwaysAllow),
        )
        .await;

        assert_eq!(action, InspectionAction::RequireApproval(None));
        assert_eq!(cache, Some(PermissionLevel::AskBefore));
    }

    const SHELL: &str = "shell";
    const SESSION: &str = "session-1";

    fn shell_inspector() -> PermissionInspector {
        let permission_manager =
            Arc::new(PermissionManager::new(tempfile::tempdir().unwrap().keep()));
        let session_manager = Arc::new(crate::session::SessionManager::new(
            tempfile::tempdir().unwrap().keep(),
        ));
        PermissionInspector::new(
            permission_manager,
            Arc::new(Mutex::new(None)),
            session_manager,
        )
    }

    async fn inspect_shell(
        inspector: &PermissionInspector,
        session_id: &str,
        command: &str,
    ) -> InspectionAction {
        inspect_shell_in_mode(inspector, session_id, command, KajiMode::Approve).await
    }

    async fn inspect_shell_in_mode(
        inspector: &PermissionInspector,
        session_id: &str,
        command: &str,
        mode: KajiMode,
    ) -> InspectionAction {
        let request = ToolRequest {
            id: "req".into(),
            tool_call: Ok(
                CallToolRequestParams::new(SHELL).with_arguments(object!({ "command": command }))
            ),
            metadata: None,
            tool_meta: None,
        };
        inspector
            .inspect(session_id, &[request], &[], mode)
            .await
            .unwrap()
            .remove(0)
            .action
    }

    /// Same precedence as the file-level case below, but through the store's
    /// own writer: approving one call must never widen into the tool-wide
    /// `ask_before` the user set with `kaji configure`.
    #[tokio::test]
    async fn a_grant_recorded_over_an_ask_before_still_asks_for_the_rest() {
        let inspector = shell_inspector();
        inspector
            .permission_manager
            .update_user_permission(SHELL, PermissionLevel::AskBefore);
        inspector
            .permission_manager
            .add_grant(SHELL, Some(&Spec::prefix("cargo test")));

        *inspector.readonly_tools.write().unwrap() = [SHELL.to_string()].into_iter().collect();

        assert_eq!(
            inspect_shell_in_mode(
                &inspector,
                SESSION,
                "cargo test --all",
                KajiMode::SmartApprove
            )
            .await,
            InspectionAction::Allow
        );
        assert_eq!(
            inspect_shell_in_mode(&inspector, SESSION, "cargo build", KajiMode::SmartApprove).await,
            InspectionAction::RequireApproval(None)
        );
    }

    #[tokio::test]
    async fn a_narrow_grant_does_not_cancel_a_name_level_ask_before() {
        let config_dir = tempfile::tempdir().unwrap().keep();
        std::fs::write(
            config_dir.join("permission.yaml"),
            "user:\n  always_allow:\n  - shell(cargo test *)\n  ask_before:\n  - shell\n  never_allow: []\n",
        )
        .unwrap();
        let session_manager = Arc::new(crate::session::SessionManager::new(
            tempfile::tempdir().unwrap().keep(),
        ));
        let inspector = PermissionInspector::new(
            Arc::new(PermissionManager::new(config_dir)),
            Arc::new(Mutex::new(None)),
            session_manager,
        );
        *inspector.readonly_tools.write().unwrap() = [SHELL.to_string()].into_iter().collect();

        assert_eq!(
            inspect_shell_in_mode(&inspector, SESSION, "cargo build", KajiMode::SmartApprove).await,
            InspectionAction::RequireApproval(None)
        );
        assert_eq!(
            inspect_shell_in_mode(
                &inspector,
                SESSION,
                "cargo test --all",
                KajiMode::SmartApprove
            )
            .await,
            InspectionAction::Allow
        );
    }

    #[tokio::test]
    async fn a_prefix_grant_does_not_cover_a_line_broken_command() {
        let inspector = shell_inspector();
        inspector
            .permission_manager
            .add_grant(SHELL, Some(&Spec::prefix("cargo test")));

        assert_eq!(
            inspect_shell(&inspector, SESSION, "cargo test\nrm -rf /").await,
            InspectionAction::RequireApproval(None)
        );
    }

    #[tokio::test]
    async fn a_denial_beats_a_session_grant() {
        let inspector = shell_inspector();
        inspector.grant_for_session(SESSION, SHELL, Some(&Spec::prefix("cargo test")));
        inspector
            .permission_manager
            .update_user_permission(SHELL, PermissionLevel::NeverAllow);

        assert_eq!(
            inspect_shell(&inspector, SESSION, "cargo test --all").await,
            InspectionAction::Deny
        );
    }

    #[tokio::test]
    async fn a_session_grant_beats_ask_before() {
        let inspector = shell_inspector();
        inspector
            .permission_manager
            .update_user_permission(SHELL, PermissionLevel::AskBefore);
        inspector.grant_for_session(SESSION, SHELL, Some(&Spec::prefix("cargo test")));

        assert_eq!(
            inspect_shell(&inspector, SESSION, "cargo test --all").await,
            InspectionAction::Allow
        );
        assert_eq!(
            inspect_shell(&inspector, "other-session", "cargo test --all").await,
            InspectionAction::RequireApproval(None)
        );
    }

    #[tokio::test]
    async fn a_persisted_rule_covers_only_matching_stages() {
        let inspector = shell_inspector();
        inspector
            .permission_manager
            .add_grant(SHELL, Some(&Spec::prefix("cargo test")));

        assert_eq!(
            inspect_shell(&inspector, SESSION, "cargo test --all").await,
            InspectionAction::Allow
        );
        assert_eq!(
            inspect_shell(&inspector, SESSION, "cargo test && rm -rf /").await,
            InspectionAction::RequireApproval(None)
        );
    }

    #[tokio::test]
    async fn a_legacy_tool_name_grant_covers_every_call() {
        let inspector = shell_inspector();
        inspector
            .permission_manager
            .update_user_permission(SHELL, PermissionLevel::AlwaysAllow);

        assert_eq!(
            inspect_shell(&inspector, SESSION, "rm -rf /").await,
            InspectionAction::Allow
        );
    }

    #[test]
    fn smart_approve_only_caches_negative_name_wide_decisions() {
        let pm = PermissionManager::new(tempfile::tempdir().unwrap().keep());
        let req = ToolRequest {
            id: "read-request".into(),
            tool_call: Ok(
                CallToolRequestParams::new("multipurpose").with_arguments(object!({
                    "command": "view status",
                })),
            ),
            metadata: None,
            tool_meta: None,
        };

        cache_non_readonly_decision(&pm, &req, true);
        assert_eq!(pm.get_smart_approve_permission("multipurpose"), None);

        cache_non_readonly_decision(&pm, &req, false);
        assert_eq!(
            pm.get_smart_approve_permission("multipurpose"),
            Some(PermissionLevel::AskBefore)
        );
    }
}
