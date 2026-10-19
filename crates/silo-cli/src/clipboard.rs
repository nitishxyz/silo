//! `silo clipboard`: the clipboard of the Silo client showing this block,
//! for programs in it. The shims in `<SILO_HOME>/shims` (`pbpaste`,
//! `pbcopy`, `xclip`, `xsel`, `wl-paste`, `wl-copy`) run
//! `silo clipboard shim <tool> <args>`; [`translate`] maps each tool's
//! flags. When no client answers, the real tool later on `PATH` runs
//! instead.

use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, bail};
use silo_client::Control;
use silo_proto::{
    control::{Method, Pasted},
    data::{Clip, ClipKind, ClipboardLocation},
    paths,
};

/// Set in spawned shells to their block's id.
pub const BLOCK_ENV: &str = "SILO_BLOCK";
/// The server waits 3 s for a client; allow for the round trip.
const CALL_TIMEOUT: Duration = Duration::from_secs(6);

/// What to paste.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Paste {
    Text,
    /// An image of this MIME type.
    Image(String),
    /// The offered MIME types, one per line.
    Types,
}

/// What a clipboard tool invocation asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    Paste {
        what: Paste,
        /// Append a newline to text lacking one (`wl-paste` without `-n`).
        newline: bool,
        /// Start a type list with `TARGETS` (`xclip`).
        targets: bool,
    },
    Copy {
        /// Text from the arguments (`wl-copy text`); else stdin.
        text: Option<String>,
        trim_newline: bool,
    },
    /// Not translatable: run the real tool.
    Fallback,
}

/// A target or `--type` value as a paste request; `None` if unsupported.
#[must_use]
pub fn parse_target(target: &str) -> Option<Paste> {
    let lower = target.to_ascii_lowercase();
    match lower.as_str() {
        "targets" | "types" => Some(Paste::Types),
        "text" | "string" | "utf8_string" | "compound_text" => Some(Paste::Text),
        t if t.starts_with("text/plain") => Some(Paste::Text),
        t if t.starts_with("image/") && t.len() > 6 => Some(Paste::Image(lower)),
        _ => None,
    }
}

/// Split combined short flags (`-bo` -> `-b -o`), keeping long options,
/// `--` and lone `-` as they are.
fn expand(args: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = false;
    for arg in args {
        if !rest && arg.len() > 2 && arg.starts_with('-') && !arg.starts_with("--") {
            out.extend(arg[1..].chars().map(|c| format!("-{c}")));
        } else {
            rest |= arg == "--";
            out.push(arg.clone());
        }
    }
    out
}

/// Map `tool args` to an [`Action`]; `stdin_tty` decides `xsel`'s default
/// direction.
#[must_use]
pub fn translate(tool: &str, args: &[String], stdin_tty: bool) -> Action {
    match tool {
        "pbpaste" | "pbcopy" => pb(tool == "pbcopy", args),
        "xclip" => xclip(args),
        "xsel" => xsel(&expand(args), stdin_tty),
        "wl-paste" => wl_paste(&expand(args)),
        "wl-copy" => wl_copy(&expand(args)),
        _ => Action::Fallback,
    }
}

fn text_paste() -> Action {
    Action::Paste { what: Paste::Text, newline: false, targets: false }
}

fn pb(copy: bool, args: &[String]) -> Action {
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-Prefer" => drop(args.next()),
            "-pboard" if args.next().is_some_and(|b| b == "general") => {}
            _ => return Action::Fallback,
        }
    }
    if copy { Action::Copy { text: None, trim_newline: false } } else { text_paste() }
}

