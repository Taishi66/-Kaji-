//! Applies the persistence side of a tool approval, shared by both agent loops.

use rmcp::model::JsonObject;

use crate::config::permission::PermissionLevel;
use crate::permission::grants::derive_grant_spec;
use crate::permission::Permission;
use crate::tool_inspection::ToolInspectionManager;

pub async fn apply_grant_decision(
    tool_inspection_manager: &ToolInspectionManager,
    tool_name: &str,
    arguments: Option<&JsonObject>,
    permission: &Permission,
    session_id: &str,
) {
    match permission {
        Permission::AllowSession => {
            let spec = derive_grant_spec(tool_name, arguments);
            tool_inspection_manager.grant_for_session(session_id, tool_name, spec.as_deref());
        }
        Permission::AlwaysAllow => {
            let spec = derive_grant_spec(tool_name, arguments);
            tool_inspection_manager
                .add_user_grant(tool_name, spec.as_deref())
                .await;
        }
        Permission::AlwaysDeny => {
            tool_inspection_manager
                .update_permission_manager(tool_name, PermissionLevel::NeverAllow)
                .await;
        }
        Permission::AllowOnce | Permission::DenyOnce | Permission::Cancel => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::KajiMode;
    use crate::config::PermissionManager;
    use crate::conversation::message::ToolRequest;
    use crate::permission::grants::GrantRule;
    use crate::permission::PermissionInspector;
    use crate::tool_inspection::InspectionAction;
    use rmcp::model::CallToolRequestParams;
    use rmcp::object;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    const SESSION: &str = "session-1";
    const SHELL: &str = "shell";

    struct Fixture {
        manager: ToolInspectionManager,
        permission_manager: Arc<PermissionManager>,
    }

    fn fixture() -> Fixture {
        let permission_manager =
            Arc::new(PermissionManager::new(tempfile::tempdir().unwrap().keep()));
        let session_manager = Arc::new(crate::session::SessionManager::new(
            tempfile::tempdir().unwrap().keep(),
        ));
        let mut manager = ToolInspectionManager::new();
        manager.add_inspector(Box::new(PermissionInspector::new(
            Arc::clone(&permission_manager),
            Arc::new(Mutex::new(None)),
            session_manager,
        )));
        Fixture {
            manager,
            permission_manager,
        }
    }

    impl Fixture {
        async fn decide(&self, command: &str, permission: Permission) {
            apply_grant_decision(
                &self.manager,
                SHELL,
                Some(&object!({ "command": command })),
                &permission,
                SESSION,
            )
            .await;
        }

        async fn inspect(&self, session_id: &str, command: &str) -> InspectionAction {
            let request = ToolRequest {
                id: "req".into(),
                tool_call: Ok(CallToolRequestParams::new(SHELL)
                    .with_arguments(object!({ "command": command }))),
                metadata: None,
                tool_meta: None,
            };
            self.manager
                .inspect_tools(session_id, &[request], &[], KajiMode::Approve)
                .await
                .unwrap()
                .remove(0)
                .action
        }
    }

    #[tokio::test]
    async fn always_allow_persists_a_two_token_prefix_rule() {
        let fixture = fixture();
        fixture
            .decide("cargo test -p kaji", Permission::AlwaysAllow)
            .await;

        assert_eq!(
            fixture.permission_manager.get_user_grants(),
            vec![GrantRule::new(SHELL, Some("cargo test *"))]
        );
        assert_eq!(fixture.permission_manager.get_user_permission(SHELL), None);
        assert_eq!(
            fixture.inspect(SESSION, "cargo test --all").await,
            InspectionAction::Allow
        );
        assert_eq!(
            fixture.inspect(SESSION, "cargo build").await,
            InspectionAction::RequireApproval(None)
        );
    }

    #[tokio::test]
    async fn always_allow_keeps_tool_wide_grants_for_tools_without_a_primary_argument() {
        let fixture = fixture();
        apply_grant_decision(
            &fixture.manager,
            "other__tool",
            Some(&object!({ "value": 1 })),
            &Permission::AlwaysAllow,
            SESSION,
        )
        .await;

        assert_eq!(
            fixture.permission_manager.get_user_grants(),
            vec![GrantRule::new("other__tool", None)]
        );
        assert_eq!(
            fixture
                .permission_manager
                .get_user_permission("other__tool"),
            Some(PermissionLevel::AlwaysAllow)
        );
    }

    #[tokio::test]
    async fn allow_session_grants_without_persisting() {
        let fixture = fixture();
        fixture
            .decide("cargo test -p kaji", Permission::AllowSession)
            .await;

        assert!(fixture.permission_manager.get_user_grants().is_empty());
        assert_eq!(
            fixture.inspect(SESSION, "cargo test --all").await,
            InspectionAction::Allow
        );
        assert_eq!(
            fixture.inspect("other-session", "cargo test --all").await,
            InspectionAction::RequireApproval(None)
        );
        assert_eq!(
            fixture.inspect(SESSION, "cargo test && rm -rf /").await,
            InspectionAction::RequireApproval(None)
        );
    }

    #[tokio::test]
    async fn always_deny_denies_the_whole_tool_over_a_session_grant() {
        let fixture = fixture();
        fixture
            .decide("cargo test -p kaji", Permission::AllowSession)
            .await;
        fixture
            .decide("cargo test -p kaji", Permission::AlwaysDeny)
            .await;

        assert_eq!(
            fixture.permission_manager.get_user_permission(SHELL),
            Some(PermissionLevel::NeverAllow)
        );
        assert_eq!(
            fixture.inspect(SESSION, "cargo test --all").await,
            InspectionAction::Deny
        );
    }

    #[tokio::test]
    async fn allow_once_grants_nothing() {
        let fixture = fixture();
        fixture
            .decide("cargo test -p kaji", Permission::AllowOnce)
            .await;

        assert!(fixture.permission_manager.get_user_grants().is_empty());
        assert_eq!(
            fixture.inspect(SESSION, "cargo test -p kaji").await,
            InspectionAction::RequireApproval(None)
        );
    }
}
