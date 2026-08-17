use crate::config::paths::Paths;
use crate::permission::grants::{call_allowed_by_any, call_denied_by_any, GrantRule, Spec};
use rmcp::model::{JsonObject, Tool};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, RwLock};
use tempfile::NamedTempFile;
use tracing;

const PERMISSION_FILE: &str = "permission.yaml";

static PERMISSION_MANAGER: LazyLock<Arc<PermissionManager>> =
    LazyLock::new(|| Arc::new(PermissionManager::new(Paths::config_dir())));

/// Enum representing the possible permission levels for a tool.
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PermissionLevel {
    AlwaysAllow, // Tool can always be used without prompt
    AskBefore,   // Tool requires permission to be granted before use
    NeverAllow,  // Tool is never allowed to be used
}

/// Struct representing the configuration of permissions, categorized by level.
#[derive(Debug, Deserialize, Serialize, Default, Clone)]
pub struct PermissionConfig {
    pub always_allow: Vec<String>, // List of tools that are always allowed
    pub ask_before: Vec<String>,   // List of tools that require user consent
    pub never_allow: Vec<String>,  // List of tools that are never allowed
}

impl PermissionConfig {
    /// Drops every `always_allow` entry of a tool, tool-wide and per-call alike,
    /// reporting whether anything was dropped.
    fn drop_grants(&mut self, tool_name: &str) -> bool {
        let before = self.always_allow.len();
        self.always_allow
            .retain(|entry| GrantRule::parse(entry).tool_name != tool_name);
        self.always_allow.len() != before
    }

    fn set_level(&mut self, principal_name: &str, level: &PermissionLevel) {
        self.always_allow.retain(|p| p != principal_name);
        self.ask_before.retain(|p| p != principal_name);
        self.never_allow.retain(|p| p != principal_name);

        match level {
            PermissionLevel::AlwaysAllow => self.always_allow.push(principal_name.to_string()),
            PermissionLevel::AskBefore => self.ask_before.push(principal_name.to_string()),
            PermissionLevel::NeverAllow => self.never_allow.push(principal_name.to_string()),
        }
    }
}

/// PermissionManager manages permission configurations for various tools.
#[derive(Debug)]
pub struct PermissionManager {
    config_path: PathBuf,
    permission_map: RwLock<HashMap<String, PermissionConfig>>,
}

// Constants representing specific permission categories
const USER_PERMISSION: &str = "user";
const SMART_APPROVE_PERMISSION: &str = "smart_approve";

impl PermissionManager {
    pub fn new(config_dir: PathBuf) -> Self {
        let permission_path = config_dir.join(PERMISSION_FILE);
        let permission_map = if permission_path.exists() {
            let file_contents =
                fs::read_to_string(&permission_path).expect("Failed to read permission.yaml");
            serde_yaml::from_str(&file_contents).unwrap_or_else(|e| {
                tracing::error!(
                    "Failed to parse {}: {}. Refusing to start with corrupted permission config.",
                    permission_path.display(),
                    e,
                );
                panic!(
                    "Corrupted permission config at {}. Fix or remove the file to continue.",
                    permission_path.display(),
                );
            })
        } else {
            // Consolidate directory creation for re-use in global singleton or ACP.
            fs::create_dir_all(&config_dir).expect("Failed to create config directory");
            HashMap::new()
        };
        PermissionManager {
            config_path: permission_path,
            permission_map: RwLock::new(permission_map),
        }
    }

    pub fn instance() -> Arc<PermissionManager> {
        Arc::clone(&PERMISSION_MANAGER)
    }

    /// Returns a list of all the names (keys) in the permission map.
    pub fn get_permission_names(&self) -> Vec<String> {
        self.permission_map
            .read()
            .unwrap()
            .keys()
            .cloned()
            .collect()
    }

    /// Retrieves the user permission level for a specific tool. Per-call grants report
    /// as `AlwaysAllow` so that a configuration UI can see, and revoke, them.
    pub fn get_user_permission(&self, principal_name: &str) -> Option<PermissionLevel> {
        let level = self.get_permission(USER_PERMISSION, principal_name);
        if level == Some(PermissionLevel::NeverAllow) {
            return level;
        }
        if self.has_user_grant(principal_name) {
            return Some(PermissionLevel::AlwaysAllow);
        }
        level
    }