/// Silo has a single clipboard: primary-selection flags address it too.
fn xclip(args: &[String]) -> Action {
    // xclip takes unique prefixes of single-dash long options.
    let is = |name: &str, full: &str, min: usize| name.len() >= min && full.starts_with(name);
    let (mut output, mut target, mut trim) = (false, None, false);
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let Some(name) = arg.strip_prefix('-').map(|a| a.trim_start_matches('-')) else { return Action::Fallback };
        if is(name, "selection", 2) {
            if !matches!(args.next().and_then(|s| s.chars().next()), Some('c' | 'p')) {
                return Action::Fallback;
            }
        } else if is(name, "out", 1) {
            output = true;
        } else if is(name, "in", 1) {
            output = false;
        } else if is(name, "target", 1) {
            target = args.next().cloned();
        } else if is(name, "rmlastnl", 1) {
            trim = true;
        } else if is(name, "display", 1) || is(name, "loops", 1) || is(name, "wait", 2) || name == "alt-text" {
            args.next();
        } else if !(is(name, "quiet", 1) || is(name, "silent", 2) || is(name, "verbose", 1) || is(name, "noutf8", 1) || name == "sensitive") {
            return Action::Fallback;
        }
    }
    if !output {
        return match target.as_deref().map(parse_target) {
            None | Some(Some(Paste::Text)) => Action::Copy { text: None, trim_newline: trim },
            _ => Action::Fallback,
        };
    }
    match target.as_deref().map_or(Some(Paste::Text), parse_target) {
        Some(what) => Action::Paste { what, newline: false, targets: true },
        None => Action::Fallback,
    }
}

fn xsel(args: &[String], stdin_tty: bool) -> Action {
    let (mut output, mut input) = (false, false);
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-b" | "--clipboard" | "-p" | "--primary" => {}
            "-o" | "--output" => output = true,
            "-i" | "--input" => input = true,
            "-n" | "--nodetach" | "-v" | "--verbose" | "--trim" => {}
            "-t" | "--selectionTimeout" | "-l" | "--logfile" => drop(args.next()),
            _ => return Action::Fallback,
        }
    }
    match (input, output) {
        (true, true) => Action::Fallback,
        (false, true) => text_paste(),
        (true, false) => Action::Copy { text: None, trim_newline: false },
        (false, false) if stdin_tty => text_paste(),
        (false, false) => Action::Copy { text: None, trim_newline: false },
    }
}

/// `--name=value` or `--name value` / `-x value`.
fn value<'a>(arg: &'a str, short: &str, long: &str, rest: &mut impl Iterator<Item = &'a String>) -> Option<Option<&'a str>> {
    if arg == short || arg == long {
        return Some(rest.next().map(String::as_str));
    }
    arg.strip_prefix(long).and_then(|v| v.strip_prefix('=')).map(Some)
}

fn wl_paste(args: &[String]) -> Action {
    let (mut what, mut newline) = (Paste::Text, true);
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if let Some(target) = value(arg, "-t", "--type", &mut args) {
            match target.and_then(parse_target) {
                Some(Paste::Types) | None => return Action::Fallback,
                Some(t) => what = t,
            }
            continue;
        }
        if value(arg, "-s", "--seat", &mut args).is_some() {
            continue;
        }
        match arg.as_str() {
            "-l" | "--list-types" => what = Paste::Types,
            "-n" | "--no-newline" => newline = false,
            "-p" | "--primary" => {}
            _ => return Action::Fallback,
        }
    }
    Action::Paste { what, newline, targets: false }
}

fn wl_copy(args: &[String]) -> Action {
    let (mut trim, mut words) = (false, Vec::new());
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if let Some(target) = value(arg, "-t", "--type", &mut args) {
            if target.and_then(parse_target) != Some(Paste::Text) {
                return Action::Fallback;
            }
            continue;
        }
        if value(arg, "-s", "--seat", &mut args).is_some() {
            continue;
        }
        match arg.as_str() {
            "-n" | "--trim-newline" => trim = true,
            "-p" | "--primary" | "-o" | "--paste-once" | "-f" | "--foreground" => {}
            "--" => words.extend(args.by_ref().cloned()),
            a if a.starts_with('-') && a.len() > 1 => return Action::Fallback,
            a => words.push(a.to_owned()),
        }
    }
    let text = (!words.is_empty()).then(|| words.join(" "));
    Action::Copy { text, trim_newline: trim }
}

