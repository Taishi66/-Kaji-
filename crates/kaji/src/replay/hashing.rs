use rmcp::model::{CallToolResult, Role, Tool};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::conversation::message::{Message, MessageContent, ToolResponse};
use crate::utils::bytes_to_hex;

#[derive(Serialize)]
struct NormalizedRequest<'a> {
    system: &'a str,
    messages: Vec<(Role, Vec<MessageContent>)>,
    tools: &'a [Tool],
}

/// SHA-256 hex de la requête telle que le provider la reçoit, sous forme
/// normalisée : tout ce qui varie d'un enregistrement à son rejeu sans changer
/// ce que le modèle lit est écarté — `Message::id`, `Message::created`,
/// `MessageMetadata`, le `_meta` de routage des `ToolRequest` et les drapeaux
/// de `CallToolResult` que les providers ne renvoient pas tous.
///
/// Clé de vérification du replay strict : l'écart entre le hash journalisé dans
/// `llm_request` et celui de la requête reconstruite arrête le rejeu
/// (`docs/superpowers/specs/2026-08-27-event-log-v2-replay-exact-design.md`).
pub fn request_hash(system: &str, messages: &[Message], tools: &[Tool]) -> String {
    let normalized = NormalizedRequest {
        system,
        messages: normalized_messages(messages),
        tools,
    };
    let serialized = serde_json::to_string(&normalized).unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(serialized.as_bytes());
    bytes_to_hex(hasher.finalize())
}

/// Même dépouillement de contenu que `TestProvider::hash_input`
/// (`providers/testprovider.rs`) — les deux sont croisés par
/// `normalization_agrees_with_testprovider`.
fn normalized_messages(messages: &[Message]) -> Vec<(Role, Vec<MessageContent>)> {
    messages
        .iter()
        .map(|message| {
            let mut cleaned_content: Vec<_> = message.content.to_vec();

            for content in &mut cleaned_content {
                match content {
                    MessageContent::ToolRequest(ref mut req) => {
                        req.tool_meta = None;
                    }
                    MessageContent::ToolResponse(ToolResponse {
                        tool_result:
                            Ok(
                                ref mut result @ CallToolResult {
                                    is_error: Some(false),
                                    ..
                                },
                            ),
                        ..
                    }) => {
                        result.is_error = None;
                        result.result_type = None;
                    }
                    _ => {}
                }
            }
            (message.role.clone(), cleaned_content)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::testprovider::TestProvider;
    use rmcp::model::{CallToolRequestParams, ContentBlock};
    use serde_json::json;

    fn tool_result(flagged: bool) -> CallToolResult {
        let mut result = CallToolResult::success(vec![ContentBlock::text("4")]);
        if !flagged {
            result.is_error = None;
            result.result_type = None;
        }
        result
    }

    fn conversation() -> Vec<Message> {
        vec![
            Message::user().with_text("compute 2 + 2"),
            Message::assistant()
                .with_tool_request("call-1", Ok(CallToolRequestParams::new("calculator__add"))),
            Message::user().with_tool_response("call-1", Ok(tool_result(false))),
        ]
    }

    /// Same conversation, rebuilt the way a replayed turn would produce it:
    /// fresh ids and timestamps, routing `_meta` attached, tool result flagged
    /// by a provider that sets `is_error: Some(false)`.
    fn conversation_with_volatile_fields() -> Vec<Message> {
        let mut messages = vec![
            Message::user().with_text("compute 2 + 2").with_id("msg_a"),
            Message::assistant()
                .with_tool_request_with_metadata(
                    "call-1",
                    Ok(CallToolRequestParams::new("calculator__add")),
                    None,
                    Some(json!({ "kaji_extension": "calculator" })),
                )
                .with_id("msg_b"),
            Message::user()
                .with_tool_response("call-1", Ok(tool_result(true)))
                .with_id("msg_c"),
        ];
        for (offset, message) in messages.iter_mut().enumerate() {
            message.created = 1_700_000_000 + offset as i64;
        }
        messages
    }

    #[test]
    fn hash_is_a_sha256_hex_digest() {
        let hash = request_hash("system", &conversation(), &[]);
        assert_eq!(hash.len(), 64);
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn volatile_fields_do_not_move_the_hash() {
        assert_eq!(
            request_hash("system", &conversation(), &[]),
            request_hash("system", &conversation_with_volatile_fields(), &[])
        );
    }

    #[test]
    fn system_prompt_is_part_of_the_request() {
        assert_ne!(
            request_hash("system", &conversation(), &[]),
            request_hash("other system", &conversation(), &[])
        );
    }

    #[test]
    fn tools_are_part_of_the_request() {
        let tools = vec![Tool::new(
            "calculator__add",
            "add two numbers",
            serde_json::Map::new(),
        )];
        assert_ne!(
            request_hash("system", &conversation(), &[]),
            request_hash("system", &conversation(), &tools)
        );
    }

    #[test]
    fn message_text_moves_the_hash() {
        let mut altered = conversation();
        altered[0] = Message::user().with_text("compute 2 + 3");
        assert_ne!(
            request_hash("system", &conversation(), &[]),
            request_hash("system", &altered, &[])
        );
    }

    /// Cross-check against the prior art this normalization was lifted from:
    /// `TestProvider::hash_input` keys its recorded scenarios on the same
    /// stripped view of the messages. The two hashes are deliberately
    /// different values (only `request_hash` covers the system prompt and the
    /// tools), so what is asserted here is that they agree on *which*
    /// differences matter — a divergence would mean one of the two strips
    /// drifted.
    #[test]
    fn normalization_agrees_with_testprovider() {
        let stable = conversation();
        let volatile = conversation_with_volatile_fields();
        let mut altered = conversation();
        altered[0] = Message::user().with_text("compute 2 + 3");

        assert_eq!(
            TestProvider::hash_input(&stable),
            TestProvider::hash_input(&volatile)
        );
        assert_eq!(
            request_hash("system", &stable, &[]),
            request_hash("system", &volatile, &[])
        );

        assert_ne!(
            TestProvider::hash_input(&stable),
            TestProvider::hash_input(&altered)
        );
        assert_ne!(
            request_hash("system", &stable, &[]),
            request_hash("system", &altered, &[])
        );
    }
}
