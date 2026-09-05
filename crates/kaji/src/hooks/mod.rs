//! Lifecycle hooks support, modelled after the Open Plugins
//! [hooks specification](https://open-plugins.com/agent-builders/components/hooks).
//!
//! Hooks live in `<plugin-root>/hooks/hooks.json` of any plugin discovered by
//! [`crate::plugins::discovery::discover_enabled_plugins`]. The schema is:
//!
//! ```json
//! {
//!   "hooks": {
//!     "PostToolUse": [
//!       {
//!         "matcher": "developer__shell|developer__text_editor",
//!         "hooks": [
//!           { "type": "command", "command": "${PLUGIN_ROOT}/scripts/log.sh" }
//!         ]
//!       }
//!     ]
//!   }
//! }
//! ```
//!
//! Kaji currently supports `type: "command"` actions. Unknown event names and
//! action types are ignored per the spec. Hook scripts receive the JSON event
//! context on stdin and SHOULD exit 0 on success.
//!
//! Les hooks déclarés par l'utilisateur — clé `hooks` de `config.yaml` et
//! `.kaji/hooks.yaml` du projet — arrivent par [`config`] et rejoignent les
//! mêmes index : un hook de config et un hook de plugin sont exécutés par le
//! même chemin, avec les mêmes règles de blocage et de timeout.
//!
//! Ce que le modèle finit par lire d'un hook — la sortie standard injectée dans
//! le prompt, la décision d'un `pre_tool_use` ou d'un `stop` — est de l'état
//! externe : c'est journalisé sous le kind `hook_output`. Un [`HookManager`]
//! monté avec [`HookManager::with_replay`] ne lance jamais de processus (spec
//! S6, `docs/superpowers/specs/2026-09-05-p3-vision-web-workflows-mission-control-design.md`).
//!
//! **Ce qui est servi au rejeu, et ce qui ne l'est pas.** Les décisions le
//! sont : un `pre_tool_use` ou un `stop` qui a bloqué à l'enregistrement bloque
//! au rejeu, sur une machine qui n'a pas le hook. Les sorties injectées dans le
//! prompt, elles, ne le sont **pas** : le message du journal les porte déjà
//! (`Agent::apply_prompt_hooks` réécrit avant la persistance), donc les
//! réappliquer les dupliquerait. Elles restent journalisées pour
//! l'observabilité — savoir ce qu'un hook avait produit ce jour-là — et le
//! rejeu se contente de les lire. Corollaire : une ligne `hook_output` de
//! sortie n'appartient pas nécessairement à un tour rejouable (un tour qui
//! n'émet aucun `AgentEvent::Message` en laisse une derrière lui) — elle est
//! inerte, jamais réinjectée, et purgée avec le reste du journal.

pub mod config;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Context, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tracing::{debug, info, warn};
use tracing_futures::Instrument;

use crate::plugins::discovery::{discover_enabled_plugins, DiscoveredPlugin};
use crate::replay::record::{record_hook_output, TurnRecorder};
use crate::replay::source::ReplaySource;

/// Default per-hook timeout when the plugin does not specify one.
const DEFAULT_HOOK_TIMEOUT_SECS: u64 = 30;

/// La raison rendue au modèle quand un `pre_tool_use` n'a pas répondu à temps.
/// Le seul événement en fail-closed : laisser passer un appel qu'un garde-fou
/// n'a pas pu examiner reviendrait à ne pas avoir de garde-fou.
pub const TIMEOUT_DENIAL: &str = "hook timeout";

/// Lifecycle events a hook can subscribe to.
///
/// The variant names match the event names used in `hooks.json`. Unknown
/// events in user config are ignored at load time, per the spec.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HookEvent {
    PreToolUse,
    PostToolUse,
    PostToolUseFailure,
    SessionStart,
    SessionEnd,
    UserPromptSubmit,
    BeforeReadFile,
    AfterFileEdit,
    BeforeShellExecution,
    AfterShellExecution,
    TurnEnd,
    Stop,
}

impl HookEvent {
    fn name(&self) -> &'static str {
        match self {
            HookEvent::PreToolUse => "PreToolUse",
            HookEvent::PostToolUse => "PostToolUse",
            HookEvent::PostToolUseFailure => "PostToolUseFailure",
            HookEvent::SessionStart => "SessionStart",
            HookEvent::SessionEnd => "SessionEnd",
            HookEvent::UserPromptSubmit => "UserPromptSubmit",
            HookEvent::BeforeReadFile => "BeforeReadFile",
            HookEvent::AfterFileEdit => "AfterFileEdit",
            HookEvent::BeforeShellExecution => "BeforeShellExecution",
            HookEvent::AfterShellExecution => "AfterShellExecution",
            HookEvent::TurnEnd => "TurnEnd",
            HookEvent::Stop => "Stop",
        }
    }

    /// Le nom que la config user écrit — celui de la spec S6, en snake_case.
    /// C'est aussi la clé d'adressage du kind `hook_output` : le journal ne doit
    /// pas dépendre de la casse qu'un fichier de config a employée.
    pub fn config_name(&self) -> &'static str {
        match self {
            HookEvent::PreToolUse => "pre_tool_use",
            HookEvent::PostToolUse => "post_tool_use",
            HookEvent::PostToolUseFailure => "post_tool_use_failure",
            HookEvent::SessionStart => "session_start",
            HookEvent::SessionEnd => "session_end",
            HookEvent::UserPromptSubmit => "user_prompt_submit",
            HookEvent::BeforeReadFile => "before_read_file",
            HookEvent::AfterFileEdit => "after_file_edit",
            HookEvent::BeforeShellExecution => "before_shell_execution",
            HookEvent::AfterShellExecution => "after_shell_execution",
            HookEvent::TurnEnd => "turn_end",
            HookEvent::Stop => "stop",
        }
    }

    /// Reconnaît les deux orthographes : `PostToolUse` des plugins Open-Plugins
    /// et `post_tool_use` de la config user. Un nom inconnu reste ignoré, comme
    /// le veut la spec plugins.
    fn from_name(name: &str) -> Option<Self> {
        const EVENTS: [HookEvent; 12] = [
            HookEvent::PreToolUse,
            HookEvent::PostToolUse,
            HookEvent::PostToolUseFailure,
            HookEvent::SessionStart,
            HookEvent::SessionEnd,
            HookEvent::UserPromptSubmit,
            HookEvent::BeforeReadFile,
            HookEvent::AfterFileEdit,
            HookEvent::BeforeShellExecution,
            HookEvent::AfterShellExecution,
            HookEvent::TurnEnd,
            HookEvent::Stop,
        ];
        EVENTS
            .into_iter()
            .find(|event| event.name() == name || event.config_name() == name)
    }

    /// Vrai pour les événements dont la sortie standard entre dans le prompt —
    /// et qui sont donc journalisés puis servis au rejeu.
    fn injects_output(&self) -> bool {
        matches!(
            self,
            HookEvent::SessionStart | HookEvent::UserPromptSubmit | HookEvent::PostToolUse
        )
    }
}

