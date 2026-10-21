//! Harness status hooks; existing configuration belongs to the user.

use anyhow::{Context, bail};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

const OPENCODE: &str = r#"export const SiloPlugin = async ({ $ }) => {
  const report = async (state, kind) => {
    try {
      if (kind) await $`silo report ${state} --app opencode --kind ${kind}`.quiet();
      else await $`silo report ${state} --app opencode`.quiet();
    } catch {}
  };
  return {
    "chat.message": async () => { await report("working"); },
    event: async ({ event }) => {
      if (event.type === "session.idle") await report("done");
      if (event.type === "permission.asked" || event.type === "permission.updated") await report("blocked", "permission");
      if (event.type === "session.error") await report("error");
    },
  };
};
"#;

/// Install or print hooks in `home`, without replacing unrelated settings.
///
/// # Errors
/// Malformed configuration, unknown harness or filesystem failure.
pub fn install(home: &Path, harness: &str, print: bool) -> anyhow::Result<()> {
    if print && harness == "codex" {
        println!("notify = [\"silo\", \"report\", \"--codex-notify\"]");
        return Ok(());
    }
    if print && harness == "opencode" {
        print!("{OPENCODE}");
        return Ok(());
    }
    let (path, content) = prepare(home, harness)?;
    if print {
        print!("{content}");
        return Ok(());
    }
    if path.exists() && std::fs::read_to_string(&path)? == content {
        return Ok(());
    }
    std::fs::create_dir_all(path.parent().context("hook path has no parent")?)?;
    std::fs::write(&path, content)?;
    eprintln!("Installed Silo hooks in {}", path.display());
    Ok(())
}

fn read(path: &Path) -> anyhow::Result<String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(e.into()),
    }
}

fn prepare(home: &Path, harness: &str) -> anyhow::Result<(PathBuf, String)> {
    match harness {
        "claude" => {
            let path = home.join(".claude/settings.json");
            let text = read(&path)?;
            let mut settings = if text.trim().is_empty() { json!({}) } else { serde_json::from_str(&text)? };
            merge_claude(&mut settings)?;
            Ok((path, format!("{}\n", serde_json::to_string_pretty(&settings)?)))
        }
        "codex" => {
            let path = home.join(".codex/config.toml");
            let mut config: toml_edit::DocumentMut = read(&path)?.parse()?;
            if !config.contains_key("notify") {
                let mut args = toml_edit::Array::new();
                for arg in ["silo", "report", "--codex-notify"] {
                    args.push(arg);
                }
                config["notify"] = toml_edit::value(args);
            }
            Ok((path, config.to_string()))
        }
        "opencode" => {
            let path = home.join(".config/opencode/plugins/silo.js");
            let old = read(&path)?;
            if !old.is_empty() && old != OPENCODE {
                bail!("{} already exists; not overwriting it", path.display());
            }
            Ok((path, OPENCODE.into()))
        }
        _ => bail!("harness must be claude, codex or opencode"),
    }
}

fn merge_claude(settings: &mut Value) -> anyhow::Result<()> {
    let settings = settings.as_object_mut().context("Claude settings must be an object")?;
    let hooks = settings.entry("hooks").or_insert_with(|| json!({})).as_object_mut().context("hooks must be an object")?;
    for (event, state, permission) in
        [("UserPromptSubmit", "working", false), ("Stop", "done", false), ("Notification", "blocked", true), ("SessionEnd", "idle", false)]
    {
        let command = format!("silo report {state} --app claude{}", if permission { " --kind permission" } else { "" });
        let entries = hooks.entry(event).or_insert_with(|| json!([])).as_array_mut().context("hook event must be an array")?;
        if entries.iter().any(|e| e["hooks"].as_array().is_some_and(|h| h.iter().any(|h| h["type"] == "command" && h["command"] == command))) {
            continue;
        }
        let mut entry = json!({"hooks":[{"type":"command", "command":command}]});
        if permission {
            entry["matcher"] = json!("permission_prompt");
        }
        entries.push(entry);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn merge_preserves_settings_and_is_idempotent() {
        let mut config = json!({"model":"custom", "hooks":{"Stop":[{"hooks":[{"type":"command","command":"other"}]}]}});
        merge_claude(&mut config).unwrap();
        let once = config.clone();
        merge_claude(&mut config).unwrap();
        assert_eq!(config, once);
        assert_eq!(config["model"], "custom");
        assert_eq!(config["hooks"]["Stop"].as_array().unwrap().len(), 2);
    }
    #[test]
    fn installation_preserves_codex_notify() {
        let dir = std::env::temp_dir().join(format!("silo-hooks-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(".codex")).unwrap();
        std::fs::write(dir.join(".codex/config.toml"), "# mine\nnotify = [\"custom\"]\n").unwrap();
        install(&dir, "codex", false).unwrap();
        assert_eq!(read(&dir.join(".codex/config.toml")).unwrap(), "# mine\nnotify = [\"custom\"]\n");
        for harness in ["claude", "opencode"] {
            install(&dir, harness, false).unwrap();
            let once = prepare(&dir, harness).unwrap();
            install(&dir, harness, false).unwrap();
            assert_eq!(prepare(&dir, harness).unwrap(), once);
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
}
