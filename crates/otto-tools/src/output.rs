use crate::{ToolCx, ToolError, ToolOutput, Truncation};
use std::sync::atomic::{AtomicU64, Ordering};
pub const MAX_BYTES: usize = 50 * 1024;
pub const MAX_LINES: usize = 2000;
static NEXT: AtomicU64 = AtomicU64::new(0);

pub(crate) struct Capture {
    file: tokio::fs::File,
    path: std::path::PathBuf,
    head: String,
    tail: String,
    bytes: u64,
    peak: usize,
    peak_capacity: usize,
}
impl Capture {
    pub(crate) fn new(cx: &ToolCx) -> Result<Self, ToolError> {
        let file = if let Some(dir) = &cx.spill_dir {
            std::fs::create_dir_all(dir).map_err(|e| ToolError::failed(e.to_string()))?;
            tempfile::NamedTempFile::new_in(dir)
        } else {
            tempfile::NamedTempFile::new()
        }
        .map_err(|e| ToolError::failed(e.to_string()))?;
        let (file, path) = file.keep().map_err(|e| ToolError::failed(e.to_string()))?;
        Ok(Self { file: tokio::fs::File::from_std(file), path, head: String::new(), tail: String::new(), bytes: 0, peak: 0, peak_capacity: 0 })
    }
    pub(crate) async fn push(&mut self, text: &str) -> Result<(), ToolError> {
        use tokio::io::AsyncWriteExt;
        self.file.write_all(text.as_bytes()).await.map_err(|e| ToolError::failed(e.to_string()))?;
        self.bytes += text.len() as u64;
        let budget = MAX_BYTES / 2 - 512;
        let mut split = if self.tail.is_empty() { (budget - self.head.len()).min(text.len()) } else { 0 };
        while !text.is_char_boundary(split) {
            split -= 1;
        }
        self.head.push_str(&text[..split]);
        self.tail.push_str(&text[split..]);
        if self.tail.len() > budget {
            let mut cut = self.tail.len() - budget;
            while !self.tail.is_char_boundary(cut) {
                cut += 1;
            }
            self.tail.drain(..cut);
        }
        self.peak = self.peak.max(self.head.len() + self.tail.len());
        self.peak_capacity = self.peak_capacity.max(self.head.capacity() + self.tail.capacity());
        Ok(())
    }
    pub(crate) fn snapshot(&self, cx: &ToolCx) -> Result<ToolOutput, ToolError> {
        let truncated = self.bytes > (self.head.len() + self.tail.len()) as u64;
        let text = if truncated { format!("{}\n[output truncated]\n{}", self.head, self.tail) } else { format!("{}{}", self.head, self.tail) };
        let mut context = cx.clone();
        context.spill_dir = None;
        let mut output = limit(text, &context)?;
        if truncated || output.truncated.is_some() {
            output.truncated = Some(Truncation { original_bytes: self.bytes, full_path: Some(self.path.clone()) });
        }
        output.metadata = serde_json::json!({"spill_path": self.path, "output_bytes": self.bytes, "peak_buffer_bytes": self.peak, "peak_buffer_capacity_bytes": self.peak_capacity});
        Ok(output)
    }
    pub(crate) async fn finish(&mut self, cx: &ToolCx) -> Result<ToolOutput, ToolError> {
        use tokio::io::AsyncWriteExt;
        self.file.flush().await.map_err(|e| ToolError::failed(e.to_string()))?;
        self.snapshot(cx)
    }
}
pub fn limit(text: String, cx: &ToolCx) -> Result<ToolOutput, ToolError> {
    if text.len() <= MAX_BYTES && text.lines().count() <= MAX_LINES {
        return Ok(ToolOutput::text(text));
    }
    let full_path = if let Some(dir) = &cx.spill_dir {
        std::fs::create_dir_all(dir).map_err(|e| ToolError::failed(e.to_string()))?;
        let path = dir.join(format!("output-{}-{}.txt", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        std::fs::write(&path, &text).map_err(|e| ToolError::failed(e.to_string()))?;
        Some(path)
    } else {
        None
    };
    let budget = MAX_BYTES / 2 - 256;
    let mut head = String::new();
    for line in text.split_inclusive('\n').take(MAX_LINES / 2 - 1) {
        for ch in line.chars() {
            if head.len() + ch.len_utf8() > budget {
                break;
            }
            head.push(ch);
        }
        if head.len() >= budget.saturating_sub(4) {
            break;
        }
    }
    let mut tail: String = text.chars().rev().take(budget / 4).collect();
    tail = tail.chars().rev().collect();
    tail = tail.lines().rev().take(MAX_LINES / 2 - 1).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n");
    let marker = full_path.as_ref().map_or_else(|| "[output truncated]".to_owned(), |p| format!("[output truncated; full output: {}]", p.display()));
    let mut output = ToolOutput::text(format!("{head}\n{marker}\n{tail}"));
    output.truncated = Some(Truncation { original_bytes: text.len() as u64, full_path });
    Ok(output)
}