impl std::fmt::Display for HookEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// Top-level `hooks.json` shape.
#[derive(Debug, Default, Deserialize)]
struct HooksFile {
    #[serde(default)]
    hooks: HashMap<String, Vec<RawHookRule>>,
}

/// One rule within a `hooks.json` event entry.
#[derive(Debug, Deserialize)]
struct RawHookRule {
    #[serde(default)]
    matcher: Option<String>,
    #[serde(default)]
    hooks: Vec<RawHookAction>,
}

/// One action entry under a rule's `hooks` array. We only run `command`
/// today, but we deserialize the others so that loading a plugin which uses
/// them does not fail.
#[derive(Debug, Deserialize)]
struct RawHookAction {
    #[serde(default, rename = "type")]
    action_type: Option<String>,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    timeout: Option<u64>,
}

/// D'où vient une règle — et donc sous quel contrat de blocage elle tourne.
///
/// La sémantique S6 (`pre_tool_use` : tout exit non nul bloque, un timeout
/// aussi) appartient aux hooks que l'utilisateur a écrits dans sa config. Un
/// hook de plugin a été écrit contre le contrat Open-Plugins — exit 2 ou
/// `{"decision":"block"}` — et le lui changer rétroactivement transformerait
/// son erreur interne (dépendance absente, fichier manquant) en blocage
/// d'outil.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RuleSource {
    Plugin,
    Config,
    ProjectConfig,
}

impl RuleSource {
    /// Vrai pour les règles qui suivent le contrat de blocage S6.
    fn blocks_on_any_failure(&self) -> bool {
        matches!(self, RuleSource::Config | RuleSource::ProjectConfig)
    }
}

/// A loaded, plugin-bound hook rule ready to execute.
#[derive(Debug, Clone)]
struct LoadedRule {
    plugin_name: String,
    plugin_root: PathBuf,
    matcher: Option<Regex>,
    actions: Vec<LoadedAction>,
    source: RuleSource,
}

#[derive(Debug, Clone)]
enum LoadedAction {
    Command { command: String, timeout: Duration },
}

/// Context passed to a hook as JSON on stdin.
///
/// The `matcher_context` is the string the rule's `matcher` regex is tested
/// against — tool name for tool events, file path for file events, command
/// string for shell events. Other fields carry the same value plus the
/// raw JSON payload of the underlying event so scripts can do richer things
/// without needing to parse a hook-specific schema.
///
/// Le payload est **borné à ces champs** : rien de la conversation, de la
/// config, des secrets ou de l'environnement de kaji n'y entre. Un hook qui a
/// besoin de plus le lit lui-même, sous l'identité de l'utilisateur.
/// `prompt` et `tool_args` sont les noms de la spec S6, `message` et
/// `tool_input` ceux des plugins Open-Plugins : les deux paires portent la même
/// valeur pour qu'un script écrit contre l'une ou l'autre fonctionne.
#[derive(Debug, Clone, Serialize)]
pub struct HookContext {
    pub event: String,
    pub session_id: String,
    pub matcher_context: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_input: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_args: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_output: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_assistant_message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<String>,
}

impl HookContext {
    pub fn new(event: HookEvent, session_id: impl Into<String>) -> Self {
        Self {
            event: event.to_string(),
            session_id: session_id.into(),
            matcher_context: None,
            tool_name: None,
            tool_input: None,
            tool_args: None,
            tool_output: None,
            message: None,
            prompt: None,
            last_assistant_message: None,
            working_dir: None,
        }
    }

    pub fn with_tool(mut self, tool_name: impl Into<String>, tool_input: Option<Value>) -> Self {
        let name = tool_name.into();
        self.matcher_context = Some(name.clone());
        self.tool_name = Some(name);
        self.tool_args = tool_input.clone();
        self.tool_input = tool_input;
        self
    }

    pub fn with_tool_output(mut self, output: Value) -> Self {
        self.tool_output = Some(output);
        self
    }

    pub fn with_message(mut self, message: impl Into<String>) -> Self {
        let msg = message.into();
        self.matcher_context.get_or_insert_with(|| msg.clone());
        self.prompt = Some(msg.clone());
        self.message = Some(msg);
        self
    }

