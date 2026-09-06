//! Le doré du rejeu d'un tour multimodal (S1) : une image attachée entre dans
//! la requête LLM comme le reste du message, donc elle doit entrer dans le
//! `request_hash` et ressortir du journal au rejeu.
//!
//! Un doré **dédié et minimal** plutôt qu'une extension du gros doré : ce qui
//! est cloué ici tient en trois phrases, et l'ajouter aux 1400 lignes de
//! `replay_golden_test` rendrait les deux plus difficiles à lire qu'ils ne le
//! sont séparément.
//!
//! Trois propriétés :
//! - l'image voyage jusqu'au provider, en base64 et avec son mime ;
//! - le `request_hash` journalisé en **dépend** — changer un octet de l'image
//!   change le hash, donc un rejeu qui perdrait l'image serait arrêté au lieu
//!   d'être resservi ;
//! - le rejeu strict resert les chunks enregistrés à partir de la requête qui
//!   porte l'image.

use anyhow::Result;
use async_trait::async_trait;
use futures::StreamExt;
use kaji::agents::{Agent, AgentConfig, KajiPlatform, SessionConfig};
use kaji::config::permission::PermissionManager;
use kaji::config::KajiMode;
use kaji::conversation::message::{Message, MessageContent};
use kaji::providers::base::{stream_from_single_message, MessageStream, Provider};
use kaji::replay::cursor::EventCursor;
use kaji::replay::hashing::request_hash;
use kaji::replay::provider::ReplayProvider;
use kaji::session::session_manager::SessionType;
use kaji::session::SessionManager;
use kaji_providers::conversation::token_usage::{ProviderUsage, Usage};
use kaji_providers::errors::ProviderError;
use kaji_providers::model::ModelConfig;
use rmcp::model::Tool;
use serde_json::Value;
use std::sync::{Arc, Mutex};

/// Base64 d'un PNG minuscule et d'un second qui n'en diffère que par ses
/// octets : c'est cette différence-là que le hash doit voir.
const IMAGE: &str = "iVBORw0KGgoAAAANSUhEUg==";
const OTHER_IMAGE: &str = "iVBORw0KGgoAAAANSUhEUh==";
const MIME: &str = "image/png";
const PROMPT: &str = "que montre cette capture ?";

/// Ce que le provider a reçu : les arguments exacts sur lesquels le hash
/// journalisé a été calculé.
#[derive(Clone)]
struct RecordedCall {
    system: String,
    messages: Vec<Message>,
    tools: Vec<Tool>,
}

struct FixtureProvider {
    calls: Arc<Mutex<Vec<RecordedCall>>>,
}

#[async_trait]
impl Provider for FixtureProvider {
    async fn stream(
        &self,
        _model_config: &ModelConfig,
        system: &str,
        messages: &[Message],
        tools: &[Tool],
    ) -> Result<MessageStream, ProviderError> {
        self.calls.lock().unwrap().push(RecordedCall {
            system: system.to_string(),
            messages: messages.to_vec(),
            tools: tools.to_vec(),
        });
        Ok(stream_from_single_message(
            Message::assistant().with_text("une capture d'écran"),
            ProviderUsage::new(
                "mock-model".to_string(),
                Usage::new(Some(7), Some(3), Some(10)),
            ),
        ))
    }

    fn get_name(&self) -> &str {
        "vision-mock"
    }
}

async fn drain(
    stream: impl futures::Stream<Item = Result<kaji::agents::AgentEvent>>,
) -> Result<()> {
    tokio::pin!(stream);
    while let Some(event) = stream.next().await {
        event?;
    }
    Ok(())
}

async fn collect_chunks(stream: MessageStream) -> Result<Vec<Value>> {
    stream
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .map(|chunk| {
            let (message, usage) = chunk.map_err(anyhow::Error::from)?;
            Ok(serde_json::json!([message, usage]))
        })
        .collect()
}

/// Les images portées par une liste de messages, dans l'ordre.
fn images_of(messages: &[Message]) -> Vec<(String, String)> {
    messages
        .iter()
        .flat_map(|message| message.content.iter())
        .filter_map(|content| match content {
            MessageContent::Image(image) => Some((image.data.clone(), image.mime_type.clone())),
            _ => None,
        })
        .collect()
}

/// Remplace l'image du message user par une autre, sans rien toucher d'autre :
/// la seule variable du test de sensibilité du hash.
fn with_other_image(messages: &[Message]) -> Vec<Message> {
    messages
        .iter()
        .cloned()
        .map(|mut message| {
            for content in message.content.iter_mut() {
                if let MessageContent::Image(image) = content {
                    image.data = OTHER_IMAGE.to_string();
                }
            }
            message
        })
        .collect()
}