/// The bytes a paste prints, if `content` answers `what`.
#[must_use]
pub fn render(what: &Paste, content: Option<Clip>, newline: bool, targets: bool) -> Option<Vec<u8>> {
    match (what, content?) {
        (Paste::Text, Clip::Text(mut text)) => {
            if newline && !text.ends_with('\n') {
                text.push('\n');
            }
            Some(text.into_bytes())
        }
        (Paste::Image(want), Clip::Image { mime, data }) if *want == mime => Some(data),
        (Paste::Types, Clip::Types(types)) => {
            let lead = targets.then_some("TARGETS".to_owned());
            Some(lead.into_iter().chain(types).map(|t| t + "\n").collect::<String>().into_bytes())
        }
        _ => None,
    }
}

fn kind(what: &Paste) -> ClipKind {
    match what {
        Paste::Text => ClipKind::Text,
        Paste::Image(_) => ClipKind::Image,
        Paste::Types => ClipKind::Types,
    }
}

/// This shell's block and a connection to the local server.
async fn connect() -> anyhow::Result<(Control, String)> {
    let block_id = std::env::var(BLOCK_ENV).ok().filter(|b| !b.is_empty()).with_context(|| format!("{BLOCK_ENV} is not set"))?;
    let socket = paths::socket(&paths::home());
    let (control, _events) = Control::connect(&socket, "silo clipboard").await?;
    Ok((control, block_id))
}

/// The block's clients' clipboard as `what`; `None` when none answers.
///
/// # Errors
/// Outside a Silo block, or the server cannot be reached.
pub async fn paste(what: &Paste) -> anyhow::Result<Option<Clip>> {
    let (control, block_id) = connect().await?;
    let reply = control.call_timeout(Method::ClipboardRead { block_id, kind: kind(what) }, CALL_TIMEOUT).await?;
    Ok(serde_json::from_value::<Pasted>(reply)?.content)
}

/// Set the block's clients' clipboard.
///
/// # Errors
/// Outside a Silo block, the server cannot be reached or no client is
/// attached.
pub async fn copy(text: String) -> anyhow::Result<()> {
    let (control, block_id) = connect().await?;
    control.call_timeout(Method::ClipboardWrite { block_id, location: ClipboardLocation::Standard, text }, CALL_TIMEOUT).await?;
    Ok(())
}

/// Whether `path` is one of our shims.
fn is_shim(path: &Path) -> bool {
    let mut head = [0u8; 512];
    let Ok(mut file) = std::fs::File::open(path) else { return false };
    let n = file.read(&mut head).unwrap_or(0);
    String::from_utf8_lossy(&head[..n]).contains(" clipboard shim ")
}

/// The real `tool` on `PATH`, skipping the shims.
fn real_tool(tool: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let shims = paths::shims(&paths::home());
    let shims = shims.canonicalize().unwrap_or(shims);
    std::env::split_paths(&std::env::var_os("PATH")?)
        .filter(|dir| dir.canonicalize().map_or(true, |d| d != shims))
        .map(|dir| dir.join(tool))
        .find(|p| p.metadata().is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0) && !is_shim(p))
}

/// Run the real tool, feeding it `stdin` if given; its exit status, or 1
/// when there is none.
fn fall_back(tool: &str, args: &[String], stdin: Option<&[u8]>) -> i32 {
    let Some(real) = real_tool(tool) else {
        eprintln!("{tool}: no Silo client answered and no {tool} is installed");
        return 1;
    };
    let mut command = std::process::Command::new(&real);
    command.args(args);
    let Some(input) = stdin else {
        use std::os::unix::process::CommandExt;
        let error = command.exec();
        eprintln!("{tool}: {error}");
        return 1;
    };
    let child = command.stdin(std::process::Stdio::piped()).spawn();
    let Ok(mut child) = child else { return 1 };
    if let Some(mut pipe) = child.stdin.take() {
        let _ = pipe.write_all(input);
    }
    child.wait().ok().and_then(|s| s.code()).unwrap_or(1)
}

