#[cfg(unix)]
mod unix {
    use kaji::agents::platform_extensions::developer::shell::{ShellParams, ShellTool};
    use std::process::Command;

    #[test]
    fn shell_filters_credentials_without_changing_parent_environment() {
        let directory = tempfile::tempdir().unwrap();
        let config_directory = directory.path().join("config");
        std::fs::create_dir(&config_directory).unwrap();
        let config = config_directory.join("config.yaml");
        std::fs::write(&config, "{}").unwrap();
        let custom = config_directory.join("custom_providers");
        std::fs::create_dir(&custom).unwrap();
        std::fs::write(custom.join("security.json"), r#"{
            "name":"custom_security", "display_name":"Security fixture", "engine":"openai", "base_url":"https://invalid.test",
            "api_key_env":"fixture_provider_key", "models":[{"name":"fixture", "context_limit":4096}]
        }"#).unwrap();
        let startup = directory.path().join("startup.sh");
        std::fs::write(&startup, "printf STARTUP_SECRET_LEAK\\n\n").unwrap();
        let result = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "unix::isolated_shell_environment_fixture",
                "--ignored",
                "--nocapture",
            ])
            .env("KAJI_PATH_ROOT", directory.path())
            .env("KAJI_DISABLE_KEYRING", "1")
            .env("KAJI_SHELL", "/bin/bash")
            .env("OPENAI_API_KEY", "fake-parent-provider-key")
            .env("FIXTURE_PROVIDER_KEY", "fake-custom-provider-key")
            .env("REFRESHED_PROVIDER_KEY", "fake-refreshed-provider-key")
            .env("BASH_ENV", &startup)
            .env("ENV", &startup)
            .env("KAJI_SECURITY_DEV_VARIABLE", "preserved-development-value")
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
    }

    #[tokio::test]
    #[ignore = "run only in the controlled subprocess above"]
    async fn isolated_shell_environment_fixture() {
        assert_eq!(
            std::env::var("OPENAI_API_KEY").unwrap(),
            "fake-parent-provider-key"
        );
        let root = std::path::PathBuf::from(std::env::var_os("KAJI_PATH_ROOT").unwrap());
        assert_eq!(
            kaji::config::paths::Paths::config_dir(),
            root.join("config")
        );
        let custom = kaji::providers::get_from_registry("custom_security")
            .await
            .unwrap();
        assert!(custom
            .metadata()
            .config_keys
            .iter()
            .any(|key| key.secret && key.name == "fixture_provider_key"));
        let tool = ShellTool::new(false).unwrap();
        let result = tool.shell(ShellParams {
            command: "printf 'parent:%s:%s:%s:%s:%s\\n' \"${OPENAI_API_KEY-unset}\" \"${FIXTURE_PROVIDER_KEY-unset}\" \"${BASH_ENV-unset}\" \"${ENV-unset}\" \"$KAJI_SECURITY_DEV_VARIABLE\"; /bin/sh -c 'printf \"child:%s:%s\\n\" \"${OPENAI_API_KEY-unset}\" \"${FIXTURE_PROVIDER_KEY-unset}\"'".to_owned(),
            timeout_secs: Some(5),
        }).await;
        assert_eq!(result.is_error, Some(false));
        let stdout = result.structured_content.unwrap()["stdout"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_eq!(
            stdout,
            "parent:unset:unset:unset:unset:preserved-development-value\nchild:unset:unset"
        );
        assert_eq!(
            std::env::var("OPENAI_API_KEY").unwrap(),
            "fake-parent-provider-key"
        );
        assert_eq!(
            std::env::var("FIXTURE_PROVIDER_KEY").unwrap(),
            "fake-custom-provider-key"
        );
        let directory = std::path::PathBuf::from(std::env::var_os("KAJI_PATH_ROOT").unwrap());
        std::fs::write(
            directory.join("config/custom_providers/refresh.json"),
            r#"{
            "name":"custom_refresh", "display_name":"Refresh fixture", "engine":"openai",
            "base_url":"https://invalid.test", "api_key_env":"REFRESHED_PROVIDER_KEY",
            "models":[{"name":"fixture", "context_limit":4096}]
        }"#,
        )
        .unwrap();
        kaji::providers::refresh_custom_providers().await.unwrap();
        let result = tool.shell_with_cwd(ShellParams {
            command: "printf '%s:%s:%s' \"${REFRESHED_PROVIDER_KEY-unset}\" \"$AGENT_SESSION_ID\" \"$PWD\"".to_owned(),
            timeout_secs: Some(5),
        }, Some(&directory), Some("fixture-session"), tokio_util::sync::CancellationToken::new()).await;
        assert_eq!(result.is_error, Some(false));
        let stdout = result.structured_content.unwrap()["stdout"]
            .as_str()
            .unwrap()
            .to_owned();
        let expected_dir = std::fs::canonicalize(directory).unwrap();
        assert_eq!(
            stdout,
            format!("unset:fixture-session:{}", expected_dir.display())
        );
        assert_eq!(
            std::env::var("REFRESHED_PROVIDER_KEY").unwrap(),
            "fake-refreshed-provider-key"
        );
        let providers_dir = expected_dir.join("config/custom_providers");
        std::fs::rename(&providers_dir, expected_dir.join("config/saved_providers")).unwrap();
        std::fs::write(&providers_dir, "not a directory").unwrap();
        assert!(kaji::providers::refresh_custom_providers().await.is_err());
        let result = tool.shell(ShellParams {
            command: "printf '%s:%s' \"${FIXTURE_PROVIDER_KEY-unset}\" \"${REFRESHED_PROVIDER_KEY-unset}\"".to_owned(),
            timeout_secs: Some(5),
        }).await;
        assert_eq!(result.is_error, Some(false));
        assert_eq!(result.structured_content.unwrap()["stdout"], "unset:unset");
    }
}
