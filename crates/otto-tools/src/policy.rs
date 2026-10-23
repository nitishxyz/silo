use crate::{ToolCx, ToolError, ToolSpec};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Component, Path, PathBuf};

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalMode {
    #[default]
    Auto,
    AskOnEffects,
    AskAll,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RuleDecision {
    Allow,
    Ask,
    Deny,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Rule {
    pub tool: String,
    pub pattern: Option<String>,
    pub decision: RuleDecision,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Policy {
    pub mode: ApprovalMode,
    pub rules: Vec<Rule>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Ask { summary: String },
    Deny { reason: String },
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Location {
    Project,
    Outside,
}

#[derive(Clone, Debug)]
pub struct Prepared {
    pub decision: Decision,
    pub input: Value,
    pub resolved: Vec<Resolved>,
}

#[derive(Serialize, Deserialize)]
struct PathGuard {
    original: String,
    approved: PathBuf,
    ancestor: PathBuf,
    dev: u64,
    ino: u64,
}

pub(crate) fn validate_prepared(input: &Value, cx: &ToolCx) -> Result<(), ToolError> {
    use std::os::unix::fs::MetadataExt;
    if let Some(guards) = input.get("__otto_paths") {
        let guards: Vec<PathGuard> = serde_json::from_value(guards.clone()).map_err(|e| ToolError::invalid(e.to_string()))?;
        for guard in guards {
            let changed = || ToolError::failed(format!("Approved path changed before execution: {}", guard.original));
            let original = resolve_path(&cx.cwd, &cx.project_root, &guard.original).map_err(|_| changed())?;
            let current = resolve_path(&cx.cwd, &cx.project_root, &guard.approved.to_string_lossy()).map_err(|_| changed())?;
            let metadata = std::fs::metadata(&guard.ancestor).map_err(|_| changed())?;
            if original.path != guard.approved || current.path != guard.approved || metadata.dev() != guard.dev || metadata.ino() != guard.ino {
                return Err(changed());
            }
        }
    }
    Ok(())
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolved {
    pub path: PathBuf,
    pub location: Location,
}

fn canonical_prefix(path: &Path) -> Result<PathBuf, ToolError> {
    let mut resolved = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                resolved.pop();
            }
            other => {
                resolved.push(other.as_os_str());
                match std::fs::symlink_metadata(&resolved) {
                    Ok(_) => resolved = std::fs::canonicalize(&resolved).map_err(|e| ToolError::failed(e.to_string()))?,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(ToolError::failed(e.to_string())),
                }
            }
        }
    }
    Ok(resolved)
}

pub fn resolve_path(cwd: &Path, project_root: &Path, raw: &str) -> Result<Resolved, ToolError> {
    let expanded = if raw == "~" || raw.starts_with("~/") {
        PathBuf::from(std::env::var_os("HOME").ok_or_else(|| ToolError::invalid("HOME is not set"))?).join(raw.strip_prefix("~/").unwrap_or(""))
    } else {
        PathBuf::from(raw)
    };
    let cwd = if cwd.is_absolute() { cwd.to_path_buf() } else { std::env::current_dir().map_err(|e| ToolError::failed(e.to_string()))?.join(cwd) };
    let path = canonical_prefix(&if expanded.is_absolute() { expanded } else { cwd.join(expanded) })?;
    let root = canonical_prefix(&if project_root.is_absolute() { project_root.to_path_buf() } else { cwd.join(project_root) })?;
    let location = if path.starts_with(root) { Location::Project } else { Location::Outside };
    Ok(Resolved { path, location })
}
fn matches(pattern: &str, text: &str) -> bool {
    globset::Glob::new(pattern).is_ok_and(|g| g.compile_matcher().is_match(text))
}
impl Policy {
    /// Prepare once before approval; execute with the returned input, not the original.
    pub fn prepare(&self, spec: &ToolSpec, input: &Value, cx: &ToolCx) -> Result<Prepared, ToolError> {
        use std::os::unix::fs::MetadataExt;
        let mut input = input.clone();
        let mut guards = Vec::new();
        let mut resolved = Vec::new();
        let mut resolve = |raw: &str| -> Result<String, ToolError> {
            let r = resolve_path(&cx.cwd, &cx.project_root, raw)?;
            let mut ancestor = r.path.parent().unwrap_or(&r.path).to_path_buf();
            while !ancestor.exists() {
                if !ancestor.pop() {
                    return Err(ToolError::invalid("No existing path ancestor"));
                }
            }
            let metadata = std::fs::metadata(&ancestor).map_err(|e| ToolError::failed(e.to_string()))?;
            guards.push(PathGuard { original: raw.into(), approved: r.path.clone(), ancestor, dev: metadata.dev(), ino: metadata.ino() });
            let path = r.path.to_str().ok_or_else(|| ToolError::invalid("Path is not UTF-8"))?.to_owned();
            resolved.push(r);
            Ok(path)
        };
        if spec.name == "apply_patch" {
            let patch = input
                .as_str()
                .or_else(|| input.get("input").or_else(|| input.get("patch")).and_then(Value::as_str))
                .ok_or_else(|| ToolError::invalid("Expected patch text"))?;
            let mut lines = Vec::new();
            for line in patch.lines() {
                let mut rewritten = line.to_owned();
                for prefix in ["*** Add File: ", "*** Update File: ", "*** Delete File: ", "*** Move to: "] {
                    if let Some(raw) = line.strip_prefix(prefix) {
                        rewritten = format!("{prefix}{}", resolve(raw)?);
                        break;
                    }
                }
                lines.push(rewritten);
            }
            input = serde_json::json!({"input": lines.join("\n")});
        } else if matches!(spec.name.as_str(), "read" | "write" | "copy_attachment" | "edit" | "multiedit" | "ls" | "tree" | "glob" | "search" | "shell")
            // An attachment read has no path.
            && !(spec.name == "read" && input.get("path").is_none() && input.get("attachment_id").is_some())
        {
            let key = if spec.name == "shell" { "cwd" } else { "path" };
            let raw = input.get(key).and_then(Value::as_str).unwrap_or(".");
            let path = resolve(raw)?;
            let object = input.as_object_mut().ok_or_else(|| ToolError::invalid("Expected object input"))?;
            object.insert(key.into(), Value::String(path));
        }
        let decision = self.decide(spec, &input, cx);
        if let Some(object) = input.as_object_mut() {
            object.remove("__otto_paths");
            if !guards.is_empty() {
                object.insert("__otto_paths".into(), serde_json::to_value(guards).map_err(|e| ToolError::failed(e.to_string()))?);
            }
        }
        validate_prepared(&input, cx)?;
        Ok(Prepared { decision, input, resolved })
    }
    pub fn decide(&self, spec: &ToolSpec, input: &Value, cx: &ToolCx) -> Decision {
        let mut paths = Vec::new();
        if spec.name == "apply_patch" {
            let patch = input.as_str().or_else(|| input.get("input").or_else(|| input.get("patch")).and_then(Value::as_str)).unwrap_or("");
            paths.extend(patch.lines().filter_map(|line| {
                ["*** Add File: ", "*** Update File: ", "*** Delete File: ", "*** Move to: "].iter().find_map(|p| line.strip_prefix(p))
            }));
        } else if let Some(raw) = input.get("path").and_then(Value::as_str) {
            paths.push(raw);
        } else if matches!(spec.name.as_str(), "ls" | "tree" | "glob" | "search") {
            paths.push(".");
        }
        let mut subjects = Vec::new();
        for raw in paths {
            match resolve_path(&cx.cwd, &cx.project_root, raw) {
                Ok(resolved) => subjects.push((resolved.path.to_string_lossy().into_owned(), resolved.location == Location::Outside)),
                Err(e) => return Decision::Deny { reason: e.to_string() },
            }
        }
        if subjects.is_empty() {
            subjects.push((input.get("command").or_else(|| input.get("url")).and_then(Value::as_str).unwrap_or("").to_owned(), false));
        }
        let mut needs_approval = false;
        for (subject, outside) in &subjects {
            let rule = self.rules.iter().find(|r| matches(&r.tool, &spec.name) && r.pattern.as_ref().is_none_or(|p| matches(p, subject)));
            match rule.map(|r| r.decision) {
                Some(RuleDecision::Deny) => return Decision::Deny { reason: format!("Denied by rule for {}: {subject}", spec.name) },
                Some(RuleDecision::Ask) => needs_approval = true,
                Some(RuleDecision::Allow) => {}
                None => needs_approval |= self.mode == ApprovalMode::AskAll || (self.mode == ApprovalMode::AskOnEffects && !spec.effects.read_only()),
            }
            needs_approval |= *outside && spec.effects.write && self.mode != ApprovalMode::Auto;
        }
        if needs_approval {
            Decision::Ask { summary: format!("{}: {}", spec.name, subjects.iter().map(|(s, _)| s.as_str()).collect::<Vec<_>>().join(", ")) }
        } else {
            Decision::Allow
        }
    }
    pub fn load(project: &Path, user: &Path, mode: ApprovalMode) -> Result<Self, ToolError> {
        let mut rules = Vec::new();
        for path in [project, user] {
            match std::fs::read(path) {
                Ok(bytes) => rules.extend(serde_json::from_slice::<Vec<Rule>>(&bytes).map_err(|e| ToolError::invalid(e.to_string()))?),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(ToolError::failed(e.to_string())),
            }
        }
        Ok(Self { mode, rules })
    }
    pub fn save(&self, path: &Path) -> Result<(), ToolError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| ToolError::failed(e.to_string()))?;
        }
        std::fs::write(path, serde_json::to_vec_pretty(&self.rules).map_err(|e| ToolError::failed(e.to_string()))?)
            .map_err(|e| ToolError::failed(e.to_string()))
    }
}
pub fn redact(input: &Value, sensitive: &[String]) -> Value {
    let mut value = input.clone();
    for pointer in sensitive {
        if let Some(target) = value.pointer_mut(pointer) {
            *target = Value::String("[redacted]".into());
        }
    }
    value
}