    /// The permission level that holds for the tool as a whole, for callers that report a
    /// tool's permission to a client. Unlike [`Self::get_user_permission`], a per-call grant
    /// stays invisible: reporting it as `AlwaysAllow` would announce an auto-approval the
    /// inspector only gives to the calls the grant covers.
    pub fn get_tool_wide_permission(&self, principal_name: &str) -> Option<PermissionLevel> {
        let covers_the_tool = |list: fn(&PermissionConfig) -> &Vec<String>| {
            self.rules_of(list)
                .iter()
                .any(|rule| rule.tool_name == principal_name && rule.spec.is_none())
        };
        if covers_the_tool(|config| &config.never_allow) {
            return Some(PermissionLevel::NeverAllow);
        }
        if covers_the_tool(|config| &config.always_allow) {
            return Some(PermissionLevel::AlwaysAllow);
        }
        self.get_permission(USER_PERMISSION, principal_name)
    }

    /// Whether the tool carries a name-level `ask_before` entry, which per-call grants
    /// narrow but never cancel.
    pub fn asks_before(&self, principal_name: &str) -> bool {
        self.get_permission(USER_PERMISSION, principal_name) == Some(PermissionLevel::AskBefore)
    }

    /// Retrieves the smart approve permission level for a specific tool.
    pub fn get_smart_approve_permission(&self, principal_name: &str) -> Option<PermissionLevel> {
        self.get_permission(SMART_APPROVE_PERMISSION, principal_name)
    }

    /// Retrieves the config file path.
    pub fn get_config_path(&self) -> &Path {
        self.config_path.as_path()
    }

    pub fn apply_tool_annotations(&self, tools: &[Tool]) {
        let mut write_annotated = Vec::new();
        for tool in tools {
            let Some(anns) = &tool.annotations else {
                continue;
            };
            if anns.read_only_hint == Some(false) {
                write_annotated.push(tool.name.to_string());
            }
        }
        if !write_annotated.is_empty() {
            self.bulk_update_smart_approve_permissions(
                &write_annotated,
                PermissionLevel::AskBefore,
            );
        }
    }

    fn bulk_update_smart_approve_permissions(&self, tool_names: &[String], level: PermissionLevel) {
        let mut map = self.permission_map.write().unwrap();
        let permission_config = map.entry(SMART_APPROVE_PERMISSION.to_string()).or_default();

        for tool_name in tool_names {
            permission_config.set_level(tool_name, &level);
        }

        self.persist(&map);
    }

    /// Commits the whole map through a temporary file of the config directory renamed
    /// onto the config path, so a reader never sees a half-written file and an
    /// interrupted write never truncates the previous one. Callers hold the write lock
    /// across this, which keeps the file's contents ordered like the map's mutations.
    fn persist(&self, map: &HashMap<String, PermissionConfig>) {
        let yaml_content =
            serde_yaml::to_string(map).expect("Failed to serialize permission config");
        let directory = self.config_path.parent().unwrap_or(Path::new("."));
        let mut file =
            NamedTempFile::new_in(directory).expect("Failed to write to permission.yaml");
        file.write_all(yaml_content.as_bytes())
            .expect("Failed to write to permission.yaml");
        file.persist(&self.config_path)
            .expect("Failed to write to permission.yaml");
    }

    /// Helper function to retrieve the permission level for a specific permission category and tool.
    fn get_permission(&self, name: &str, principal_name: &str) -> Option<PermissionLevel> {
        let map = self.permission_map.read().unwrap();
        // Check if the permission category exists in the map
        if let Some(permission_config) = map.get(name) {
            // Check the permission levels for the given tool
            if permission_config
                .always_allow
                .contains(&principal_name.to_string())
            {
                return Some(PermissionLevel::AlwaysAllow);
            } else if permission_config
                .ask_before
                .contains(&principal_name.to_string())
            {
                return Some(PermissionLevel::AskBefore);
            } else if permission_config
                .never_allow
                .contains(&principal_name.to_string())
            {
                return Some(PermissionLevel::NeverAllow);
            }
        }
        None // Return None if no matching permission level is found
    }

    /// Whether the user's `always_allow` list covers this exact call.
    pub fn is_call_allowed(&self, tool_name: &str, arguments: Option<&JsonObject>) -> bool {
        let rules = self.rules_of(|config| &config.always_allow);
        call_allowed_by_any(&rules, tool_name, arguments)
    }

