use async_stream::try_stream;
use futures::stream::{self, BoxStream};
use futures::{Stream, StreamExt};
use rmcp::model::CallToolResult;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use std::path::PathBuf;

use crate::conversation::message::Message;
use crate::mcp_utils::ToolResult;
use crate::permission::grant_decision::apply_grant_decision;
use rmcp::model::{ContentBlock, ServerNotification};

#[derive(Clone)]
pub(crate) struct ToolCallNotificationEmitter {
    sender: mpsc::Sender<ServerNotification>,
}

impl ToolCallNotificationEmitter {
    pub(crate) fn new(sender: mpsc::Sender<ServerNotification>) -> Self {
        Self { sender }
    }

    pub(crate) fn emit_best_effort(&self, notification: ServerNotification) {
        // Do not let a slow notification consumer delay tool execution.
        let _ = self.sender.try_send(notification);
    }
}

/// Context passed through the tool call dispatch chain.
#[derive(Clone)]
pub struct ToolCallContext {
    pub session_id: String,
    pub working_dir: Option<PathBuf>,
    pub tool_call_request_id: Option<String>,
    notification_emitter: Option<ToolCallNotificationEmitter>,
}

impl ToolCallContext {
    pub fn new(
        session_id: String,
        working_dir: Option<PathBuf>,
        tool_call_request_id: Option<String>,
    ) -> Self {
        Self {
            session_id,
            working_dir,
            tool_call_request_id,
            notification_emitter: None,
        }
    }

    pub fn working_dir_str(&self) -> Option<&str> {
        self.working_dir.as_ref().and_then(|p| p.to_str())
    }

    pub(crate) fn with_notification_emitter(
        mut self,
        notification_emitter: ToolCallNotificationEmitter,
    ) -> Self {
        self.notification_emitter = Some(notification_emitter);
        self
    }

    pub(crate) fn notification_emitter(&self) -> Option<&ToolCallNotificationEmitter> {
        self.notification_emitter.as_ref()
    }
}

// ToolCallResult combines the result of a tool call with an optional notification stream that
// can be used to receive notifications from the tool.
pub struct ToolCallResult {
    pub result: Box<dyn Future<Output = ToolResult<rmcp::model::CallToolResult>> + Send + Unpin>,
    pub notification_stream: Option<Box<dyn Stream<Item = ServerNotification> + Send + Unpin>>,
    pub action_required_stream: Option<Box<dyn Stream<Item = Message> + Send + Unpin>>,
}

impl From<ToolResult<rmcp::model::CallToolResult>> for ToolCallResult {
    fn from(result: ToolResult<rmcp::model::CallToolResult>) -> Self {
        Self {
            result: Box::new(futures::future::ready(result)),
            notification_stream: None,
            action_required_stream: None,
        }
    }
}

use crate::agents::Agent;
use crate::conversation::message::ToolRequest;
use crate::session::Session;
use crate::tool_inspection::get_security_finding_id_from_results;

pub(super) enum ToolStreamItem<T> {
    ActionRequired(Message),
    Message(ServerNotification),
    Result(T),
}

pub(super) type ToolStream =
    Pin<Box<dyn Stream<Item = ToolStreamItem<ToolResult<CallToolResult>>> + Send>>;

pub(super) fn tool_stream<S, A, F>(rx: S, action_required_rx: A, done: F) -> ToolStream
where
    S: Stream<Item = ServerNotification> + Send + Unpin + 'static,
    A: Stream<Item = Message> + Send + Unpin + 'static,
    F: Future<Output = ToolResult<CallToolResult>> + Send + 'static,
{
    Box::pin(async_stream::stream! {
        tokio::pin!(done);
        let mut rx = rx;
        let mut action_required_rx = action_required_rx;

        loop {
            tokio::select! {
                Some(msg) = action_required_rx.next() => {
                    yield ToolStreamItem::ActionRequired(msg);
                }
                Some(msg) = rx.next() => {
                    yield ToolStreamItem::Message(msg);
                }
                r = &mut done => {
                    yield ToolStreamItem::Result(r);
                    break;
                }
            }
        }
    })
}

pub const DECLINED_RESPONSE: &str = "The user has declined to run this tool. \
    DO NOT attempt to call this tool again. \
    If there are no alternative methods to proceed, clearly explain the situation and STOP.";

pub const CHAT_MODE_TOOL_SKIPPED_RESPONSE: &str = "Let the user know the tool call was skipped in kaji chat mode. \
                                        DO NOT apologize for skipping the tool call. DO NOT say sorry. \
                                        Provide an explanation of what the tool call would do, structured as a \
                                        plan for the user. Again, DO NOT apologize. \
                                        **Example Plan:**\n \
                                        1. **Identify Task Scope** - Determine the purpose and expected outcome.\n \
                                        2. **Outline Steps** - Break down the steps.\n \
                                        If needed, adjust the explanation based on user preferences or questions.";

