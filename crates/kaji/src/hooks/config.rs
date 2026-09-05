//! Les hooks déclarés par l'utilisateur, hors plugins.
//!
//! Deux sources, fusionnées dans cet ordre : la clé `hooks` de la config user
//! puis `.kaji/hooks.yaml` à la racine du projet. Le projet vient après, donc
//! ses règles tournent après celles de l'utilisateur ; aucune ne remplace
//! l'autre — les hooks s'additionnent, comme les règles d'un plugin.
//!
//! Les couches que la clé `hooks` traverse sont exactement celles de
//! [`Config`] : `/etc/kaji/config.yaml`, puis les fichiers de
//! `KAJI_ADDITIONAL_CONFIG_FILES`, puis `~/.config/kaji/config.yaml`. **Il
//! n'existe aucune couche projet** — c'est ce qui empêche un dépôt cloné de se
//! donner le consentement à lui-même. La clé est lue par
//! [`Config::get_param_from_files`] et non `get_param` : ce dernier lirait la
//! variable d'environnement `HOOKS` avant les fichiers, ce qui rouvrirait la
//! porte que le gate ci-dessous ferme. Une `HOOKS=` posée est ignorée, avec un
//! `warn!`.
//!
//! **Les hooks projet sont inactifs par défaut.** Le fichier vit dans le dépôt :
//! un `git clone` suffirait sinon à faire exécuter du shell au premier `kaji`
//! lancé dedans. `KAJI_PROJECT_HOOKS=1` (ou la clé de config du même nom) est
//! le consentement explicite qui les active — les hooks user, eux, sont de la
//! config que l'utilisateur a écrite lui-même, même modèle de confiance que le
//! reste de `config.yaml`.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use tracing::{debug, warn};

use crate::config::Config;

/// Le défaut S6, en secondes. Un hook plus lent que ça est abandonné.
pub const DEFAULT_TIMEOUT_S: u64 = 10;

/// La clé de config (et la variable d'environnement) qui active les hooks
/// déclarés par le dépôt courant.
pub const PROJECT_HOOKS_KEY: &str = "KAJI_PROJECT_HOOKS";

/// La clé de config qui porte les hooks de l'utilisateur.
pub const USER_HOOKS_KEY: &str = "hooks";

/// Le fichier de hooks d'un projet, relatif à sa racine.
pub const PROJECT_HOOKS_FILE: &str = ".kaji/hooks.yaml";

/// Un hook déclaré en config : `{event, command, timeout_s?, matcher?}`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct HookEntry {
    pub event: String,
    pub command: String,
    #[serde(default)]
    pub matcher: Option<String>,
    #[serde(default)]
    pub timeout_s: Option<u64>,
}

impl HookEntry {
    pub fn timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.timeout_s.unwrap_or(DEFAULT_TIMEOUT_S))
    }
}

/// `.kaji/hooks.yaml` accepte la liste nue comme la liste sous `hooks:` — la
/// seconde forme est celle de `config.yaml`, et un utilisateur qui recopie sa
/// config dans le fichier projet ne doit pas tomber sur une erreur de parsing.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum HooksDocument {
    Wrapped { hooks: Vec<HookEntry> },
    Bare(Vec<HookEntry>),
}

impl HooksDocument {
    fn entries(self) -> Vec<HookEntry> {
        match self {
            HooksDocument::Wrapped { hooks } => hooks,
            HooksDocument::Bare(hooks) => hooks,
        }
    }
}

/// Vrai quand l'utilisateur a explicitement consenti aux hooks du dépôt.
pub fn project_hooks_enabled() -> bool {
    Config::global()
        .get_param::<serde_yaml::Value>(PROJECT_HOOKS_KEY)
        .is_ok_and(|value| truthy(&value))
}

fn truthy(value: &serde_yaml::Value) -> bool {
    match value {
        serde_yaml::Value::Bool(flag) => *flag,
        serde_yaml::Value::Number(number) => number.as_i64().is_some_and(|n| n != 0),
        serde_yaml::Value::String(text) => {
            matches!(
                text.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes"
            )
        }
        _ => false,
    }
}

/// Les hooks de l'utilisateur. Une clé absente est le cas courant, pas une
/// erreur ; une clé mal formée est signalée puis ignorée — une config de hooks
/// cassée ne doit pas empêcher kaji de démarrer.
pub fn user_entries() -> Vec<HookEntry> {
    if std::env::var(USER_HOOKS_KEY.to_uppercase()).is_ok() {
        warn!(
            "variable d'environnement `{}` ignorée : les hooks se déclarent en config.yaml ou \
             .kaji/hooks.yaml, jamais par l'environnement",
            USER_HOOKS_KEY.to_uppercase()
        );
    }
    match Config::global().get_param_from_files::<Vec<HookEntry>>(USER_HOOKS_KEY) {
        Ok(entries) => entries,
        Err(crate::config::ConfigError::NotFound(_)) => Vec::new(),
        Err(error) => {
            warn!(%error, "config `hooks` illisible — hooks utilisateur ignorés");
            Vec::new()
        }
    }
}