fn read_stdin() -> anyhow::Result<String> {
    let mut bytes = Vec::new();
    std::io::stdin().read_to_end(&mut bytes)?;
    String::from_utf8(bytes).context("clipboard text must be UTF-8")
}

fn trimmed(mut text: String, trim: bool) -> String {
    if trim && text.ends_with('\n') {
        text.pop();
    }
    text
}

/// Run as shim `tool` with `args`; returns the exit status.
pub async fn shim(tool: &str, args: &[String]) -> i32 {
    use std::io::IsTerminal;
    match translate(tool, args, std::io::stdin().is_terminal()) {
        Action::Fallback => fall_back(tool, args, None),
        Action::Paste { what, newline, targets } => {
            let content = paste(&what).await.ok().flatten();
            match render(&what, content, newline, targets) {
                Some(bytes) => i32::from(std::io::stdout().write_all(&bytes).is_err()),
                None => fall_back(tool, args, None),
            }
        }
        Action::Copy { text, trim_newline } => {
            let from_args = text.is_some();
            let text = match text.map_or_else(read_stdin, Ok) {
                Ok(text) => trimmed(text, trim_newline),
                Err(error) => {
                    eprintln!("{tool}: {error:#}");
                    return 1;
                }
            };
            if copy(text.clone()).await.is_ok() {
                return 0;
            }
            fall_back(tool, args, (!from_args).then_some(text.as_bytes()))
        }
    }
}

/// `silo clipboard paste`: print the clipboard; 1 if nothing answers.
///
/// # Errors
/// Outside a Silo block or the server cannot be reached.
pub async fn paste_command(target: &str) -> anyhow::Result<i32> {
    let Some(what) = parse_target(target) else { bail!("unsupported type {target:?} (text, types or image/<format>)") };
    match render(&what, paste(&what).await?, false, false) {
        Some(bytes) => {
            std::io::stdout().write_all(&bytes)?;
            Ok(0)
        }
        None => Ok(1),
    }
}

