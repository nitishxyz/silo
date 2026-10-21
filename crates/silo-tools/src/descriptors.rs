use crate::{Effects, ToolDescriptor};
use schemars::{JsonSchema, schema_for};
use serde_json::json;

pub const BROWSER: &[&str] = &[
    "browser_snapshot",
    "browser_find",
    "browser_click",
    "browser_hover",
    "browser_fill",
    "browser_fill_form",
    "browser_type",
    "browser_press",
    "browser_scroll",
    "browser_wait_for",
    "browser_dialog",
    "browser_eval",
    "browser_console",
    "browser_network",
    "browser_screenshot",
];

#[derive(JsonSchema)]
#[allow(dead_code)]
struct Fields {
    block_id: String,
    command: String,
    cwd: String,
    place: String,
    session: String,
    exec: bool,
    text: String,
    keys: Vec<String>,
    submit: bool,
    history: bool,
    lines: u32,
    status: Vec<String>,
    quiet_ms: u64,
    timeout_ms: u64,
    force: bool,
    url: String,
    back: bool,
    forward: bool,
    reload: bool,
    within: String,
    depth: u32,
    boxes: bool,
    regex: String,
    target: String,
    double: bool,
    button: String,
    value: String,
    fields: Vec<Field>,
    key: String,
    dx: f64,
    dy: f64,
    gone: bool,
    accept: bool,
    prompt_text: String,
    function: String,
    args: Vec<String>,
    since: u64,
    level: String,
    limit: u32,
    failed_only: bool,
    full_page: bool,
    format: String,
    quality: u32,
    include_snapshot: bool,
    title: String,
    body: String,
}
#[derive(JsonSchema)]
#[allow(dead_code)]
struct Field {
    target: String,
    value: String,
}

pub fn descriptors() -> Vec<ToolDescriptor> {
    let entries = [
        (
            "terminal_open",
            "Open an ordinary shell block. Drive claude, codex, opencode or otto: open with command, send text with submit:true, wait for done/blocked, then read. On blocked, answer with send. The same pattern works on remote hosts.",
            "block_id command cwd place session exec",
            "",
            false,
        ),
        (
            "terminal_run",
            "Run a shell command and return exit, duration_ms, output and truncated. Reuses a private shell block unless block_id is supplied. For structured harness results use claude -p ... --output-format json or codex exec --json ... . Timeout/cancellation does not stop the program.",
            "block_id command cwd timeout_ms",
            "command",
            false,
        ),
        (
            "terminal_send",
            "Type text then keys. submit:true sends Enter separately after 150 ms so TUIs do not treat it as pasted text. After sending a harness prompt, terminal_wait then terminal_read.",
            "block_id text keys submit",
            "",
            false,
        ),
        (
            "terminal_read",
            "Read screen text; lines includes scrollback and returns its tail. Read after terminal_wait to see a harness answer or permission prompt.",
            "block_id history lines",
            "",
            true,
        ),
        (
            "terminal_wait",
            "Wait for fresh status (default done, blocked, error), next command completion, text regex, quiet_ms or exit. timeout_ms defaults to 120000 and returns reason timeout. Hooks make harness status exact; use quiet_ms without hooks. Then terminal_read; answer blocked prompts with terminal_send.",
            "block_id status text quiet_ms timeout_ms",
            "",
            true,
        ),
        ("terminal_list", "List ordinary blocks, cwd, busy programs, status records and last command/exit.", "", "", true),
        ("terminal_close", "Close a block; force is required for a busy program.", "block_id force", "", false),
        (
            "browser_open",
            "Open an ordinary browser block. Its page runs in the Silo app that shows it, so the default (a split next to your own block) is what works best; a tab or session only runs once the user opens it. Take browser_snapshot for element refs before acting.",
            "block_id url place session",
            "url",
            false,
        ),
        (
            "browser_navigate",
            "Navigate with exactly one of url, back:true, forward:true or reload:true. Take a new snapshot after navigation.",
            "block_id url back forward reload",
            "",
            false,
        ),
        (
            "browser_snapshot",
            "Read an accessibility tree with element refs. Prefer snapshots to screenshots. A node keeps its ref until the page navigates.",
            "block_id within depth boxes",
            "",
            true,
        ),
        ("browser_find", "Find accessibility nodes and their paths; provide exactly one of text or regex.", "block_id text regex", "", true),
        (
            "browser_click",
            "Click a snapshot ref (e12) or CSS selector. include_snapshot returns the new tree.",
            "block_id target double button include_snapshot",
            "target",
            false,
        ),
        ("browser_hover", "Hover a snapshot ref or CSS selector.", "block_id target include_snapshot", "target", false),
        (
            "browser_fill",
            "Fill input, textarea or select by ref/selector; use true/false strings for checkboxes.",
            "block_id target value include_snapshot",
            "target value",
            false,
        ),
        ("browser_fill_form", "Fill several fields by ref or CSS selector.", "block_id fields include_snapshot", "fields", false),
        (
            "browser_type",
            "Type into the focused element; optional submit is a key such as Enter.",
            "block_id text submit include_snapshot",
            "text",
            false,
        ),
        ("browser_press", "Press a key or chord such as Enter, Control+A or Meta+Shift+R.", "block_id key include_snapshot", "key", false),
        ("browser_scroll", "Scroll by dx/dy, or scroll target ref/selector into view.", "block_id target dx dy include_snapshot", "", false),
        ("browser_wait_for", "Wait for page text strings (or their disappearance with gone:true).", "block_id text gone timeout_ms", "", true),
        (
            "browser_dialog",
            "Accept or dismiss the pending dialog; prompt_text answers a prompt.",
            "block_id accept prompt_text include_snapshot",
            "accept",
            false,
        ),
        (
            "browser_eval",
            "Evaluate JavaScript function source; args are element refs. Requires control access.",
            "block_id function args include_snapshot",
            "function",
            false,
        ),
        ("browser_console", "Read console messages since an id, optionally filtered by level.", "block_id since level limit", "", true),
        ("browser_network", "Read recent network requests, optionally failed only.", "block_id since limit failed_only", "", true),
        (
            "browser_screenshot",
            "Capture an image for visual checks. Prefer browser_snapshot for interaction.",
            "block_id target full_page format quality",
            "",
            true,
        ),
        ("session_list", "List visible sessions.", "", "", true),
        ("notify", "Notify the user on the agent's block, or block_id when outside a block.", "block_id title body", "body", false),
    ];
    let schema = serde_json::to_value(schema_for!(Fields)).expect("schema serializes");
    entries.into_iter().map(|(name, description, fields, required, read)| {
        let mut properties = serde_json::Map::new();
        for field in fields.split_whitespace() { properties.insert(field.into(), schema["properties"][field].clone()); }
        if name == "browser_type" { properties.insert("submit".into(), json!({"type":"string"})); }
        if name == "browser_wait_for" { properties.insert("text".into(), json!({"type":"array", "items":{"type":"string"}})); }
        for (field, choices) in [("place", vec!["split", "tab", "session"]), ("button", vec!["left", "right", "middle"]), ("format", vec!["png", "jpeg"])] {
            if let Some(property) = properties.get_mut(field) { property["enum"] = json!(choices); }
        }
        let mut input_schema = json!({"type":"object", "properties": properties, "required": required.split_whitespace().collect::<Vec<_>>(), "additionalProperties": false});
        if let Some(defs) = schema.get("$defs") { input_schema["$defs"] = defs.clone(); }
        ToolDescriptor { name: name.into(), description: description.into(), input_schema, effects: Effects { read, write: !read, process: !read }, deferred: name.starts_with("browser_") }
    }).collect()
}
