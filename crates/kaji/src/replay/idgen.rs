use sha2::{Digest, Sha256};
use std::sync::{Arc, Mutex};

use crate::utils::bytes_to_hex;

pub trait IdGen: Send + Sync {
    fn next_message_id(&self) -> String;

    /// Ouvre le tour : les ids repartent de zéro sous la graine de la session
    /// et le rang du tour. C'est ce qui rend l'enregistrement redérivable —
    /// une session reprise dans un autre processus ne redonne pas les ids de
    /// son premier tour, et le rejeu retrouve chaque tour à sa place sans
    /// dépendre de ce qui a été consommé avant lui.
    fn begin_turn(&self, _seed: &str, _turn_seq: i64) {}
}

/// Ids de message dérivés de `(graine, tour, rang)` — spec S1 : le journal n'a
/// pas à porter les ids, le rejeu les redérive.
pub struct SessionIdGen {
    /// La graine imposée par le rejeu (`log_meta.idgen_seed`) : il tourne dans
    /// une session dérivée et doit redonner les ids de la session source.
    /// Absente à l'enregistrement, où chaque tour adopte celle de sa session.
    pinned_seed: Option<String>,
    scope: Mutex<Scope>,
}

struct Scope {
    seed: String,
    turn_seq: i64,
    counter: u64,
}

impl SessionIdGen {
    pub fn new(seed: &str) -> Self {
        Self {
            pinned_seed: Some(seed.to_string()),
            scope: Mutex::new(Scope {
                seed: seed.to_string(),
                turn_seq: -1,
                counter: 0,
            }),
        }
    }

    /// L'enregistrement : la graine est celle de la session du tour, connue
    /// seulement quand le tour s'ouvre. Hors tour — un message nommé avant le
    /// premier `begin_turn` — la graine reste aléatoire, comme avant : deux
    /// agents du même processus ne doivent pas se partager une suite d'ids.
    #[allow(clippy::disallowed_methods)] // graine hors tour, remplacée dès le premier begin_turn
    pub fn per_session() -> Self {
        Self {
            pinned_seed: None,
            scope: Mutex::new(Scope {
                seed: uuid::Uuid::new_v4().to_string(),
                turn_seq: -1,
                counter: 0,
            }),
        }
    }
}

impl IdGen for SessionIdGen {
    fn next_message_id(&self) -> String {
        let mut scope = self.scope.lock().expect("idgen scope");
        let n = scope.counter;
        scope.counter += 1;
        let mut h = Sha256::new();
        h.update(scope.seed.as_bytes());
        h.update(b":");
        h.update(scope.turn_seq.to_le_bytes());
        h.update(b":");
        h.update(n.to_le_bytes());
        let hex = bytes_to_hex(h.finalize());
        let short: String = hex.chars().take(32).collect();
        format!("msg_{short}")
    }

    fn begin_turn(&self, seed: &str, turn_seq: i64) {
        let mut scope = self.scope.lock().expect("idgen scope");
        scope.seed = self.pinned_seed.clone().unwrap_or_else(|| seed.to_string());
        scope.turn_seq = turn_seq;
        scope.counter = 0;
    }
}

pub fn default_idgen() -> Arc<dyn IdGen> {
    Arc::new(SessionIdGen::per_session())
}