/// `silo clipboard copy`: set the clipboard from stdin.
///
/// # Errors
/// See [`copy`].
pub async fn copy_command() -> anyhow::Result<i32> {
    copy(read_stdin()?).await?;
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(tool: &str, args: &str) -> Action {
        let args: Vec<String> = args.split_whitespace().map(str::to_owned).collect();
        translate(tool, &args, false)
    }

    fn paste(what: Paste) -> Action {
        Action::Paste { what, newline: false, targets: false }
    }

    fn copy() -> Action {
        Action::Copy { text: None, trim_newline: false }
    }

    fn png() -> Paste {
        Paste::Image("image/png".into())
    }

    #[test]
    fn pb_tools() {
        assert_eq!(t("pbpaste", ""), paste(Paste::Text));
        assert_eq!(t("pbpaste", "-Prefer txt"), paste(Paste::Text));
        assert_eq!(t("pbcopy", ""), copy());
        assert_eq!(t("pbpaste", "-pboard find"), Action::Fallback);
    }

    #[test]
    fn xclip_flags() {
        let x = |what| Action::Paste { what, newline: false, targets: true };
        assert_eq!(t("xclip", "-selection clipboard -o"), x(Paste::Text));
        assert_eq!(t("xclip", "-sel c -out"), x(Paste::Text));
        assert_eq!(t("xclip", "-selection clipboard -t TARGETS -o"), x(Paste::Types));
        assert_eq!(t("xclip", "-selection clipboard -t image/png -o"), x(png()));
        assert_eq!(t("xclip", "-o -t text/plain;charset=utf-8"), x(Paste::Text));
        assert_eq!(t("xclip", "-selection clipboard"), copy());
        assert_eq!(t("xclip", ""), copy());
        assert_eq!(t("xclip", "-selection primary -i"), copy());
        assert_eq!(t("xclip", "-sel clip -r"), Action::Copy { text: None, trim_newline: true });
        assert_eq!(t("xclip", "-selection clipboard -t image/png -i"), Action::Fallback);
        assert_eq!(t("xclip", "-o -t application/x-foo"), Action::Fallback);
        assert_eq!(t("xclip", "-selection secondary -o"), Action::Fallback);
        assert_eq!(t("xclip", "-i file.txt"), Action::Fallback);
        assert_eq!(t("xclip", "-version"), Action::Fallback);
    }

    #[test]
    fn xsel_flags() {
        assert_eq!(t("xsel", "-b -o"), paste(Paste::Text));
        assert_eq!(t("xsel", "--clipboard --output"), paste(Paste::Text));
        assert_eq!(t("xsel", "-bo"), paste(Paste::Text));
        assert_eq!(t("xsel", "-bi"), copy());
        assert_eq!(t("xsel", "-p -i"), copy());
        assert_eq!(t("xsel", "-b"), copy());
        assert_eq!(translate("xsel", &["-b".into()], true), paste(Paste::Text));
        assert_eq!(t("xsel", "-ba"), Action::Fallback);
        assert_eq!(t("xsel", "-c"), Action::Fallback);
    }

    #[test]
    fn wl_paste_flags() {
        let w = |what, newline| Action::Paste { what, newline, targets: false };
        assert_eq!(t("wl-paste", ""), w(Paste::Text, true));
        assert_eq!(t("wl-paste", "-n"), w(Paste::Text, false));
        assert_eq!(t("wl-paste", "--list-types"), w(Paste::Types, true));
        assert_eq!(t("wl-paste", "-l"), w(Paste::Types, true));
        assert_eq!(t("wl-paste", "--type image/png"), w(png(), true));
        assert_eq!(t("wl-paste", "-t image/png"), w(png(), true));
        assert_eq!(t("wl-paste", "--type=image/png --no-newline"), w(png(), false));
        assert_eq!(t("wl-paste", "-pn"), w(Paste::Text, false));
        assert_eq!(t("wl-paste", "--type text"), w(Paste::Text, true));
        assert_eq!(t("wl-paste", "--watch cat"), Action::Fallback);
        assert_eq!(t("wl-paste", "-t application/octet-stream"), Action::Fallback);
    }

    #[test]
    fn wl_copy_flags() {
        assert_eq!(t("wl-copy", ""), copy());
        assert_eq!(t("wl-copy", "--primary"), copy());
        assert_eq!(t("wl-copy", "hello world"), Action::Copy { text: Some("hello world".into()), trim_newline: false });
        assert_eq!(t("wl-copy", "-n -- -x"), Action::Copy { text: Some("-x".into()), trim_newline: true });
        assert_eq!(t("wl-copy", "--type text/plain"), copy());
        assert_eq!(t("wl-copy", "--type image/png"), Action::Fallback);
        assert_eq!(t("wl-copy", "--clear"), Action::Fallback);
        assert_eq!(t("nope", ""), Action::Fallback);
    }

    #[test]
    fn rendering() {
        let text = || Some(Clip::Text("hi".into()));
        assert_eq!(render(&Paste::Text, text(), false, false).unwrap(), b"hi");
        assert_eq!(render(&Paste::Text, text(), true, false).unwrap(), b"hi\n");
        assert_eq!(render(&png(), text(), false, false), None);
        assert_eq!(render(&Paste::Text, None, false, false), None);
        let image = Some(Clip::Image { mime: "image/png".into(), data: vec![1, 2] });
        assert_eq!(render(&png(), image.clone(), true, false).unwrap(), vec![1, 2]);
        assert_eq!(render(&Paste::Image("image/jpeg".into()), image, false, false), None);
        let types = || Some(Clip::Types(vec!["image/png".into(), "text/plain".into()]));
        assert_eq!(render(&Paste::Types, types(), false, false).unwrap(), b"image/png\ntext/plain\n");
        assert_eq!(render(&Paste::Types, types(), false, true).unwrap(), b"TARGETS\nimage/png\ntext/plain\n");
        assert_eq!(parse_target("IMAGE/PNG"), Some(png()));
        assert_eq!(parse_target("image/"), None);
    }
}