    /// Whether any rule of the user's `never_allow` list catches this call.
    pub fn is_call_denied(&self, tool_name: &str, arguments: Option<&JsonObject>) -> bool {
        let rules = self.rules_of(|config| &config.never_allow);
        call_denied_by_any(&rules, tool_name, arguments)
    }

    fn rules_of(&self, list: impl Fn(&PermissionConfig) -> &Vec<String>) -> Vec<GrantRule> {
        let map = self.permission_map.read().unwrap();
        map.get(USER_PERMISSION)
            .map(|config| list(config).iter().map(|e| GrantRule::parse(e)).collect())
            .unwrap_or_default()
    }

    pub fn get_user_grants(&self) -> Vec<GrantRule> {
        self.rules_of(|config| &config.always_allow)
    }

    fn has_user_grant(&self, tool_name: &str) -> bool {
        self.get_user_grants()
            .iter()
            .any(|grant| grant.tool_name == tool_name)
    }

    /// Records an `always_allow` grant, dropping the rules the new one subsumes.
    /// `spec` of `None` grants the tool as a whole. A grant only ever widens what
    /// a single call may do: `never_allow` outranks it, and the tool-wide
    /// `ask_before` stays in place to catch everything the grant does not cover
    /// (see [`Self::asks_before`]). Only [`Self::update_user_permission`] moves a
    /// tool out of `ask_before`.
    pub fn add_grant(&self, tool_name: &str, spec: Option<&Spec>) {
        let new_rule = GrantRule::new(tool_name, spec.cloned());
        let mut map = self.permission_map.write().unwrap();
        let permission_config = map.entry(USER_PERMISSION.to_string()).or_default();

        let already_covered = permission_config
            .always_allow
            .iter()
            .any(|entry| GrantRule::parse(entry).covers(&new_rule));
        if !already_covered {
            permission_config
                .always_allow
                .retain(|entry| !new_rule.covers(&GrantRule::parse(entry)));
            permission_config.always_allow.push(new_rule.to_string());
        }

        self.persist(&map);
    }

    /// Updates the user permission level for a specific tool. Setting any level revokes
    /// the tool's per-call grants: they are what the level replaces. Revocation and level
    /// are committed together, so no reader ever sees the tool stripped of its grants
    /// without its new level.
    pub fn update_user_permission(&self, principal_name: &str, level: PermissionLevel) {
        let mut map = self.permission_map.write().unwrap();
        let permission_config = map.entry(USER_PERMISSION.to_string()).or_default();
        permission_config.drop_grants(principal_name);
        permission_config.set_level(principal_name, &level);

        self.persist(&map);
    }

    /// Drops every `always_allow` entry of a tool, tool-wide and per-call alike.
    pub fn remove_grants(&self, tool_name: &str) {
        let mut map = self.permission_map.write().unwrap();
        let Some(permission_config) = map.get_mut(USER_PERMISSION) else {
            return;
        };
        if !permission_config.drop_grants(tool_name) {
            return;
        }

        self.persist(&map);
    }

    /// Updates the smart approve permission level for a specific tool.
    pub fn update_smart_approve_permission(&self, principal_name: &str, level: PermissionLevel) {
        self.update_permission(SMART_APPROVE_PERMISSION, principal_name, level)
    }

    /// Helper function to update a permission level for a specific tool in a given permission category.
    fn update_permission(&self, name: &str, principal_name: &str, level: PermissionLevel) {
        let mut map = self.permission_map.write().unwrap();
        map.entry(name.to_string())
            .or_default()
            .set_level(principal_name, &level);

        self.persist(&map);
    }