    pub fn with_last_assistant_message(mut self, message: impl Into<String>) -> Self {
        let message = message.into();
        if !message.is_empty() {
            self.last_assistant_message = Some(message);
        }
        self
    }

    pub fn with_working_dir(mut self, dir: impl Into<String>) -> Self {
        self.working_dir = Some(dir.into());
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookDecision {
    Allow,
    Deny { reason: String, plugin: String },
}

/// Loads and executes plugin hooks.
#[derive(Default, Clone)]
pub struct HookManager {
    rules: HashMap<HookEvent, Vec<LoadedRule>>,
    use_login_shell_path: bool,
    /// Le journal du tour rejoué. Sa seule présence suffit à interdire tout
    /// `spawn` : un rejeu relit ce que les hooks ont produit, il ne les
    /// rejoue pas — un `rtk-rewrite` d'hier ne doit pas réécrire autrement
    /// aujourd'hui, et un `shosoin-context` absent de la machine de rejeu ne
    /// doit rien changer au prompt.
    replay: Option<ReplaySource>,
}

impl HookManager {
    /// Build a manager by scanning all enabled plugins for `hooks/hooks.json`,
    /// then the user's own `hooks:` config and the project's
    /// `.kaji/hooks.yaml` (voir [`config`] pour le gate d'activation projet).
    pub fn load(project_root: Option<&Path>, use_login_shell_path: bool) -> Self {
        let plugins = discover_enabled_plugins(project_root);
        let mut manager = Self::from_plugins(plugins, use_login_shell_path);
        if config_hooks_enabled() {
            manager.add_config_rules(project_root);
        }
        manager
    }

    #[cfg(test)]
    pub(crate) fn from_plugins_for_test(plugins: Vec<DiscoveredPlugin>) -> Self {
        Self::from_plugins(plugins, false)
    }

    /// Un manager monté sur les seuls hooks de config — l'entrée de test des
    /// suites d'acceptation S6, qui n'ont pas de plugin à découvrir.
    pub fn from_entries(entries: Vec<config::HookEntry>, root: &Path, source: &str) -> Self {
        let mut manager = Self::default();
        manager.extend_from_entries(entries, root, source, RuleSource::Config);
        manager
    }

    /// Sert le journal au lieu du shell. Point unique : tous les événements
    /// passent par `emit`/`emit_blocking`/`emit_capturing`, qui court-circuitent
    /// tous les trois quand il est posé.
    pub fn with_replay(mut self, replay: ReplaySource) -> Self {
        self.replay = Some(replay);
        self
    }

    fn add_config_rules(&mut self, project_root: Option<&Path>) {
        let user_root = crate::config::paths::Paths::config_dir();
        self.extend_from_entries(
            config::user_entries(),
            &user_root,
            "user",
            RuleSource::Config,
        );
        if let Some(root) = project_root {
            self.extend_from_entries(
                config::project_entries(project_root),
                root,
                "project",
                RuleSource::ProjectConfig,
            );
        }
    }

    fn extend_from_entries(
        &mut self,
        entries: Vec<config::HookEntry>,
        root: &Path,
        source: &str,
        rule_source: RuleSource,
    ) {
        for entry in entries {
            let Some(event) = HookEvent::from_name(&entry.event) else {
                warn!(event = %entry.event, source, "hook ignoré : événement inconnu");
                continue;
            };
            let matcher = match entry.matcher.as_deref().filter(|s| !s.is_empty()) {
                Some(pattern) => match Regex::new(pattern) {
                    Ok(regex) => Some(regex),
                    Err(error) => {
                        warn!(pattern, source, %error, "hook ignoré : matcher invalide");
                        continue;
                    }
                },
                None => None,
            };
            self.rules.entry(event).or_default().push(LoadedRule {
                plugin_name: source.to_string(),
                plugin_root: root.to_path_buf(),
                matcher,
                actions: vec![LoadedAction::Command {
                    command: entry.command.clone(),
                    timeout: entry.timeout(),
                }],
                source: rule_source,
            });
        }
    }

    fn from_plugins(plugins: Vec<DiscoveredPlugin>, use_login_shell_path: bool) -> Self {
        let mut rules: HashMap<HookEvent, Vec<LoadedRule>> = HashMap::new();
        let mut total = 0usize;

        for plugin in plugins {
            let hooks_path = plugin.root.join("hooks").join("hooks.json");
            if !hooks_path.is_file() {
                continue;
            }
            match load_hooks_file(&hooks_path, &plugin.name, &plugin.root) {
                Ok(loaded) => {
                    for (event, plugin_rules) in loaded {
                        total += plugin_rules.len();
                        rules.entry(event).or_default().extend(plugin_rules);
                    }
                }
                Err(err) => warn!(
                    plugin = %plugin.name,
                    path = %hooks_path.display(),
                    error = %err,
                    "Failed to load plugin hooks; skipping",
                ),
            }
        }

        if total > 0 {
            info!(
                rule_count = total,
                events = ?rules.keys().map(|e| e.name()).collect::<Vec<_>>(),
                "Loaded plugin hooks",
            );
        }

        Self {
            rules,
            use_login_shell_path,
            replay: None,
        }
    }

    /// Returns true if any rule is registered for `event`.
    pub fn has_hooks(&self, event: HookEvent) -> bool {
        self.rules.get(&event).is_some_and(|r| !r.is_empty())
    }

    /// Les règles à faire tourner pour cet événement, ou `None` quand il n'y a
    /// rien à faire — pas de règle, ou un rejeu en cours.
    fn runnable(&self, event: HookEvent) -> Option<&Vec<LoadedRule>> {
        if self.replay.is_some() {
            return None;
        }
        self.rules.get(&event).filter(|rules| !rules.is_empty())
    }

    fn matches(rule: &LoadedRule, ctx: &HookContext) -> bool {
        match &rule.matcher {
            Some(matcher) => matcher.is_match(ctx.matcher_context.as_deref().unwrap_or("")),
            None => true,
        }
    }

    async fn run_action(
        &self,
        event: HookEvent,
        session_id: &str,
        rule: &LoadedRule,
        command: &str,
        payload: &str,
        timeout: Duration,
    ) -> Result<std::process::Output> {
        announce_project_hook(rule);
        let span = tracing::info_span!(
            target: "kaji::hooks",
            "execute_hook",
            "gen_ai.operation.name" = "execute_hook",
            "kaji.hook.event" = %event,
            "kaji.hook.plugin" = %rule.plugin_name,
            "error.type" = tracing::field::Empty,
            session.id = %session_id,
        );
        let result = run_command_hook(
            command,
            &rule.plugin_root,
            payload,
            timeout,
            self.use_login_shell_path,
        )
        .instrument(span.clone())
        .await;
        match &result {
            Ok(output) if !output.status.success() => {
                span.record("error.type", "hook_exit");
            }
            Err(_) => {
                span.record("error.type", "hook_execution_error");
            }
            _ => {}
        }
        result
    }

    /// Fire all rules whose matcher matches the event context. Errors from
    /// individual hooks are logged but never propagated — a misbehaving hook
    /// MUST NOT crash the host tool.
    pub async fn emit(&self, event: HookEvent, ctx: HookContext) {
        let Some(rules) = self.runnable(event) else {
            return;
        };

        let payload = match serde_json::to_string(&ctx) {
            Ok(s) => s,
            Err(err) => {
                warn!(event = %event, error = %err, "Failed to serialize hook context");
                return;
            }
        };

        for rule in rules {
            if let Some(matcher) = &rule.matcher {
                let target = ctx.matcher_context.as_deref().unwrap_or("");
                if !matcher.is_match(target) {
                    continue;
                }
            }

            for action in &rule.actions {
                let LoadedAction::Command { command, timeout } = action;
                debug!(
                    plugin = %rule.plugin_name,
                    event = %event,
                    command = %command,
                    "Running plugin hook",
                );
                let res = self
                    .run_action(event, &ctx.session_id, rule, command, &payload, *timeout)
                    .await
                    .and_then(|o| {
                        if o.status.success() {
                            Ok(())
                        } else {
                            anyhow::bail!(
                                "hook `{command}` exited with {:?}: {}",
                                o.status.code(),
                                String::from_utf8_lossy(&o.stderr).trim()
                            )
                        }
                    });
                if let Err(err) = res {
                    warn!(
                        plugin = %rule.plugin_name,
                        event = %event,
                        command = %command,
                        error = %err,
                        "Plugin hook failed",
                    );
                }
            }
        }
    }

    /// Like [`Self::emit`], but stops at the first rule that denies the event
    /// and returns the denial.
    ///
    /// Sur `pre_tool_use`, et pour les seules règles issues de la config, la
    /// règle S6 s'applique : **toute** sortie non nulle bloque l'appel, stderr
    /// en devient la raison rendue au modèle, et un timeout bloque aussi
    /// ([`TIMEOUT_DENIAL`]) — fail-closed, parce qu'un garde-fou qui n'a pas pu
    /// répondre n'est pas un garde-fou qui a dit oui. Partout ailleurs — les
    /// autres événements (`Stop`), et **tous** les hooks de plugins — le
    /// contrat Open-Plugins tient : seuls l'exit 2 avec la raison sur stderr et
    /// `{"decision":"block","reason":"..."}` sur stdout bloquent, tout le reste
    /// est laissé passer (voir [`RuleSource`]).
    ///
    /// La décision est journalisée sous `hook_output` et **servie au rejeu** :
    /// `addr` en est la clé (l'id d'appel d'outil, vide pour un événement de
    /// tour). Un rejeu ne relance donc aucun hook, et rejoue le même blocage
    /// sur une machine où le hook n'existe pas.
    pub async fn emit_blocking(
        &self,
        event: HookEvent,
        ctx: HookContext,
        recorder: Option<&Arc<TurnRecorder>>,
        addr: &str,
    ) -> HookDecision {
        if let Some(replay) = &self.replay {
            return match replay.hook_denial(event.config_name(), addr) {
                Some((reason, plugin)) => HookDecision::Deny { reason, plugin },
                None => HookDecision::Allow,
            };
        }

        let Some(rules) = self.runnable(event) else {
            return HookDecision::Allow;
        };

        let payload = match serde_json::to_string(&ctx) {
            Ok(s) => s,
            Err(err) => {
                warn!(event = %event, error = %err, "Failed to serialize hook context");
                return HookDecision::Allow;
            }
        };

        for rule in rules {
            if !Self::matches(rule, &ctx) {
                continue;
            }

            // Le contrat de blocage se lit par règle, pas par événement : un
            // `pre_tool_use` de plugin garde le contrat sous lequel il a été
            // écrit, seuls les hooks de config suivent la sémantique S6.
            let fail_closed = event == HookEvent::PreToolUse && rule.source.blocks_on_any_failure();

            for action in &rule.actions {
                let LoadedAction::Command { command, timeout } = action;
                let denial = match self
                    .run_action(event, &ctx.session_id, rule, command, &payload, *timeout)
                    .await
                {
                    Ok(output) => deny_reason(&output, fail_closed),
                    Err(err) => {
                        warn!(
                            plugin = %rule.plugin_name,
                            event = %event,
                            command = %command,
                            error = %err,
                            "Plugin hook failed",
                        );
                        fail_closed.then(|| TIMEOUT_DENIAL.to_string())
                    }
                };

                if let Some(reason) = denial {
                    info!(
                        plugin = %rule.plugin_name,
                        event = %event,
                        command = %command,
                        reason = %reason,
                        "Plugin hook denied tool call",
                    );
                    record_hook_output(
                        recorder,
                        event.config_name(),
                        addr,
                        None,
                        Some((&reason, &rule.plugin_name)),
                    )
                    .await;
                    return HookDecision::Deny {
                        reason,
                        plugin: rule.plugin_name.clone(),
                    };
                }
            }
        }

        HookDecision::Allow
    }

    /// Lance les hooks de `event` et rend ce que le modèle doit lire : la sortie
    /// standard des exécutions réussies, dans l'ordre des règles, jointe par une
    /// ligne vide. `None` quand rien n'a été produit.
    ///
    /// Un hook qui échoue ou dépasse son délai est ignoré, avec un warning —
    /// fail-open : injecter du contexte est un service, pas un garde-fou, et le
    /// tour doit partir même quand `shosoin-context` est cassé.
    ///
    /// La sortie entre dans le prompt : elle est journalisée sous `hook_output`
    /// pour l'observabilité. Au rejeu, c'est le message enregistré qui la porte
    /// — cette fonction ne relance jamais la commande, et son appelant ne
    /// réapplique pas ce qu'elle rend (voir la doc du module).
    pub async fn emit_capturing(
        &self,
        event: HookEvent,
        ctx: HookContext,
        recorder: Option<&Arc<TurnRecorder>>,
        addr: &str,
    ) -> Option<String> {
        debug_assert!(
            event.injects_output(),
            "seuls les événements dont la sortie entre dans le prompt sont capturés"
        );
        if let Some(replay) = &self.replay {
            return replay.hook_output(event.config_name(), addr);
        }

        let rules = self.runnable(event)?;
        let payload = match serde_json::to_string(&ctx) {
            Ok(payload) => payload,
            Err(error) => {
                warn!(event = %event, %error, "Failed to serialize hook context");
                return None;
            }
        };

        let mut captured: Vec<String> = Vec::new();
        for rule in rules {
            if !Self::matches(rule, &ctx) {
                continue;
            }
            for action in &rule.actions {
                let LoadedAction::Command { command, timeout } = action;
                match self
                    .run_action(event, &ctx.session_id, rule, command, &payload, *timeout)
                    .await
                {
                    Ok(output) if output.status.success() => {
                        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
                        if !stdout.is_empty() {
                            captured.push(stdout);
                        }
                    }
                    Ok(output) => warn!(
                        plugin = %rule.plugin_name,
                        event = %event,
                        command = %command,
                        code = ?output.status.code(),
                        stderr = %String::from_utf8_lossy(&output.stderr).trim(),
                        "hook en échec — sortie ignorée",
                    ),
                    Err(error) => warn!(
                        plugin = %rule.plugin_name,
                        event = %event,
                        command = %command,
                        %error,
                        "hook en échec — sortie ignorée",
                    ),
                }
            }
        }

        if captured.is_empty() {
            return None;
        }
        let output = captured.join("\n\n");
        record_hook_output(recorder, event.config_name(), addr, Some(&output), None).await;
        Some(output)
    }
}

/// `any_non_zero` porte la sémantique S6 de `pre_tool_use` : là, un exit
/// quelconque non nul bloque. Ailleurs seul l'exit 2 du contrat plugins compte.
/// Ajoute le retour d'un `post_tool_use` au résultat d'outil que le modèle va
/// lire. Un bloc de texte de plus, jamais une réécriture : le résultat réel de
/// l'outil reste intact devant.
///
/// Les deux boucles appellent cette fonction — la sémantique n'est écrite
/// qu'ici, seul le branchement est en double.
pub fn append_tool_feedback(
    result: std::result::Result<rmcp::model::CallToolResult, rmcp::ErrorData>,
    feedback: &str,
) -> std::result::Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
    result.map(|mut call_result| {
        call_result
            .content
            .push(rmcp::model::ContentBlock::text(feedback));
        call_result
    })
}

/// Le consentement `KAJI_PROJECT_HOOKS` se donne une fois et s'oublie ensuite.
/// La première exécution d'un hook du dépôt le rappelle, pour que « ce dépôt
/// exécute son propre shell chez moi » reste un fait visible six mois plus tard.
fn announce_project_hook(rule: &LoadedRule) {
    static ANNOUNCED: std::sync::Once = std::sync::Once::new();
    if rule.source != RuleSource::ProjectConfig {
        return;
    }
    ANNOUNCED.call_once(|| {
        info!(
            path = %rule.plugin_root.join(config::PROJECT_HOOKS_FILE).display(),
            "hooks du dépôt actifs : premier hook projet exécuté de cette session",
        );
    });
}

/// Les hooks déclarés en config ne sont montés que dans le binaire kaji.
///
/// [`HookManager::load`] lit `Config::global()`, c'est-à-dire le
/// `config.yaml` réel de la machine : sans cette porte, une suite de tests qui
/// construit un `Agent` exécuterait les hooks de la personne qui la lance, et
/// un `post_tool_use` configuré ferait tomber des assertions sans rapport.
/// Les plugins découverts, eux, étaient déjà là avant S6.
static CONFIG_HOOKS_ENABLED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Autorise [`HookManager::load`] à monter les hooks de config. Appelé une fois
/// par le binaire kaji, au démarrage — jamais par une bibliothèque ni par un
/// test.
pub fn enable_config_hooks() {
    CONFIG_HOOKS_ENABLED.store(true, std::sync::atomic::Ordering::Relaxed);
}

fn config_hooks_enabled() -> bool {
    CONFIG_HOOKS_ENABLED.load(std::sync::atomic::Ordering::Relaxed)
}

fn deny_reason(output: &std::process::Output, any_non_zero: bool) -> Option<String> {
    const DEFAULT: &str = "denied by plugin hook";
    let non_empty = |s: String| if s.is_empty() { DEFAULT.into() } else { s };

    let blocking_exit = match output.status.code() {
        Some(2) => true,
        Some(code) => any_non_zero && code != 0,
        None => any_non_zero,
    };
    if blocking_exit {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Some(non_empty(stderr));
    }

    #[derive(Deserialize)]
    struct Resp {
        decision: Option<String>,
        reason: Option<String>,
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let trimmed = stdout.trim();
    if !trimmed.starts_with('{') {
        return None;
    }
    let parsed: Resp = serde_json::from_str(trimmed).ok()?;
    (parsed.decision.as_deref() == Some("block"))
        .then(|| non_empty(parsed.reason.unwrap_or_default()))
}

fn load_hooks_file(
    path: &Path,
    plugin_name: &str,
    plugin_root: &Path,
) -> Result<HashMap<HookEvent, Vec<LoadedRule>>> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let parsed: HooksFile =
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;

    let mut out: HashMap<HookEvent, Vec<LoadedRule>> = HashMap::new();
    for (event_name, raw_rules) in parsed.hooks {
        let Some(event) = HookEvent::from_name(&event_name) else {
            debug!(plugin = plugin_name, event = %event_name, "Ignoring unknown hook event");
            continue;
        };

        for raw in raw_rules {
            let matcher = match raw.matcher.as_deref().filter(|s| !s.is_empty()) {
                Some(pattern) => match Regex::new(pattern) {
                    Ok(re) => Some(re),
                    Err(err) => {
                        warn!(
                            plugin = plugin_name,
                            pattern,
                            error = %err,
                            "Invalid hook matcher regex; skipping rule",
                        );
                        continue;
                    }
                },
                None => None,
            };

            let mut actions = Vec::new();
            for raw_action in raw.hooks {
                match raw_action.action_type.as_deref().unwrap_or("command") {
                    "command" => {
                        if let Some(cmd) = raw_action.command {
                            let timeout = Duration::from_secs(
                                raw_action.timeout.unwrap_or(DEFAULT_HOOK_TIMEOUT_SECS),
                            );
                            actions.push(LoadedAction::Command {
                                command: cmd,
                                timeout,
                            });
                        }
                    }
                    other => {
                        debug!(
                            plugin = plugin_name,
                            action_type = other,
                            "Ignoring unsupported hook action type",
                        );
                    }
                }
            }

            if actions.is_empty() {
                continue;
            }

            out.entry(event).or_default().push(LoadedRule {
                plugin_name: plugin_name.to_string(),
                plugin_root: plugin_root.to_path_buf(),
                matcher,
                actions,
                source: RuleSource::Plugin,
            });
        }
    }

    Ok(out)
}

async fn run_command_hook(
    raw_command: &str,
    plugin_root: &Path,
    payload: &str,
    timeout: Duration,
    use_login_shell_path: bool,
) -> Result<std::process::Output> {
    match tokio::time::timeout(
        timeout,
        run_command_hook_inner(raw_command, plugin_root, payload, use_login_shell_path),
    )
    .await
    {
        Ok(res) => res,
        Err(_) => anyhow::bail!("hook `{raw_command}` timed out after {:?}", timeout),
    }
}

async fn run_command_hook_inner(
    raw_command: &str,
    plugin_root: &Path,
    payload: &str,
    use_login_shell_path: bool,
) -> Result<std::process::Output> {
    let command = expand_plugin_root(raw_command, plugin_root);
    let path = if use_login_shell_path {
        hook_path().await
    } else {
        None
    };
    let mut process = hook_command(&command, plugin_root, path.as_deref());
    process
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = process
        .spawn()
        .with_context(|| format!("spawning hook `{command}`"))?;

    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(payload.as_bytes()).await;
        let _ = stdin.shutdown().await;
    }