async fn record_one_multimodal_turn(state_machine: Option<&str>) -> Result<()> {
    let label = format!("KAJI_STATE_MACHINE={state_machine:?}");
    let _guard = env_lock::lock_env([("KAJI_STATE_MACHINE", state_machine)]);

    let data_dir = tempfile::tempdir()?;
    let working_dir = data_dir.path().join("workspace");
    std::fs::create_dir_all(&working_dir)?;

    let session_manager = Arc::new(SessionManager::new(data_dir.path().join("data")));
    let agent = Agent::with_config(AgentConfig::new(
        Arc::clone(&session_manager),
        Arc::new(PermissionManager::new(data_dir.path().join("config"))),
        None,
        KajiMode::Auto,
        true,
        KajiPlatform::KajiCli,
    ));
    let session = session_manager
        .create_session(
            working_dir,
            "vision-replay-golden".to_string(),
            SessionType::Hidden,
            KajiMode::Auto,
        )
        .await?;

    let provider = Arc::new(FixtureProvider {
        calls: Arc::new(Mutex::new(Vec::new())),
    });
    let calls = Arc::clone(&provider.calls);
    agent
        .update_provider(provider, ModelConfig::new("mock-model"), &session.id)
        .await?;

    drain(
        agent
            .reply(
                Message::user().with_text(PROMPT).with_image(IMAGE, MIME),
                SessionConfig {
                    id: session.id.clone(),
                    schedule_id: None,
                    max_turns: Some(2),
                    retry_config: None,
                },
                None,
            )
            .await?,
    )
    .await?;

    // Les deux boucles n'appellent pas le provider le même nombre de fois — la
    // boucle historique glisse une génération de titre hors du tour, qui n'est
    // pas journalisée. L'appel qui compte ici est celui qui porte l'image, et
    // il s'identifie par son contenu puis se retrouve au journal par son hash,
    // jamais par sa position dans la liste.
    let calls = calls.lock().unwrap().clone();
    let with_image: Vec<&RecordedCall> = calls
        .iter()
        .filter(|call| !images_of(&call.messages).is_empty())
        .collect();
    assert_eq!(
        with_image.len(),
        1,
        "{label}: un seul appel porte l'image du tour"
    );
    let call = with_image[0];
    assert_eq!(
        images_of(&call.messages),
        vec![(IMAGE.to_string(), MIME.to_string())],
        "{label}: l'image attachée arrive au provider telle qu'elle a été encodée"
    );

    let cursor = Arc::new(EventCursor::load(&session_manager, &session.id).await?);
    let hash = request_hash(&call.system, &call.messages, &call.tools);
    let position = cursor
        .llm_responses
        .iter()
        .find(|(_, exchange)| exchange.request_hash == hash)
        .map(|(position, _)| *position)
        .unwrap_or_else(|| {
            panic!("{label}: aucun llm_request ne porte le hash de la requête à l'image")
        });
    assert_eq!(
        position.1, 0,
        "{label}: l'appel qui porte l'image ouvre son tour"
    );

    let altered = request_hash(&call.system, &with_other_image(&call.messages), &call.tools);
    assert_ne!(
        altered, hash,
        "{label}: changer l'image change le hash — sinon un rejeu qui la perd passerait"
    );
    assert!(
        !cursor
            .llm_responses
            .values()
            .any(|exchange| exchange.request_hash == altered),
        "{label}: aucun échange enregistré ne répondrait à une autre image"
    );

    // Rejeu strict : la requête qui porte l'image retrouve ses chunks…
    let model_config = ModelConfig::new("mock-model");
    let strict = ReplayProvider::new(Arc::clone(&cursor), false);
    strict.position().begin_turn(position.0);
    let served = collect_chunks(
        strict
            .stream(&model_config, &call.system, &call.messages, &call.tools)
            .await
            .unwrap_or_else(|error| panic!("{label}: le rejeu strict sert le tour image: {error}")),
    )
    .await?;
    assert!(
        !served.is_empty(),
        "{label}: le rejeu rend les chunks enregistrés de l'appel image"
    );

    // … et la même requête à une image près est arrêtée, au lieu de recevoir
    // un échange qui n'est pas le sien.
    let strict = ReplayProvider::new(Arc::clone(&cursor), false);
    strict.position().begin_turn(position.0);
    let refused = strict
        .stream(
            &model_config,
            &call.system,
            &with_other_image(&call.messages),
            &call.tools,
        )
        .await;
    assert!(
        refused.is_err(),
        "{label}: une image différente est une requête différente, le rejeu strict s'arrête"
    );

    Ok(())
}

#[tokio::test]
async fn an_attached_image_is_hashed_and_replayed_on_the_legacy_loop() -> Result<()> {
    record_one_multimodal_turn(None).await
}

#[tokio::test]
async fn an_attached_image_is_hashed_and_replayed_on_the_state_machine() -> Result<()> {
    record_one_multimodal_turn(Some("1")).await
}