    /// Removes all entries where the principal name starts with the given extension name.
    pub fn remove_extension(&self, extension_name: &str) {
        let mut map = self.permission_map.write().unwrap();
        for permission_config in map.values_mut() {
            permission_config
                .always_allow
                .retain(|p| !p.starts_with(extension_name));
            permission_config
                .ask_before
                .retain(|p| !p.starts_with(extension_name));
            permission_config
                .never_allow
                .retain(|p| !p.starts_with(extension_name));
        }

        self.persist(&map);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission::grants::derive_shell_grant;
    use rmcp::model::ToolAnnotations;
    use rmcp::object;
    use tempfile::TempDir;

    // Helper function to create a test instance of PermissionManager with a temp dir
    fn create_test_permission_manager() -> (PermissionManager, TempDir) {
        let temp_dir = TempDir::new().unwrap();
        let manager = PermissionManager::new(temp_dir.path().to_path_buf());
        (manager, temp_dir)
    }

    #[test]
    fn test_get_permission_names_empty() {
        let (manager, _temp_dir) = create_test_permission_manager();

        assert!(manager.get_permission_names().is_empty());
    }

    #[test]
    fn test_update_user_permission() {
        let (manager, _temp_dir) = create_test_permission_manager();
        manager.update_user_permission("tool1", PermissionLevel::AlwaysAllow);

        let permission = manager.get_user_permission("tool1");
        assert_eq!(permission, Some(PermissionLevel::AlwaysAllow));
    }

    #[test]
    fn test_update_smart_approve_permission() {
        let (manager, _temp_dir) = create_test_permission_manager();
        manager.update_smart_approve_permission("tool2", PermissionLevel::AskBefore);

        let permission = manager.get_smart_approve_permission("tool2");
        assert_eq!(permission, Some(PermissionLevel::AskBefore));
    }

    #[test]
    fn test_get_permission_not_found() {
        let (manager, _temp_dir) = create_test_permission_manager();

        let permission = manager.get_user_permission("non_existent_tool");
        assert_eq!(permission, None);
    }

    #[test]
    fn test_permission_levels() {
        let (manager, _temp_dir) = create_test_permission_manager();

        manager.update_user_permission("tool4", PermissionLevel::AlwaysAllow);
        manager.update_user_permission("tool5", PermissionLevel::AskBefore);
        manager.update_user_permission("tool6", PermissionLevel::NeverAllow);

        // Check the permission levels
        assert_eq!(
            manager.get_user_permission("tool4"),
            Some(PermissionLevel::AlwaysAllow)
        );
        assert_eq!(
            manager.get_user_permission("tool5"),
            Some(PermissionLevel::AskBefore)
        );
        assert_eq!(
            manager.get_user_permission("tool6"),
            Some(PermissionLevel::NeverAllow)
        );
    }

    #[test]
    fn test_permission_update_replaces_existing_level() {
        let (manager, _temp_dir) = create_test_permission_manager();

        // Initially AlwaysAllow
        manager.update_user_permission("tool7", PermissionLevel::AlwaysAllow);
        assert_eq!(
            manager.get_user_permission("tool7"),
            Some(PermissionLevel::AlwaysAllow)
        );

        // Now change to NeverAllow
        manager.update_user_permission("tool7", PermissionLevel::NeverAllow);
        assert_eq!(
            manager.get_user_permission("tool7"),
            Some(PermissionLevel::NeverAllow)
        );

        // Ensure it's removed from other levels
        let map = manager.permission_map.read().unwrap();
        let config = map.get(USER_PERMISSION).unwrap();
        assert!(!config.always_allow.contains(&"tool7".to_string()));
        assert!(!config.ask_before.contains(&"tool7".to_string()));
        assert!(config.never_allow.contains(&"tool7".to_string()));
    }

    #[test]
    fn test_remove_extension() {
        let (manager, _temp_dir) = create_test_permission_manager();
        manager.update_user_permission("prefix__tool1", PermissionLevel::AlwaysAllow);
        manager.update_user_permission("nonprefix__tool2", PermissionLevel::AlwaysAllow);
        manager.update_user_permission("prefix__tool3", PermissionLevel::AskBefore);

        // Remove entries starting with "prefix"
        manager.remove_extension("prefix");

        let map = manager.permission_map.read().unwrap();
        let config = map.get(USER_PERMISSION).unwrap();

        // Verify entries with "prefix" are removed
        assert!(!config.always_allow.contains(&"prefix__tool1".to_string()));
        assert!(!config.ask_before.contains(&"prefix__tool3".to_string()));

        // Verify other entries remain
        assert!(config
            .always_allow
            .contains(&"nonprefix__tool2".to_string()));
    }

    #[test]
    #[should_panic(expected = "Corrupted permission config")]
    fn test_corrupted_permission_file_panics() {
        let temp_dir = TempDir::new().unwrap();
        let permission_path = temp_dir.path().join(PERMISSION_FILE);
        fs::write(&permission_path, "{{invalid yaml: [broken").unwrap();
        PermissionManager::new(temp_dir.path().to_path_buf());
    }

    const SHELL: &str = "shell";

    fn shell_call(command: &str) -> rmcp::model::JsonObject {
        object!({ "command": command })
    }

    fn grants(manager: &PermissionManager) -> Vec<String> {
        manager
            .get_user_grants()
            .iter()
            .map(GrantRule::to_string)
            .collect()
    }

    #[test]
    fn grants_survive_a_reload() {
        let temp_dir = TempDir::new().unwrap();
        let manager = PermissionManager::new(temp_dir.path().to_path_buf());
        manager.add_grant(SHELL, Some(&Spec::prefix("cargo test")));
        manager.add_grant(SHELL, Some(&Spec::exact("rm -rf *")));
        manager.add_grant("other__tool", None);

        let reloaded = PermissionManager::new(temp_dir.path().to_path_buf());
        assert_eq!(
            grants(&reloaded),
            vec!["shell(cargo test *)", "shell(rm -rf \\*)", "other__tool"]
        );
        assert!(reloaded.is_call_allowed(SHELL, Some(&shell_call("cargo test --all"))));
        assert!(!reloaded.is_call_allowed(SHELL, Some(&shell_call("cargo build"))));
        assert!(reloaded.is_call_allowed("other__tool", None));
    }

    #[test]
    fn a_literal_star_grant_never_reloads_as_a_prefix() {
        let temp_dir = TempDir::new().unwrap();
        let manager = PermissionManager::new(temp_dir.path().to_path_buf());
        manager.add_grant(SHELL, Some(&derive_shell_grant("rm -rf *")));

        let reloaded = PermissionManager::new(temp_dir.path().to_path_buf());
        assert!(reloaded.is_call_allowed(SHELL, Some(&shell_call("rm -rf *"))));
        assert!(!reloaded.is_call_allowed(SHELL, Some(&shell_call("rm -rf /"))));
        assert!(!reloaded.is_call_allowed(SHELL, Some(&shell_call("rm -rf /home"))));
    }

    #[test]
    fn a_new_grant_absorbs_the_rules_it_covers() {
        let (manager, _temp_dir) = create_test_permission_manager();
        manager.add_grant(SHELL, Some(&Spec::exact("cargo test -p kaji")));
        manager.add_grant(SHELL, Some(&Spec::prefix("cargo build")));
        manager.add_grant(SHELL, Some(&Spec::prefix("cargo test")));

        assert_eq!(
            grants(&manager),
            vec!["shell(cargo build *)", "shell(cargo test *)"]
        );
    }

    #[test]
    fn a_covered_grant_is_not_added() {
        let (manager, _temp_dir) = create_test_permission_manager();
        manager.add_grant(SHELL, Some(&Spec::prefix("cargo test")));
        manager.add_grant(SHELL, Some(&Spec::exact("cargo test -p kaji")));

        assert_eq!(grants(&manager), vec!["shell(cargo test *)"]);
    }

    #[test]
    fn a_tool_wide_grant_absorbs_every_rule_of_that_tool() {
        let (manager, _temp_dir) = create_test_permission_manager();
        manager.add_grant(SHELL, Some(&Spec::prefix("cargo test")));
        manager.add_grant("other__tool", Some(&Spec::exact("kept")));
        manager.add_grant(SHELL, None);

        assert_eq!(grants(&manager), vec!["other__tool(kept)", "shell"]);
    }

    #[test]
    fn a_legacy_tool_name_entry_allows_every_call() {
        let (manager, _temp_dir) = create_test_permission_manager();
        manager.update_user_permission(SHELL, PermissionLevel::AlwaysAllow);

        assert!(manager.is_call_allowed(SHELL, Some(&shell_call("rm -rf /"))));
        assert!(!manager.is_call_denied(SHELL, Some(&shell_call("rm -rf /"))));
    }

    #[test]
    fn a_denied_tool_name_denies_every_call() {
        let (manager, _temp_dir) = create_test_permission_manager();
        manager.update_user_permission(SHELL, PermissionLevel::NeverAllow);

        assert!(manager.is_call_denied(SHELL, Some(&shell_call("cargo test"))));
        assert!(!manager.is_call_allowed(SHELL, Some(&shell_call("cargo test"))));
    }

    #[test]
    fn a_denied_call_rule_catches_every_stage() {
        let (manager, _temp_dir) = create_test_permission_manager();
        {
            let mut map = manager.permission_map.write().unwrap();
            map.entry(USER_PERMISSION.to_string())
                .or_default()
                .never_allow
                .push("shell(rm -rf *)".to_string());
        }

        assert!(manager.is_call_denied(SHELL, Some(&shell_call("rm -rf /"))));
        assert!(manager.is_call_denied(SHELL, Some(&shell_call("rm -rf / && true"))));
        assert!(manager.is_call_denied(SHELL, Some(&shell_call("true && rm -rf /"))));
        assert!(manager.is_call_denied(SHELL, Some(&shell_call("true\nrm -rf /"))));
        assert!(manager.is_call_denied(SHELL, Some(&shell_call("true\r\nrm -rf /"))));
        assert!(!manager.is_call_denied(SHELL, Some(&shell_call("cargo test"))));
    }

    #[test]
    fn a_prefix_grant_is_not_spliced_by_a_line_break() {
        let (manager, _temp_dir) = create_test_permission_manager();
        manager.add_grant(SHELL, Some(&derive_shell_grant("cargo test -p kaji")));

        assert!(manager.is_call_allowed(SHELL, Some(&shell_call("cargo test --all"))));
        assert!(!manager.is_call_allowed(SHELL, Some(&shell_call("cargo test\nrm -rf /"))));
        assert!(!manager.is_call_allowed(SHELL, Some(&shell_call("cargo test\r\nrm -rf /"))));
    }

    #[test]
    fn a_grant_never_cancels_ask_before_nor_a_denial() {
        let (manager, _temp_dir) = create_test_permission_manager();
        manager.update_user_permission(SHELL, PermissionLevel::AskBefore);
        manager.add_grant(SHELL, Some(&Spec::prefix("cargo test")));

        assert_eq!(
            manager.get_user_permission(SHELL),
            Some(PermissionLevel::AlwaysAllow)
        );
        assert!(manager.is_call_allowed(SHELL, Some(&shell_call("cargo test --all"))));
        // The narrow grant covers `cargo test`; everything else the tool-wide
        // ask_before still catches (`PermissionInspector` reads the grants
        // first, then `asks_before`).
        assert!(manager.asks_before(SHELL));
        assert!(!manager.is_call_allowed(SHELL, Some(&shell_call("cargo build"))));

        manager.update_user_permission("other__tool", PermissionLevel::NeverAllow);
        manager.add_grant("other__tool", Some(&Spec::exact("anything")));

        assert_eq!(
            manager.get_user_permission("other__tool"),
            Some(PermissionLevel::NeverAllow)
        );
        assert!(manager.is_call_denied("other__tool", None));
    }

    #[test]
    fn a_call_grant_is_visible_and_revocable_as_a_permission_level() {
        let (manager, _temp_dir) = create_test_permission_manager();
        manager.add_grant(SHELL, Some(&Spec::prefix("cargo test")));
        manager.add_grant(SHELL, Some(&Spec::prefix("git status")));

        assert_eq!(
            manager.get_user_permission(SHELL),
            Some(PermissionLevel::AlwaysAllow)
        );

        manager.update_user_permission(SHELL, PermissionLevel::AskBefore);

        assert_eq!(
            manager.get_user_permission(SHELL),
            Some(PermissionLevel::AskBefore)
        );
        assert!(grants(&manager).is_empty());
        assert!(!manager.is_call_allowed(SHELL, Some(&shell_call("cargo test --all"))));
    }

    #[test]
    fn a_narrow_grant_is_never_reported_tool_wide() {
        let (manager, _temp_dir) = create_test_permission_manager();
        manager.add_grant(SHELL, Some(&Spec::prefix("cargo test")));

        assert_eq!(manager.get_tool_wide_permission(SHELL), None);
        assert_eq!(
            manager.get_user_permission(SHELL),
            Some(PermissionLevel::AlwaysAllow)
        );

        manager.update_user_permission(SHELL, PermissionLevel::AskBefore);
        manager.add_grant(SHELL, Some(&Spec::prefix("cargo test")));

        assert_eq!(
            manager.get_tool_wide_permission(SHELL),
            Some(PermissionLevel::AskBefore)
        );
        assert_eq!(
            manager.get_user_permission(SHELL),
            Some(PermissionLevel::AlwaysAllow)
        );
    }

    #[test]
    fn a_tool_wide_grant_is_reported_tool_wide() {
        let (manager, _temp_dir) = create_test_permission_manager();
        manager.update_user_permission(SHELL, PermissionLevel::AskBefore);
        manager.add_grant(SHELL, None);

        assert_eq!(
            manager.get_tool_wide_permission(SHELL),
            Some(PermissionLevel::AlwaysAllow)
        );
        assert_eq!(
            manager.get_user_permission(SHELL),
            Some(PermissionLevel::AlwaysAllow)
        );
    }

    #[test]
    fn a_denial_outranks_a_grant_in_the_tool_wide_view() {
        let (manager, _temp_dir) = create_test_permission_manager();
        manager.update_user_permission(SHELL, PermissionLevel::NeverAllow);
        manager.add_grant(SHELL, Some(&Spec::exact("cargo test")));
        manager.add_grant(SHELL, None);

        assert_eq!(
            manager.get_tool_wide_permission(SHELL),
            Some(PermissionLevel::NeverAllow)
        );
    }

    fn written_config(dir: &Path) -> HashMap<String, PermissionConfig> {
        let contents = fs::read_to_string(dir.join(PERMISSION_FILE)).unwrap();
        serde_yaml::from_str(&contents).unwrap()
    }

    #[test]
    fn revoking_a_grant_leaves_one_consistent_file_and_no_leftovers() {
        let (manager, temp_dir) = create_test_permission_manager();
        manager.add_grant(SHELL, Some(&Spec::prefix("cargo test")));
        manager.update_user_permission(SHELL, PermissionLevel::AskBefore);

        let entries: Vec<String> = fs::read_dir(temp_dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(entries, vec![PERMISSION_FILE.to_string()]);

        let config = written_config(temp_dir.path())
            .remove(USER_PERMISSION)
            .unwrap();
        assert!(config.always_allow.is_empty());
        assert_eq!(config.ask_before, vec![SHELL.to_string()]);
    }

    #[test]
    fn concurrent_writes_never_expose_a_partial_file() {
        use std::sync::atomic::{AtomicBool, Ordering};

        const WRITERS: usize = 8;
        const WRITES_PER_WRITER: usize = 20;

        let (manager, temp_dir) = create_test_permission_manager();
        let manager = Arc::new(manager);
        manager.update_user_permission("seed", PermissionLevel::AskBefore);

        let stop = Arc::new(AtomicBool::new(false));
        let reader = {
            let path = temp_dir.path().join(PERMISSION_FILE);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let contents = fs::read_to_string(&path).unwrap();
                    serde_yaml::from_str::<HashMap<String, PermissionConfig>>(&contents)
                        .expect("permission.yaml must always parse");
                }
            })
        };

