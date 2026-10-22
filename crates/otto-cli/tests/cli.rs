use std::process::{Command, Stdio};

fn command(home: &std::path::Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_otto-silo"));
    command
        .env("OTTO_SILO_HOME", home)
        .env("OTTO_SILO_CONFIG_DIR", home.join("config"))
        .env("OTTO_SILO_OFFLINE", "1")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .env_remove("GEMINI_API_KEY")
        .env_remove("GOOGLE_API_KEY")
        .env_remove("XAI_API_KEY")
        .env_remove("OTTO_SILO_MODEL")
        .stdin(Stdio::null());
    command
}

#[test]
fn server_status_without_daemon() {
    let home = tempfile::tempdir().unwrap();
    let status = command(home.path()).args(["server", "status"]).output().unwrap();
    assert!(status.status.success(), "{}", String::from_utf8_lossy(&status.stderr));
    assert_eq!(String::from_utf8(status.stdout).unwrap().trim(), "not running");
    assert!(!otto_proto::paths::socket(home.path()).exists(), "status must not autostart a daemon");
}

#[test]
fn status_models_and_missing_auth() {
    let home = tempfile::tempdir().unwrap();
    let status = command(home.path()).args(["auth", "status"]).output().unwrap();
    assert!(status.status.success(), "{}", String::from_utf8_lossy(&status.stderr));
    let text = String::from_utf8(status.stdout).unwrap();
    assert!(text.contains("anthropic\t-\tno"));
    assert!(text.contains("openai\t-\tno"));
    let models = command(home.path()).args(["models"]).output().unwrap();
    assert!(models.status.success());
    assert_eq!(String::from_utf8(models.stdout).unwrap().lines().count(), 1);
    let models = command(home.path()).args(["models", "--all", "--provider", "openai"]).output().unwrap();
    assert!(models.status.success());
    let text = String::from_utf8(models.stdout).unwrap();
    assert!(text.contains("openai/gpt-5.3-codex"));
    assert!(!text.contains("anthropic/"));
    assert!(text.contains("api_key,subscription"));
    for args in [vec!["ask", "hello"], vec!["ask", "--model", "anthropic/claude-sonnet-5-5", "hello"]] {
        let ask = command(home.path()).args(args).output().unwrap();
        assert_eq!(ask.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&ask.stderr).contains("otto-silo auth login"));
    }
}

#[test]
fn catalog_modes_and_unsupported_providers() {
    let home = tempfile::tempdir().unwrap();
    let models = command(home.path()).args(["models", "--all", "--provider", "openai", "--mode", "subscription"]).output().unwrap();
    assert!(models.status.success());
    let text = String::from_utf8(models.stdout).unwrap();
    assert!(text.contains("openai/gpt-6.1-sol"));
    assert!(text.contains("\tplan\t"));
    for id in ["gpt-5.5", "gpt-5.4", "gpt-5.3-codex"] {
        assert!(!text.contains(&format!("openai/{id}\t")));
    }
    // Every provider is implemented: google lists nothing until signed in, and is supported with --all.
    let models = command(home.path()).args(["models", "--provider", "google"]).output().unwrap();
    assert!(models.status.success());
    assert_eq!(String::from_utf8(models.stdout).unwrap().lines().count(), 1);
    for provider in ["google", "xai"] {
        let models = command(home.path()).args(["models", "--all", "--provider", provider]).output().unwrap();
        assert!(models.status.success());
        let text = String::from_utf8(models.stdout).unwrap();
        assert!(text.contains(&format!("\n{provider}/")), "{text}");
        assert!(!text.contains("not yet supported"), "{text}");
    }
    let unknown = command(home.path()).args(["models", "--provider", "nowhere"]).output().unwrap();
    assert!(!unknown.status.success());
    let models = command(home.path()).env("OPENAI_API_KEY", "test-key").args(["models", "--mode", "subscription"]).output().unwrap();
    assert!(models.status.success());
    assert_eq!(String::from_utf8(models.stdout).unwrap().lines().count(), 1);
}

#[test]
fn api_key_login_and_logout() {
    let home = tempfile::tempdir().unwrap();
    let login = command(home.path()).args(["auth", "login", "anthropic", "--api-key=test-key"]).output().unwrap();
    assert!(login.status.success(), "{}", String::from_utf8_lossy(&login.stderr));
    let status = command(home.path()).args(["auth", "status"]).output().unwrap();
    assert!(String::from_utf8_lossy(&status.stdout).contains("anthropic\tapi_key\tyes"));
    assert!(!String::from_utf8_lossy(&status.stdout).contains("test-key"));
    let models = command(home.path()).args(["models"]).output().unwrap();
    assert!(models.status.success());
    let text = String::from_utf8(models.stdout).unwrap();
    assert!(text.contains("anthropic/"));
    assert!(!text.contains("openai/"));
    assert!(command(home.path()).args(["auth", "logout", "anthropic"]).status().unwrap().success());
    let status = command(home.path()).args(["auth", "status"]).output().unwrap();
    assert!(String::from_utf8_lossy(&status.stdout).contains("anthropic\t-\tno"));
}
