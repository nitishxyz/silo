//! Conflicts: a conflicted file's `<<<<<<<` … `>>>>>>>` regions, both
//! sides of each (and the base with `diff3`), and their resolution: some
//! regions rewritten as one side, a whole side of the file taken, or the
//! file marked resolved (`git add`).

use std::path::Path;

use sha2::{Digest, Sha256};
use silo_proto::{
    changes::{ConflictFile, ConflictRegion, Take},
    control::RpcError,
};

use super::git::Git;

/// A region and the lines it spans in the file (`start..end`, markers
/// included).
pub struct Span {
    pub region: ConflictRegion,
    pub start: usize,
    pub end: usize,
}

fn marker<'a>(line: &'a str, mark: &str) -> Option<&'a str> {
    let line = line.strip_suffix('\n').unwrap_or(line);
    let line = line.strip_suffix('\r').unwrap_or(line);
    let rest = line.strip_prefix(mark)?;
    // `<<<<<<<` alone or with a label; not a longer run of the character.
    (rest.is_empty() || rest.starts_with(' ')).then(|| rest.trim())
}

fn text(line: &str) -> String {
    line.trim_end_matches('\n').trim_end_matches('\r').to_owned()
}

/// The regions of `lines` (each with its line break).
#[must_use]
pub fn regions(lines: &[&str]) -> Vec<Span> {
    let mut out: Vec<Span> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let Some(ours_label) = marker(lines[i], "<<<<<<<") else {
            i += 1;
            continue;
        };
        let start = i;
        let (mut ours, mut base, mut theirs) = (Vec::new(), None::<Vec<String>>, Vec::new());
        let mut part = 0; // 0 ours, 1 base, 2 theirs
        let mut end = None;
        let mut j = i + 1;
        while j < lines.len() {
            let line = lines[j];
            if part == 0 && marker(line, "|||||||").is_some() {
                (part, base) = (1, Some(Vec::new()));
            } else if part < 2 && marker(line, "=======").is_some_and(str::is_empty) {
                part = 2;
            } else if part == 2
                && let Some(label) = marker(line, ">>>>>>>")
            {
                end = Some((j, label.to_owned()));
                break;
            } else if part == 0 && marker(line, "<<<<<<<").is_some() {
                break;
            } else {
                match part {
                    0 => ours.push(text(line)),
                    1 => base.get_or_insert_with(Vec::new).push(text(line)),
                    _ => theirs.push(text(line)),
                }
            }
            j += 1;
        }
        let Some((last, theirs_label)) = end else {
            i = j.max(i + 1);
            continue;
        };
        let mut hash = Sha256::new();
        for part in [&ours, base.as_ref().unwrap_or(&Vec::new()), &theirs] {
            for line in part {
                hash.update(line.as_bytes());
                hash.update(b"\n");
            }
            hash.update(b"\0");
        }
        let digest: String = hash.finalize().iter().take(8).map(|b| format!("{b:02x}")).collect();
        // The same conflict twice: the second is `<hash>-2`.
        let seen = out.iter().filter(|s| s.region.id.split('-').next() == Some(digest.as_str())).count();
        let id = if seen == 0 { digest } else { format!("{digest}-{}", seen + 1) };
        let line = u32::try_from(start + 1).unwrap_or(u32::MAX);
        let region = ConflictRegion { id, line, ours, base, theirs, ours_label: ours_label.to_owned(), theirs_label };
        out.push(Span { region, start, end: last + 1 });
        i = last + 1;
    }
    out
}

/// What the file reads as: text split in lines, or `None` (binary, gone).
fn read(root: &Path, rel: &str) -> Option<String> {
    let bytes = std::fs::read(root.join(rel)).ok()?;
    if bytes[..bytes.len().min(8000)].contains(&0) {
        return None;
    }
    String::from_utf8(bytes).ok()
}

/// `changes.conflict`.
#[must_use]
pub fn conflict(root: &Path, rel: &str) -> ConflictFile {
    match read(root, rel) {
        Some(text) => {
            let lines: Vec<&str> = text.split_inclusive('\n').collect();
            ConflictFile { path: rel.to_owned(), regions: regions(&lines).into_iter().map(|s| s.region).collect(), whole: false }
        }
        None => ConflictFile { path: rel.to_owned(), regions: Vec::new(), whole: true },
    }
}

/// `lines` with the regions `ids` names written as `take`'s side, keeping
/// each line's break.
///
/// # Errors
/// `conflict` for an id the file doesn't have (any more).
pub fn rewrite(text: &str, ids: &[String], take: Take) -> Result<String, RpcError> {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let spans = regions(&lines);
    if let Some(missing) = ids.iter().find(|id| !spans.iter().any(|s| &s.region.id == *id)) {
        return Err(RpcError::new(RpcError::CONFLICT, "The file changed; review it again").with_data(serde_json::json!({ "region": missing })));
    }
    let eol = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for span in spans.iter().filter(|s| ids.contains(&s.region.id)) {
        out.extend(lines[at..span.start].iter().copied());
        let r = &span.region;
        let picked: Vec<&String> = match take {
            Take::Ours => r.ours.iter().collect(),
            Take::Theirs => r.theirs.iter().collect(),
            Take::Both => r.ours.iter().chain(&r.theirs).collect(),
            Take::Unknown => return Err(RpcError::bad_request("take must be ours, theirs or both")),
        };
        // The region's last line had no break (the file's end): nor does its side.
        let open_end = !lines[span.end - 1].ends_with('\n');
        for (i, line) in picked.iter().enumerate() {
            out.push_str(line);
            if !(open_end && i + 1 == picked.len()) {
                out.push_str(eol);
            }
        }
        at = span.end;
    }
    out.extend(lines[at..].iter().copied());
    Ok(out)
}