    child
        .wait_with_output()
        .await
        .with_context(|| format!("waiting on hook `{command}`"))
}

fn hook_command(command: &str, plugin_root: &Path, path: Option<&str>) -> Command {
    #[cfg(not(windows))]
    {
        if crate::agents::platform_extensions::developer::shell::is_flatpak() {
            let mut process =
                crate::agents::platform_extensions::developer::shell::flatpak_spawn_command();
            process.arg(format!("--env=PLUGIN_ROOT={}", plugin_root.display()));
            if let Some(path) = path {
                process.arg(format!("--env=PATH={path}"));
            }
            process.arg("sh").arg("-c").arg(command);
            return process;
        }
    }

    let mut process = Command::new("sh");
    process
        .arg("-c")
        .arg(command)
        .env("PLUGIN_ROOT", plugin_root);
    if let Some(path) = path {
        process.env("PATH", path);
    }
    process
}

async fn hook_path() -> Option<String> {
    static HOOK_PATH: OnceLock<tokio::sync::watch::Receiver<Option<String>>> = OnceLock::new();
    let mut rx = HOOK_PATH
        .get_or_init(|| {
            let (tx, rx) = tokio::sync::watch::channel(None);
            tokio::spawn(async move {
                let path = resolve_hook_path().await;
                let _ = tx.send(path);
            });
            rx
        })
        .clone();

    if rx.borrow().is_some() {
        return rx.borrow().clone();
    }
    if rx.changed().await.is_ok() {
        rx.borrow().clone()
    } else {
        None
    }
}