impl Agent {
    pub(super) fn handle_approval_tool_requests<'a>(
        &'a self,
        tool_requests: &'a [ToolRequest],
        tool_futures: &'a mut Vec<(String, ToolStream)>,
        request_to_response_map: &'a mut HashMap<String, Message>,
        cancellation_token: Option<CancellationToken>,
        session: &'a Session,
        inspection_results: &'a [crate::tool_inspection::InspectionResult],
    ) -> BoxStream<'a, anyhow::Result<Message>> {
        try_stream! {
        for request in tool_requests.iter() {
            if let Ok(tool_call) = request.tool_call.clone() {
                let security_message = inspection_results.iter()
                    .find(|result| result.tool_request_id == request.id)
                    .and_then(|result| {
                        if let crate::tool_inspection::InspectionAction::RequireApproval(Some(message)) = &result.action {
                            Some(message.clone())
                        } else {
                            None
                        }
                    });

                let confirmation_rx = self.tool_confirmation_router.register(request.id.clone()).await;

                let action_required_msg = Message::assistant()
                    .with_action_required(
                        request.id.clone(),
                        tool_call.name.to_string().clone(),
                        tool_call.arguments.clone().unwrap_or_default(),
                        security_message,
                    )
                    .user_only();
                yield action_required_msg;

                let confirmation = confirmation_rx.await
                    .map_err(|_| anyhow::anyhow!("Confirmation channel closed for request {}", request.id))?;

                if let Some(finding_id) = get_security_finding_id_from_results(&request.id, inspection_results) {
                    let action = if confirmation.permission.allows_execution() { "ALLOW" } else { "BLOCK" };
                    tracing::info!(
                        monotonic_counter.kaji.prompt_injection_user_decisions = 1,
                        security.event_type = "user_decision",
                        security.action = action,
                        security.finding_id = %finding_id,
                        tool.request_id = %request.id,
                        user.decision = ?confirmation.permission,
                        "security finding: user decision"
                    );
                }

                apply_grant_decision(
                    &self.tool_inspection_manager,
                    &tool_call.name,
                    tool_call.arguments.as_ref(),
                    &confirmation.permission,
                    &session.id,
                ).await;

                if confirmation.permission.allows_execution() {
                    let (req_id, tool_result) = self.dispatch_tool_call(tool_call.clone(), request.id.clone(), cancellation_token.clone(), session).await;

                    tool_futures.push((req_id, match tool_result {
                        Ok(result) => tool_stream(
                            result.notification_stream.unwrap_or_else(|| Box::new(stream::empty())),
                            result.action_required_stream.unwrap_or_else(|| Box::new(stream::empty())),
                            result.result,
                        ),
                        Err(e) => tool_stream(
                            Box::new(stream::empty()),
                            Box::new(stream::empty()),
                            futures::future::ready(Err(e)),
                        ),
                    }));
                } else {
                    if let Some(response) = request_to_response_map.get_mut(&request.id) {
                        response.add_tool_response_with_metadata(
                            request.id.clone(),
                            Ok(CallToolResult::error(vec![ContentBlock::text(DECLINED_RESPONSE)])),
                            request.metadata.as_ref(),
                        );
                    }
                }
            }
        }
    }.boxed()
    }

    pub(crate) fn handle_frontend_tool_request<'a>(
        &'a self,
        tool_request: &'a ToolRequest,
        message_tool_response: &'a mut Message,
    ) -> BoxStream<'a, anyhow::Result<Message>> {
        try_stream! {
                if let Ok(tool_call) = tool_request.tool_call.clone() {
                    if self.is_frontend_tool(&tool_call.name).await {
                        yield Message::assistant().with_frontend_tool_request(
                            tool_request.id.clone(),
                            Ok(tool_call.clone())
                        );

                        if let Some((id, result)) = self.tool_result_rx.lock().await.recv().await {
                            message_tool_response.add_tool_response_with_metadata(
                                id,
                                result,
                                tool_request.metadata.as_ref(),
                            );
                        }
                    }
            }
        }
        .boxed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::{AgentConfig, AgentEvent, KajiPlatform, SessionConfig};
    use crate::config::permission::PermissionManager;
    use crate::config::KajiMode;
    use crate::conversation::message::{ActionRequiredData, MessageContent};
    use crate::permission::permission_confirmation::PrincipalType;
    use crate::permission::{Permission, PermissionConfirmation};
    use crate::session::session_manager::SessionType;
    use crate::session::SessionManager;
    use futures::StreamExt;
    use kaji_providers::base::{stream_from_single_message, MessageStream, Provider};
    use kaji_providers::conversation::token_usage::{ProviderUsage, Usage};
    use kaji_providers::errors::ProviderError;
    use kaji_providers::model::ModelConfig;
    use rmcp::model::{CallToolRequestParams, Tool};
    use rmcp::object;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex as StdMutex};

    struct ScriptedProvider {
        responses: StdMutex<VecDeque<Message>>,
    }

    impl ScriptedProvider {
        fn new(responses: Vec<Message>) -> Self {
            Self {
                responses: StdMutex::new(responses.into()),
            }
        }
    }

    #[async_trait::async_trait]
    impl Provider for ScriptedProvider {
        fn get_name(&self) -> &str {
            "scripted"
        }

        async fn stream(
            &self,
            _: &ModelConfig,
            _: &str,
            _: &[Message],
            _: &[Tool],
        ) -> std::result::Result<MessageStream, ProviderError> {
            let message = self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Message::assistant().with_text("nothing left to do"));
            let usage = ProviderUsage::new("scripted".to_string(), Usage::default());
            Ok(stream_from_single_message(message, usage))
        }
    }

    struct Approval {
        agent: Agent,
        permission_manager: Arc<PermissionManager>,
        session_id: String,
    }

    async fn approve_shell_call(command: &str, permission: Permission) -> Approval {
        let permission_manager =
            Arc::new(PermissionManager::new(tempfile::tempdir().unwrap().keep()));
        let session_manager = Arc::new(SessionManager::new(tempfile::tempdir().unwrap().keep()));
        let agent = Agent::with_config(AgentConfig::new(
            Arc::clone(&session_manager),
            Arc::clone(&permission_manager),
            None,
            KajiMode::Approve,
            true,
            KajiPlatform::KajiCli,
        ));
        let session = session_manager
            .create_session(
                PathBuf::default(),
                "legacy-approval".to_string(),
                SessionType::Hidden,
                KajiMode::Approve,
            )
            .await
            .unwrap();

        let tool_call =
            CallToolRequestParams::new("shell").with_arguments(object!({ "command": command }));
        let provider = Arc::new(ScriptedProvider::new(vec![
            Message::assistant().with_tool_request("call-1", Ok(tool_call))
        ]));
        agent
            .update_provider(provider, ModelConfig::new("scripted-model"), &session.id)
            .await
            .unwrap();

        {
            let stream = agent
                .reply(
                    Message::user().with_text("run it"),
                    SessionConfig {
                        id: session.id.clone(),
                        schedule_id: None,
                        max_turns: Some(2),
                        retry_config: None,
                    },
                    None,
                )
                .await
                .unwrap();
            tokio::pin!(stream);

            while let Some(event) = stream.next().await {
                if let Ok(AgentEvent::Message(message)) = event {
                    for content in &message.content {
                        if let MessageContent::ActionRequired(action) = content {
                            if let ActionRequiredData::ToolConfirmation { id, .. } = &action.data {
                                agent
                                    .handle_confirmation(
                                        id.clone(),
                                        PermissionConfirmation {
                                            principal_type: PrincipalType::Tool,
                                            permission: permission.clone(),
                                        },
                                    )
                                    .await;
                            }
                        }
                    }
                }
            }
        }

        Approval {
            agent,
            permission_manager,
            session_id: session.id,
        }
    }

    impl Approval {
        fn grants(&self) -> Vec<String> {
            self.permission_manager
                .get_user_grants()
                .iter()
                .map(ToString::to_string)
                .collect()
        }

        async fn inspect(&self, command: &str) -> crate::tool_inspection::InspectionAction {
            let request = ToolRequest {
                id: "req".into(),
                tool_call: Ok(CallToolRequestParams::new("shell")
                    .with_arguments(object!({ "command": command }))),
                metadata: None,
                tool_meta: None,
            };
            self.agent
                .tool_inspection_manager
                .inspect_tools(&self.session_id, &[request], &[], KajiMode::Approve)
                .await
                .unwrap()
                .remove(0)
                .action
        }
    }

    #[tokio::test]
    async fn always_allow_persists_a_command_prefix_rule() {
        let approval = approve_shell_call("cargo test -p kaji", Permission::AlwaysAllow).await;

        assert_eq!(approval.grants(), ["shell(cargo test *)"]);
        assert_eq!(
            approval.permission_manager.get_user_permission("shell"),
            None
        );
    }

    #[tokio::test]
    async fn always_deny_still_denies_the_whole_tool() {
        let approval = approve_shell_call("cargo test -p kaji", Permission::AlwaysDeny).await;

        assert_eq!(
            approval.permission_manager.get_user_permission("shell"),
            Some(crate::config::permission::PermissionLevel::NeverAllow)
        );
    }

    #[tokio::test]
    async fn allow_session_grants_without_persisting() {
        let approval = approve_shell_call("cargo test -p kaji", Permission::AllowSession).await;

        assert!(approval.grants().is_empty());
        assert_eq!(
            approval.inspect("cargo test --all").await,
            crate::tool_inspection::InspectionAction::Allow
        );
        assert_eq!(
            approval.inspect("cargo build").await,
            crate::tool_inspection::InspectionAction::RequireApproval(None)
        );
    }
}
