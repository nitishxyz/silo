//! Editing MCP configuration files: add, change and remove servers, keeping every other
//! key, and the user's list of project servers turned off (kept in the user's
//! `mcp.json`, `disabledProjectServers: {root: [name]}`, so a shared `.mcp.json` stays untouched).
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};

const DISABLED: &str = "disabledProjectServers";

pub fn user_file(user_config_dir: &Path) -> PathBuf {
    user_config_dir.join("mcp.json")
}

/// The project file defining `name`: `.otto/mcp.json` when it does, else `.mcp.json`.
pub fn project_file(root: &Path, name: &str) -> PathBuf {
    let otto = root.join(".otto/mcp.json");
    if entry(&otto, name).is_some() { otto } else { root.join(".mcp.json") }
}

fn read(path: &Path) -> Result<Map<String, Value>, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<Value>(&text) {
            Ok(Value::Object(map)) => Ok(map),
            _ => Err(format!("{} is not a JSON object", path.file_name().unwrap_or_default().to_string_lossy())),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Map::new()),
        Err(e) => Err(e.to_string()),
    }
}

fn write(path: &Path, file: &Map<String, Value>) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let temporary = path.with_extension(format!("json.{}.tmp", std::process::id()));
    let mut text = serde_json::to_string_pretty(file).map_err(|e| e.to_string())?;
    text.push('\n');
    std::fs::write(&temporary, text).map_err(|e| e.to_string())?;
    std::fs::rename(&temporary, path).map_err(|e| e.to_string())
}

/// `mcpServers`, or `servers` in a file that only has that.
fn key(file: &Map<String, Value>) -> &'static str {
    if !file.contains_key("mcpServers") && file.contains_key("servers") { "servers" } else { "mcpServers" }
}

pub fn entry(path: &Path, name: &str) -> Option<Map<String, Value>> {
    let file = read(path).ok()?;
    file.get(key(&file))?.get(name)?.as_object().cloned()
}

/// Edit the server entries of `path` (created when missing).
fn edit<T>(path: &Path, f: impl FnOnce(&mut Map<String, Value>) -> T) -> Result<T, String> {
    let mut file = read(path)?;
    let key = key(&file);
    let servers = file.entry(key).or_insert_with(|| json!({}));
    if !servers.is_object() {
        *servers = json!({});
    }
    let result = f(servers.as_object_mut().expect("object"));
    write(path, &file)?;
    Ok(result)
}

pub fn put(path: &Path, name: &str, server: Map<String, Value>) -> Result<(), String> {
    edit(path, |servers| {
        servers.insert(name.to_owned(), Value::Object(server));
    })
}

/// Whether `path` defined `name`.
pub fn remove(path: &Path, name: &str) -> Result<bool, String> {
    if entry(path, name).is_none() {
        return Ok(false);
    }
    edit(path, |servers| servers.remove(name).is_some())
}

fn root_key(root: &Path) -> String {
    root.to_string_lossy().into_owned()
}

/// Project servers of `root` the user turned off.
pub fn project_disabled(user_config_dir: &Path, root: &Path) -> Vec<String> {
    let Ok(file) = read(&user_file(user_config_dir)) else { return vec![] };
    file.get(DISABLED)
        .and_then(|d| d.get(root_key(root)))
        .and_then(Value::as_array)
        .map(|names| names.iter().filter_map(Value::as_str).map(String::from).collect())
        .unwrap_or_default()
}

pub fn set_project_disabled(user_config_dir: &Path, root: &Path, name: &str, disabled: bool) -> Result<(), String> {
    let path = user_file(user_config_dir);
    let mut file = read(&path)?;
    let mut names = project_disabled(user_config_dir, root);
    names.retain(|n| n != name);
    if disabled {
        names.push(name.to_owned());
        names.sort();
    } else if !file.contains_key(DISABLED) {
        return Ok(());
    }
    let all = file.entry(DISABLED).or_insert_with(|| json!({}));
    if !all.is_object() {
        *all = json!({});
    }
    let all = all.as_object_mut().expect("object");
    if names.is_empty() {
        all.remove(&root_key(root));
    } else {
        all.insert(root_key(root), json!(names));
    }
    if all.is_empty() {
        file.remove(DISABLED);
    }
    write(&path, &file)
}