/// Which sides the index holds for `rel`: (ours, theirs).
fn stages(root: &Path, rel: &str) -> Result<(bool, bool), RpcError> {
    let out = Git::read(root, ["ls-files", "-u", "-z", "--", rel]).run()?;
    let fields = super::git::fields(&out.stdout);
    let has = |stage: &str| fields.iter().any(|f| f.split('\t').next().and_then(|m| m.split(' ').nth(2)) == Some(stage));
    Ok((has("2"), has("3")))
}

/// `changes.resolve`.
///
/// # Errors
/// `bad_request` for a file that isn't conflicted (a whole side), regions
/// left when marking it resolved, or `both` of a binary file; `conflict`
/// for regions gone; `git` when git refuses.
pub fn resolve(root: &Path, rel: &str, take: Option<Take>, ids: Option<&[String]>) -> Result<(), RpcError> {
    match (take, ids) {
        (Some(take), Some(ids)) => {
            if ids.is_empty() {
                return Err(RpcError::bad_request("no regions named"));
            }
            let text = read(root, rel).ok_or_else(|| RpcError::bad_request("Only a whole side of this file can be taken"))?;
            let next = rewrite(&text, ids, take)?;
            std::fs::write(root.join(rel), next).map_err(|e| RpcError::internal(format!("{rel}: {e}")))
        }
        (Some(take), None) => {
            let (ours, theirs) = stages(root, rel)?;
            if !ours && !theirs {
                return Err(RpcError::bad_request(format!("{rel} isn't conflicted")));
            }
            let side = match take {
                Take::Ours => ours.then_some("--ours"),
                Take::Theirs => theirs.then_some("--theirs"),
                Take::Both => {
                    let text = read(root, rel).ok_or_else(|| RpcError::bad_request("Both sides of a binary file can't be kept"))?;
                    let ids: Vec<String> = conflict(root, rel).regions.into_iter().map(|r| r.id).collect();
                    let next = rewrite(&text, &ids, Take::Both)?;
                    std::fs::write(root.join(rel), next).map_err(|e| RpcError::internal(format!("{rel}: {e}")))?;
                    Git::write(root, ["add", "--", rel]).run()?;
                    return Ok(());
                }
                Take::Unknown => return Err(RpcError::bad_request("take must be ours, theirs or both")),
            };
            match side {
                Some(side) => {
                    Git::write(root, ["checkout", side, "--", rel]).run()?;
                    Git::write(root, ["add", "--", rel]).run()?;
                }
                // That side deleted it.
                None => {
                    Git::write(root, ["rm", "-q", "-f", "--", rel]).run()?;
                }
            }
            Ok(())
        }
        (None, Some(_)) => Err(RpcError::bad_request("name a side to take")),
        (None, None) => {
            let left = conflict(root, rel).regions.len();
            if left > 0 {
                let what = if left == 1 { "1 conflict is".to_owned() } else { format!("{left} conflicts are") };
                return Err(RpcError::bad_request(format!("{what} still in the file: take a side first")));
            }
            Git::write(root, ["add", "-A", "--", rel]).run()?;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regions_parse_with_a_base_and_rewrite_keeping_line_breaks() {
        let text = "a\r\n<<<<<<< HEAD\r\nours\r\n||||||| base\r\nold\r\n=======\r\ntheirs\r\n>>>>>>> feature\r\nz\r\n<<<<<<< HEAD\nx\n=======\ny\n>>>>>>> feature";
        let lines: Vec<&str> = text.split_inclusive('\n').collect();
        let spans = regions(&lines);
        assert_eq!(spans.len(), 2);
        let r = &spans[0].region;
        assert_eq!((r.line, r.ours_label.as_str(), r.theirs_label.as_str()), (2, "HEAD", "feature"));
        assert_eq!((r.ours.clone(), r.base.clone(), r.theirs.clone()), (vec!["ours".into()], Some(vec!["old".into()]), vec!["theirs".into()]));
        let ids = vec![spans[0].region.id.clone(), spans[1].region.id.clone()];
        assert_eq!(rewrite(text, &ids, Take::Theirs).unwrap(), "a\r\ntheirs\r\nz\r\ny");
        assert_eq!(rewrite(text, &ids[..1], Take::Both).unwrap().lines().take(3).collect::<Vec<_>>(), ["a", "ours", "theirs"]);
        assert_eq!(rewrite(text, &["nope".into()], Take::Ours).unwrap_err().code, RpcError::CONFLICT);
    }

    #[test]
    fn the_same_conflict_twice_has_two_ids_and_unclosed_markers_are_none() {
        let one = "<<<<<<< a\nx\n=======\ny\n>>>>>>> b\n";
        let text = format!("{one}mid\n{one}<<<<<<< a\nnever closed\n");
        let lines: Vec<&str> = text.split_inclusive('\n').collect();
        let ids: Vec<String> = regions(&lines).into_iter().map(|s| s.region.id).collect();
        assert_eq!(ids.len(), 2);
        assert_eq!(ids[1], format!("{}-2", ids[0]));
        let longer: Vec<&str> = "<<<<<<<< not a marker\n".split_inclusive('\n').collect();
        assert!(regions(&longer).is_empty());
    }
}