/// Les hooks du dépôt, quand ils sont activés. `None` en projet, `Some(chemin)`
/// sinon : l'appelant n'a pas à savoir où vit le fichier.
pub fn project_hooks_path(project_root: &Path) -> PathBuf {
    project_root.join(PROJECT_HOOKS_FILE)
}

/// Les hooks déclarés par `project_root`. Vide — et silencieux — quand le
/// consentement n'a pas été donné : c'est le cas par défaut d'un dépôt cloné.
pub fn project_entries(project_root: Option<&Path>) -> Vec<HookEntry> {
    let Some(root) = project_root else {
        return Vec::new();
    };
    let path = project_hooks_path(root);
    if !path.is_file() {
        return Vec::new();
    }
    if !project_hooks_enabled() {
        debug!(
            path = %path.display(),
            "hooks projet présents mais désactivés — poser KAJI_PROJECT_HOOKS=1 pour les activer"
        );
        return Vec::new();
    }
    parse_document(&path)
}

fn parse_document(path: &Path) -> Vec<HookEntry> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) => {
            warn!(path = %path.display(), %error, "hooks projet illisibles");
            return Vec::new();
        }
    };
    match serde_yaml::from_str::<HooksDocument>(&text) {
        Ok(document) => document.entries(),
        Err(error) => {
            warn!(path = %path.display(), %error, "hooks projet mal formés — ignorés");
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, body: &str) -> PathBuf {
        let path = dir.join(PROJECT_HOOKS_FILE);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, body).unwrap();
        path
    }

    const ONE_HOOK: &str = "hooks:\n  - event: user_prompt_submit\n    command: echo hi\n";

    #[test]
    fn a_hook_entry_defaults_its_timeout_to_ten_seconds() {
        let entry: HookEntry =
            serde_yaml::from_str("event: pre_tool_use\ncommand: guard.sh\n").unwrap();
        assert_eq!(entry.timeout().as_secs(), DEFAULT_TIMEOUT_S);
        assert_eq!(entry.matcher, None);

        let explicit: HookEntry =
            serde_yaml::from_str("event: pre_tool_use\ncommand: guard.sh\ntimeout_s: 3\n").unwrap();
        assert_eq!(explicit.timeout().as_secs(), 3);
    }

    #[test]
    fn a_project_file_parses_wrapped_or_bare() {
        let wrapped: HooksDocument = serde_yaml::from_str(ONE_HOOK).unwrap();
        let bare: HooksDocument =
            serde_yaml::from_str("- event: user_prompt_submit\n  command: echo hi\n").unwrap();
        assert_eq!(wrapped.entries(), bare.entries());
    }

    /// La décision de sécurité : un dépôt cloné n'exécute rien tant que
    /// l'utilisateur n'a pas dit oui.
    #[test]
    fn project_hooks_stay_inert_until_the_user_opts_in() {
        let project = tempfile::tempdir().unwrap();
        write(project.path(), ONE_HOOK);

        let off = env_lock::lock_env([(PROJECT_HOOKS_KEY, None::<&str>)]);
        assert!(
            project_entries(Some(project.path())).is_empty(),
            "un .kaji/hooks.yaml ne tourne pas sans consentement"
        );
        drop(off);

        let _on = env_lock::lock_env([(PROJECT_HOOKS_KEY, Some("1"))]);
        assert_eq!(
            project_entries(Some(project.path())).len(),
            1,
            "KAJI_PROJECT_HOOKS=1 les active"
        );
    }

    /// Les hooks se déclarent par fichiers. `get_param` lirait `HOOKS` en
    /// majuscules avant les fichiers : un `.envrc`, un `docker-compose.yml` ou
    /// un `Makefile` du dépôt aurait donc ouvert une seconde porte vers `sh -c`
    /// sans jamais toucher au gate `KAJI_PROJECT_HOOKS`.
    #[test]
    fn an_env_hooks_key_declares_nothing() {
        let env_key = USER_HOOKS_KEY.to_uppercase();
        let _guard = env_lock::lock_env([(
            env_key.as_str(),
            Some(r#"[{"event":"session_start","command":"echo pwned"}]"#),
        )]);
        assert!(user_entries().is_empty());
    }

    #[test]
    fn a_malformed_project_file_is_ignored_not_fatal() {
        let project = tempfile::tempdir().unwrap();
        write(project.path(), "hooks: [ this is not a hook ]");
        let _on = env_lock::lock_env([(PROJECT_HOOKS_KEY, Some("1"))]);
        assert!(project_entries(Some(project.path())).is_empty());
    }
}