async fn resolve_hook_path() -> Option<String> {
    #[cfg(not(windows))]
    {
        tokio::task::spawn_blocking(|| {
            crate::agents::platform_extensions::developer::shell::resolve_login_shell_path()
                .map(|login| merge_paths(&login, &std::env::var("PATH").unwrap_or_default()))
        })
        .await
        .ok()
        .flatten()
    }
    #[cfg(windows)]
    {
        None
    }
}

fn merge_paths(first: &str, second: &str) -> String {
    let mut seen = std::collections::HashSet::new();
    let mut merged = Vec::new();
    for entry in first.split(':').chain(second.split(':')) {
        if !entry.is_empty() && seen.insert(entry) {
            merged.push(entry);
        }
    }
    merged.join(":")
}

fn expand_plugin_root(command: &str, plugin_root: &Path) -> String {
    command.replace("${PLUGIN_ROOT}", &plugin_root.to_string_lossy())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::discovery::{DiscoveredPlugin, PluginScope};

    fn write_plugin(root: &Path, name: &str, hooks_json: &str) -> PathBuf {
        let plugin = root.join(name);
        std::fs::create_dir_all(plugin.join("hooks")).unwrap();
        std::fs::write(plugin.join("hooks").join("hooks.json"), hooks_json).unwrap();
        plugin
    }

    fn make_manager(plugins: Vec<DiscoveredPlugin>) -> HookManager {
        HookManager::from_plugins(plugins, false)
    }

    #[test]
    fn ignores_unknown_events() {
        let tmp = tempfile::tempdir().unwrap();
        let root = write_plugin(
            tmp.path(),
            "p",
            r#"{"hooks":{"NotARealEvent":[{"hooks":[{"type":"command","command":"echo"}]}]}}"#,
        );
        let mgr = make_manager(vec![DiscoveredPlugin {
            name: "p".into(),
            root,
            scope: PluginScope::User,
        }]);
        assert!(!mgr.has_hooks(HookEvent::PreToolUse));
    }

    #[test]
    fn loads_matcher_and_command() {
        let tmp = tempfile::tempdir().unwrap();
        let root = write_plugin(
            tmp.path(),
            "p",
            r#"{"hooks":{"PostToolUse":[{"matcher":"developer__.*","hooks":[{"type":"command","command":"echo hi"}]}]}}"#,
        );
        let mgr = make_manager(vec![DiscoveredPlugin {
            name: "p".into(),
            root,
            scope: PluginScope::User,
        }]);
        assert!(mgr.has_hooks(HookEvent::PostToolUse));
    }

    #[test]
    fn invalid_matcher_skipped_without_panic() {
        let tmp = tempfile::tempdir().unwrap();
        let root = write_plugin(
            tmp.path(),
            "p",
            r#"{"hooks":{"PostToolUse":[{"matcher":"[invalid","hooks":[{"type":"command","command":"echo"}]}]}}"#,
        );
        let mgr = make_manager(vec![DiscoveredPlugin {
            name: "p".into(),
            root,
            scope: PluginScope::User,
        }]);
        assert!(!mgr.has_hooks(HookEvent::PostToolUse));
    }

    #[tokio::test]
    async fn emit_runs_command_with_plugin_root_substitution() {
        let tmp = tempfile::tempdir().unwrap();
        let marker = tmp.path().join("ran.txt");
        let marker_path = marker.to_string_lossy().into_owned();
        let hooks = format!(
            r#"{{"hooks":{{"SessionStart":[{{"hooks":[{{"type":"command","command":"sh -c 'echo $PLUGIN_ROOT > {marker}'"}}]}}]}}}}"#,
            marker = marker_path,
        );
        let root = write_plugin(tmp.path(), "p", &hooks);
        let mgr = make_manager(vec![DiscoveredPlugin {
            name: "p".into(),
            root: root.clone(),
            scope: PluginScope::User,
        }]);

        mgr.emit(
            HookEvent::SessionStart,
            HookContext::new(HookEvent::SessionStart, "session-1"),
        )
        .await;

        let written = std::fs::read_to_string(&marker).unwrap();
        assert_eq!(written.trim(), root.to_string_lossy());
    }

    #[tokio::test]
    async fn stop_hook_emit_blocking_returns_denial() {
        let tmp = tempfile::tempdir().unwrap();
        let root = write_plugin(
            tmp.path(),
            "p",
            r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"printf '%s' '{\"decision\":\"block\",\"reason\":\"say something first\"}'"}]}]}}"#,
        );
        let mgr = make_manager(vec![DiscoveredPlugin {
            name: "p".into(),
            root,
            scope: PluginScope::User,
        }]);

        let decision = mgr
            .emit_blocking(
                HookEvent::Stop,
                HookContext::new(HookEvent::Stop, "s"),
                None,
                "",
            )
            .await;

        assert_eq!(
            decision,
            HookDecision::Deny {
                reason: "say something first".into(),
                plugin: "p".into(),
            }
        );
    }

    /// La sémantique S6 « tout exit non nul bloque » appartient aux hooks que
    /// l'utilisateur a déclarés en config. Un `PreToolUse` de plugin écrit
    /// contre le contrat historique — exit 1 sur une erreur interne, dépendance
    /// absente — passait avant ce commit et doit continuer de passer.
    #[tokio::test]
    async fn a_non_zero_exit_blocks_a_config_hook_but_not_a_plugin_hook() {
        let tmp = tempfile::tempdir().unwrap();
        let root = write_plugin(
            tmp.path(),
            "p",
            r#"{"hooks":{"PreToolUse":[{"hooks":[{"type":"command","command":"exit 1"}]}]}}"#,
        );
        let plugin = make_manager(vec![DiscoveredPlugin {
            name: "p".into(),
            root,
            scope: PluginScope::User,
        }]);
        assert_eq!(
            plugin
                .emit_blocking(
                    HookEvent::PreToolUse,
                    HookContext::new(HookEvent::PreToolUse, "s"),
                    None,
                    "",
                )
                .await,
            HookDecision::Allow,
            "un plugin garde son contrat : seul l'exit 2 bloque",
        );

        let configured = HookManager::from_entries(
            vec![config::HookEntry {
                event: "pre_tool_use".into(),
                command: "echo refuse >&2; exit 1".into(),
                matcher: None,
                timeout_s: None,
            }],
            tmp.path(),
            "user",
        );
        assert_eq!(
            configured
                .emit_blocking(
                    HookEvent::PreToolUse,
                    HookContext::new(HookEvent::PreToolUse, "s"),
                    None,
                    "",
                )
                .await,
            HookDecision::Deny {
                reason: "refuse".into(),
                plugin: "user".into(),
            },
        );
    }

    /// Même partage pour le timeout : fail-closed est la règle S6, pas celle
    /// des plugins — un hook de plugin lent bloquerait un appel d'outil sans
    /// avoir jamais été écrit contre ce contrat.
    #[tokio::test]
    async fn a_timeout_fails_closed_only_for_a_config_hook() {
        let tmp = tempfile::tempdir().unwrap();
        let root = write_plugin(
            tmp.path(),
            "p",
            r#"{"hooks":{"PreToolUse":[{"hooks":[{"type":"command","command":"sleep 30","timeout":1}]}]}}"#,
        );
        let plugin = make_manager(vec![DiscoveredPlugin {
            name: "p".into(),
            root,
            scope: PluginScope::User,
        }]);
        assert_eq!(
            plugin
                .emit_blocking(
                    HookEvent::PreToolUse,
                    HookContext::new(HookEvent::PreToolUse, "s"),
                    None,
                    "",
                )
                .await,
            HookDecision::Allow,
        );

        let configured = HookManager::from_entries(
            vec![config::HookEntry {
                event: "pre_tool_use".into(),
                command: "sleep 30".into(),
                matcher: None,
                timeout_s: Some(1),
            }],
            tmp.path(),
            "user",
        );
        assert_eq!(
            configured
                .emit_blocking(
                    HookEvent::PreToolUse,
                    HookContext::new(HookEvent::PreToolUse, "s"),
                    None,
                    "",
                )
                .await,
            HookDecision::Deny {
                reason: TIMEOUT_DENIAL.into(),
                plugin: "user".into(),
            },
        );
    }

    #[test]
    fn merge_paths_keeps_login_entries_first() {
        assert_eq!(
            merge_paths("/opt/homebrew/bin:/bin", "/bin:/usr/bin:/custom/bin"),
            "/opt/homebrew/bin:/bin:/usr/bin:/custom/bin"
        );
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn command_hooks_repair_path_when_enabled() {
        let tmp = tempfile::tempdir().unwrap();
        let login_bin = tmp.path().join("login-bin");
        std::fs::create_dir(&login_bin).unwrap();

        let fake_shell = tmp.path().join("fake-login-shell");
        std::fs::write(
            &fake_shell,
            "#!/bin/sh\nprintf '%s\\n' \"$FAKE_LOGIN_PATH\"\n",
        )
        .unwrap();
        let helper = login_bin.join("hook-visible-tool");
        std::fs::write(&helper, "#!/bin/sh\nprintf 'hook-visible-tool-ran'\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for path in [&fake_shell, &helper] {
                let mut perms = std::fs::metadata(path).unwrap().permissions();
                perms.set_mode(0o755);
                std::fs::set_permissions(path, perms).unwrap();
            }
        }

        let fake_shell = fake_shell.to_string_lossy().into_owned();
        let fake_login_path = format!("{}:/usr/bin:/bin", login_bin.display());
        let _guard = env_lock::lock_env([
            ("KAJI_SHELL", Some(fake_shell.as_str())),
            ("FAKE_LOGIN_PATH", Some(fake_login_path.as_str())),
            (
                "PATH",
                Some("/Applications/Kaji.app/Contents/Resources/bin:/usr/bin:/bin:/usr/sbin:/sbin"),
            ),
        ]);

        let output = run_command_hook(
            "hook-visible-tool",
            tmp.path(),
            "{}",
            Duration::from_secs(5),
            true,
        )
        .await
        .unwrap();

        assert!(output.status.success());
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            "hook-visible-tool-ran"
        );
    }

    #[tokio::test]
    async fn matcher_filters_by_tool_name() {
        let tmp = tempfile::tempdir().unwrap();
        let marker = tmp.path().join("ran.txt");
        let hooks = format!(
            r#"{{"hooks":{{"PreToolUse":[{{"matcher":"developer__shell","hooks":[{{"type":"command","command":"touch {}"}}]}}]}}}}"#,
            marker.to_string_lossy(),
        );
        let root = write_plugin(tmp.path(), "p", &hooks);
        let mgr = make_manager(vec![DiscoveredPlugin {
            name: "p".into(),
            root,
            scope: PluginScope::User,
        }]);

        // Non-matching tool: marker not created.
        mgr.emit(
            HookEvent::PreToolUse,
            HookContext::new(HookEvent::PreToolUse, "s").with_tool("other__tool", None),
        )
        .await;
        assert!(!marker.exists());

        // Matching tool: marker created.
        mgr.emit(
            HookEvent::PreToolUse,
            HookContext::new(HookEvent::PreToolUse, "s").with_tool("developer__shell", None),
        )
        .await;
        assert!(marker.exists());
    }
}
