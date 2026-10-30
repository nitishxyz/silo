//! Whistle through needle's C API (`needle.h`, macos-arm64, linked by
//! build.rs). The engine reads no environment and makes no network calls
//! (its archive imports no socket or getenv symbol); the telemetry the
//! needle README mentions lives only in its Python package.

use std::{
    ffi::{CStr, CString, c_char, c_int, c_uchar, c_ulonglong},
    path::Path,
    sync::OnceLock,
};

use anyhow::{Context as _, anyhow};

use super::{Engine, Options, Partial};

unsafe extern "C" {
    fn needle_load(cact: *const c_uchar, n: c_ulonglong) -> c_int;
    fn needle_last_error() -> *const c_char;
    fn needle_transcribe(
        pcm: *const f32,
        samples: c_int,
        language: *const c_char,
        keywords: *const c_char,
        word_timestamps: c_int,
        out: *mut c_char,
        out_capacity: c_int,
    ) -> c_int;
    fn needle_stream_transcribe_process(
        pcm: *const f32,
        samples: c_int,
        language: *const c_char,
        keywords: *const c_char,
        out: *mut c_char,
        out_capacity: c_int,
    ) -> c_int;
    fn needle_stream_transcribe_stop(out: *mut c_char, out_capacity: c_int) -> c_int;
}

/// Room for a transcript's JSON (320 tokens at most, plus word times).
const OUT: usize = 64 * 1024;

fn last_error() -> String {
    // SAFETY: the runtime owns the string until the next API call.
    let error = unsafe { needle_last_error() };
    if error.is_null() { "unknown error".into() } else { unsafe { CStr::from_ptr(error) }.to_string_lossy().into_owned() }
}

/// The engine holds one model per process, read in place: its bytes stay
/// for the process's life, loaded once.
fn load_once(path: &Path) -> anyhow::Result<()> {
    static LOADED: OnceLock<Result<(), String>> = OnceLock::new();
    if let Some(done) = LOADED.get() {
        return done.clone().map_err(|e| anyhow!(e));
    }
    let bytes: &'static [u8] = std::fs::read(path).context("read the Fast model")?.leak();
    // SAFETY: `bytes` lives forever; the engine copies or maps what it keeps.
    let code = unsafe { needle_load(bytes.as_ptr(), bytes.len() as c_ulonglong) };
    let done = if code < 0 { Err(format!("load the Fast model: {}", last_error())) } else { Ok(()) };
    LOADED.get_or_init(|| done).clone().map_err(|e| anyhow!(e))
}

/// Calls one of the engine's text-producing functions with an output buffer.
fn call(f: impl FnOnce(*mut c_char, c_int) -> c_int) -> anyhow::Result<serde_json::Value> {
    let mut out = vec![0u8; OUT];
    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
    let code = f(out.as_mut_ptr().cast(), OUT as c_int);
    if code < 0 {
        return Err(anyhow!("{}", last_error()));
    }
    let text = CStr::from_bytes_until_nul(&out).map_err(|_| anyhow!("unterminated output"))?;
    serde_json::from_slice(text.to_bytes()).context("parse the engine's output")
}

fn field(value: &serde_json::Value, key: &str) -> String {
    value.get(key).and_then(|v| v.as_str()).unwrap_or_default().to_owned()
}

/// The language and newline-separated keywords as C strings.
fn c_options(options: &Options) -> (Option<CString>, Option<CString>) {
    let language = options.language.and_then(|l| CString::new(l).ok());
    let words: Vec<&str> = options.keywords.iter().map(|k| k.trim()).filter(|k| !k.is_empty() && !k.contains(['\n', '\0'])).collect();
    let keywords = (!words.is_empty()).then(|| CString::new(words.join("\n")).ok()).flatten();
    (language, keywords)
}

fn ptr(s: Option<&CString>) -> *const c_char {
    s.map_or(std::ptr::null(), |s| s.as_ptr())
}

pub struct Whistle {
    /// The live clip's words committed so far.
    committed: String,
}

impl Whistle {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        load_once(path)?;
        Ok(Self { committed: String::new() })
    }
}

fn samples(pcm: &[f32]) -> anyhow::Result<c_int> {
    c_int::try_from(pcm.len()).map_err(|_| anyhow!("the clip is too long"))
}

impl Engine for Whistle {
    fn transcribe(&mut self, pcm: &[f32], options: &Options) -> anyhow::Result<String> {
        let n = samples(pcm)?;
        let (language, keywords) = c_options(options);
        // SAFETY: valid pointers for the call's duration.
        let out = call(|out, cap| unsafe { needle_transcribe(pcm.as_ptr(), n, ptr(language.as_ref()), ptr(keywords.as_ref()), 0, out, cap) })?;
        Ok(field(&out, "text"))
    }

    fn stream(&mut self, pcm: &[f32], options: &Options) -> anyhow::Result<Option<Partial>> {
        let n = samples(pcm)?;
        let (language, keywords) = c_options(options);
        // SAFETY: as above.
        let out =
            call(|out, cap| unsafe { needle_stream_transcribe_process(pcm.as_ptr(), n, ptr(language.as_ref()), ptr(keywords.as_ref()), out, cap) })?;
        let words = field(&out, "text");
        if !words.trim().is_empty() {
            if !self.committed.is_empty() {
                self.committed.push(' ');
            }
            self.committed.push_str(words.trim());
        }
        Ok(Some(Partial { committed: self.committed.clone(), pending: field(&out, "pending") }))
    }

    fn end_stream(&mut self) {
        self.committed.clear();
        // SAFETY: as above; only resets the stream.
        let _ = call(|out, cap| unsafe { needle_stream_transcribe_stop(out, cap) });
    }
}
