use super::*;
use crate::{Presentation, ToolOutput};
use std::{collections::BTreeMap, path::PathBuf};
fn patch_spec() -> ToolSpec {
    let mut s = spec(
        "apply_patch",
        "Apply a Codex patch (as input or patch): *** Begin Patch, Add/Update/Delete File sections, and *** End Patch. Update hunks use @@, space-prefixed context, - removals and + additions; optional *** Move to: renames. All hunks are validated before files change. Read files first and provide distinctive context.",
        json!({"input":{"type":"string"},"patch":{"type":"string"}}),
        &[],
        Effects::WRITE,
    );
    // GPT models write Codex patches as freeform text.
    s.presentations.push(Presentation {
        family: "openai".into(),
        name: "apply_patch".into(),
        description: None,
        input_schema: None,
        freeform: Some(json!({"type":"text"})),
    });
    s
}
tool!(ApplyPatch, patch_spec(), run);
#[derive(Debug)]
struct Hunk {
    anchor: Option<String>,
    old: Vec<String>,
    new: Vec<String>,
    eof: bool,
}
#[derive(Debug)]
enum Operation {
    Add(String, String),
    Delete(String),
    Update(String, Option<String>, Vec<Hunk>),
}
fn parse(patch: &str) -> Result<Vec<Operation>, ToolError> {
    let lines: Vec<_> = patch.lines().collect();
    if lines.first() != Some(&"*** Begin Patch") || lines.last() != Some(&"*** End Patch") {
        return Err(ToolError::invalid("Expected *** Begin Patch and *** End Patch"));
    }
    let mut ops = Vec::new();
    let mut i = 1;
    while i + 1 < lines.len() {
        let header = lines[i];
        i += 1;
        if let Some(path) = header.strip_prefix("*** Add File: ") {
            let mut text = String::new();
            while i + 1 < lines.len() && !lines[i].starts_with("*** ") {
                let line = lines[i].strip_prefix('+').ok_or_else(|| ToolError::invalid("Add File lines must start with +"))?;
                text.push_str(line);
                text.push('\n');
                i += 1;
            }
            ops.push(Operation::Add(path.into(), text));
        } else if let Some(path) = header.strip_prefix("*** Delete File: ") {
            ops.push(Operation::Delete(path.into()));
        } else if let Some(path) = header.strip_prefix("*** Update File: ") {
            let destination = lines.get(i).and_then(|l| l.strip_prefix("*** Move to: ")).map(str::to_owned);
            if destination.is_some() {
                i += 1;
            }
            let mut hunks = Vec::new();
            while i + 1 < lines.len() && !lines[i].starts_with("*** ") {
                let anchor = if lines[i] == "@@" {
                    i += 1;
                    None
                } else if let Some(anchor) = lines[i].strip_prefix("@@ ") {
                    i += 1;
                    Some(anchor.to_owned())
                } else if hunks.is_empty() && lines[i].starts_with([' ', '+', '-']) {
                    None
                } else {
                    return Err(ToolError::invalid("Expected @@ hunk header"));
                };
                let mut h = Hunk { anchor, old: vec![], new: vec![], eof: false };
                while i + 1 < lines.len() && !lines[i].starts_with("@@") && !lines[i].starts_with("*** ") {
                    let line = lines[i];
                    i += 1;
                    match line.as_bytes().first() {
                        Some(b' ') => {
                            h.old.push(line[1..].into());
                            h.new.push(line[1..].into());
                        }
                        Some(b'-') => h.old.push(line[1..].into()),
                        Some(b'+') => h.new.push(line[1..].into()),
                        _ => return Err(ToolError::invalid("Hunk lines must start with space, +, or -")),
                    }
                }
                if lines.get(i) == Some(&"*** End of File") {
                    h.eof = true;
                    i += 1;
                }
                if h.old.is_empty() && h.new.is_empty() {
                    return Err(ToolError::invalid("Empty hunk"));
                }
                hunks.push(h);
            }
            if hunks.is_empty() {
                return Err(ToolError::invalid("Update File requires a hunk"));
            }
            ops.push(Operation::Update(path.into(), destination, hunks));
        } else {
            return Err(ToolError::invalid(format!("Unknown patch header: {header}")));
        }
    }
    if ops.is_empty() {
        return Err(ToolError::invalid("Patch has no file operations"));
    }
    Ok(ops)
}
fn locate(lines: &[String], needle: &[String], start: usize, eof: bool) -> Result<usize, ToolError> {
    if needle.is_empty() {
        return Ok(lines.len());
    }
    for mode in 0..3 {
        let normalize = |s: &str| match mode {
            0 => s.to_owned(),
            1 => s.trim_end().to_owned(),
            _ => s.trim().to_owned(),
        };
        let positions: Vec<_> = (start..=lines.len().saturating_sub(needle.len()))
            .filter(|&p| {
                p + needle.len() <= lines.len()
                    && (!eof || p + needle.len() == lines.len())
                    && lines[p..p + needle.len()].iter().zip(needle).all(|(a, b)| normalize(a) == normalize(b))
            })
            .collect();
        if let Some(&p) = positions.first() {
            return Ok(p);
        }
    }
    Err(ToolError::invalid(format!("Hunk context not found: {}", needle.join("\n"))))
}
fn update(text: &str, hunks: &[Hunk]) -> Result<String, ToolError> {
    let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
    let mut cursor = 0;
    for h in hunks {
        if let Some(anchor) = &h.anchor {
            cursor = locate(&lines, std::slice::from_ref(anchor), cursor, false)? + 1;
        }
        let at = locate(&lines, &h.old, cursor, h.eof)?;
        lines.splice(at..at + h.old.len(), h.new.clone());
        cursor = at + h.new.len();
    }
    let mut result = lines.join("\n");
    if !lines.is_empty() {
        result.push('\n');
    }
    Ok(result)
}
async fn run(input: Value, cx: ToolCx) -> Result<ToolOutput, ToolError> {
    let patch = input
        .as_str()
        .or_else(|| input.get("input").or_else(|| input.get("patch")).and_then(Value::as_str))
        .ok_or_else(|| ToolError::invalid("Expected patch string or {input/patch: string}"))?;
    let ops = parse(patch)?;
    let mut changes: BTreeMap<PathBuf, Option<String>> = BTreeMap::new();
    let mut moved_permissions = BTreeMap::new();
    let resolve = |s: &str| {
        if s.trim().is_empty() {
            return Err(ToolError::invalid("Empty patch path"));
        }
        Ok(crate::policy::resolve_path(&cx.cwd, &cx.project_root, s)?.path)
    };
    let mut insert = |p: PathBuf, text: Option<String>| -> Result<(), ToolError> {
        if changes.insert(p.clone(), text).is_some() {
            return Err(ToolError::invalid(format!("Duplicate patch path: {}", p.display())));
        }
        Ok(())
    };
    for op in ops {
        match op {
            Operation::Add(p, text) => {
                let p = resolve(&p)?;
                if p.exists() {
                    return Err(ToolError::invalid(format!("Add target already exists: {}", p.display())));
                }
                insert(p, Some(text))?;
            }
            Operation::Delete(p) => {
                let p = resolve(&p)?;
                regular_text(&p).map_err(io)?;
                insert(p, None)?;
            }
            Operation::Update(p, dest, hunks) => {
                let p = resolve(&p)?;
                let old = regular_text(&p).map_err(io)?;
                let new = update(&old, &hunks)?;
                if let Some(dest) = dest {
                    let dest = resolve(&dest)?;
                    if dest != p {
                        if dest.exists() {
                            return Err(ToolError::invalid("Move destination already exists"));
                        }
                        moved_permissions.insert(dest.clone(), std::fs::metadata(&p).map_err(io)?.permissions());
                        insert(p, None)?;
                        insert(dest, Some(new))?;
                    } else {
                        insert(p, Some(new))?;
                    }
                } else {
                    insert(p, Some(new))?;
                }
            }
        }
    }
    let mut originals = BTreeMap::new();
    let mut permissions = BTreeMap::new();
    let mut metadata = Vec::new();
    for (p, new) in &changes {
        let old = match regular_text(p) {
            Ok(s) => Some(s),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(io(e)),
        };
        metadata.push(diff(p, old.as_deref().unwrap_or(""), new.as_deref().unwrap_or("")));
        if old.is_some() {
            permissions.insert(p.clone(), std::fs::metadata(p).map_err(io)?.permissions());
        }
        originals.insert(p.clone(), old);
    }
    let mut staged = BTreeMap::new();
    for (p, new) in &changes {
        if let Some(text) = new {
            let file = stage(p, text).map_err(io)?;
            if let Some(permissions) = moved_permissions.get(p) {
                file.as_file().set_permissions(permissions.clone()).map_err(io)?;
            }
            staged.insert(p.clone(), file);
        }
    }
    let mut applied = Vec::new();
    for (p, new) in &changes {
        applied.push(p.clone());
        let result = match new {
            Some(_) => staged.remove(p).expect("staged write").persist(p).map(|_| ()).map_err(|e| e.error),
            None => std::fs::remove_file(p),
        };
        if let Err(e) = result {
            let mut failures = Vec::new();
            for p in applied.into_iter().rev() {
                let r = match &originals[&p] {
                    Some(old) => atomic_write(&p, old).and_then(|()| std::fs::set_permissions(&p, permissions[&p].clone())),
                    None => {
                        if p.exists() {
                            std::fs::remove_file(&p)
                        } else {
                            Ok(())
                        }
                    }
                };
                if let Err(e) = r {
                    failures.push(format!("{}: {e}", p.display()));
                }
            }
            return Err(ToolError::failed(format!("Patch rolled back after {e}; rollback errors: {failures:?}")));
        }
    }
    Ok(ToolOutput::text(format!("Applied patch to {} paths", changes.len())).with_metadata(json!({"files":metadata})))
}