        let writers: Vec<_> = (0..WRITERS)
            .map(|index| {
                let manager = Arc::clone(&manager);
                std::thread::spawn(move || {
                    for _ in 0..WRITES_PER_WRITER {
                        manager.update_user_permission(
                            &format!("tool{index}"),
                            PermissionLevel::AlwaysAllow,
                        );
                    }
                })
            })
            .collect();
        for writer in writers {
            writer.join().unwrap();
        }
        stop.store(true, Ordering::Relaxed);
        reader.join().unwrap();

        let config = written_config(temp_dir.path())
            .remove(USER_PERMISSION)
            .unwrap();
        for index in 0..WRITERS {
            assert!(config.always_allow.contains(&format!("tool{index}")));
        }
        assert_eq!(fs::read_dir(temp_dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn revoking_grants_leaves_other_tools_alone() {
        let (manager, _temp_dir) = create_test_permission_manager();
        manager.add_grant(SHELL, Some(&Spec::prefix("cargo test")));
        manager.add_grant("other__tool", None);

        manager.remove_grants(SHELL);

        assert_eq!(grants(&manager), vec!["other__tool"]);
    }

    use test_case::test_case;

    #[test_case(
        vec![Tool::new("tool".to_string(), String::new(), object!({"type": "object"}))
            .annotate(ToolAnnotations::new().read_only(false))],
        Some(PermissionLevel::AskBefore);
        "write_annotation_caches_ask"
    )]
    #[test_case(
        vec![Tool::new("tool".to_string(), String::new(), object!({"type": "object"}))],
        None;
        "unannotated_left_uncached"
    )]
    #[test_case(
        vec![Tool::new("tool".to_string(), String::new(), object!({"type": "object"}))
            .annotate(ToolAnnotations::new().read_only(true))],
        None;
        "readonly_annotation_skipped"
    )]
    fn test_apply_tool_annotations(tools: Vec<Tool>, expect_cache: Option<PermissionLevel>) {
        let (manager, _temp_dir) = create_test_permission_manager();
        manager.apply_tool_annotations(&tools);
        assert_eq!(manager.get_smart_approve_permission("tool"), expect_cache);
    }
}
