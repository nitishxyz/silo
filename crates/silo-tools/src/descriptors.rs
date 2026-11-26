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
    visible: bool,
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
    screenshot: bool,
    selector: String,
    path: String,
    line: u64,
    highlight: Vec<Highlight>,
    scope: String,
    hunks: Vec<HunkMark>,
}
#[derive(JsonSchema)]
#[allow(dead_code)]
struct HunkMark {
    /// The file (absolute, ~/…, or relative to your working directory).
    path: String,
    /// A line on the new side, 1-based.
    line: u64,
    /// A short note shown beside it.
    note: Option<String>,
}
#[derive(JsonSchema)]
#[allow(dead_code)]
struct Highlight {
    /// First line, 1-based.
    start: u64,
    /// Last line, inclusive (start for one line).
    end: u64,
    /// A short label shown beside the lines.
    note: Option<String>,
}
#[derive(JsonSchema)]
#[allow(dead_code)]
struct Field {
    target: String,
    value: String,
}

/// Whether `name` is one of Silo's own tools (or an old name of one).
pub fn is_builtin(name: &str) -> bool {
    static NAMES: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    name == "terminal_list" || NAMES.get_or_init(|| descriptors().into_iter().map(|d| d.name).collect()).iter().any(|n| n == name)
}

pub fn descriptors() -> Vec<ToolDescriptor> {
    let entries = [
        (
            "terminal_open",
            "Open a shell block for long-lived or stateful work: dev servers, watchers, REPLs, ssh, anything the user may take over; read it with terminal_read/terminal_wait. place: split (default, next to your block), tab (a new tab in your session, or in session) or session (a new session). visible:false parks it next to your block, out of every layout (the user can reveal it; it closes with your block). Drive claude, codex, opencode or otto: open with command, send text with submit:true, wait for done/blocked, then read. On blocked, answer with send. The same pattern works on remote hosts.",
            "block_id command cwd place session exec visible",
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
        (
            "block_list",
            "List every block in every session (terminal, browser, agent, files): id, kind, title, cwd, status records and busy program; terminals also give their last command and exit code.",
            "",
            "",
            true,
        ),
        ("terminal_close", "Close a block; force is required for a busy program.", "block_id force", "", false),
        (
            "browser_open",
            "Open an ordinary browser block (default: a split next to your own block). place:tab opens a new tab in your session (session names another one). visible:false parks it next to your block, out of every layout: a background browser the Silo app runs off screen, with the same state, history and actions as a shown one (the user can reveal it; it closes with your block). Take browser_snapshot for element refs before acting.",
            "block_id url place session visible",
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
            "Click a snapshot ref (e12) or CSS selector. The result describes the page the click led to; navigating_to means a navigation is still loading (wait_for or snapshot next). include_snapshot returns the new tree.",
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
            "Type into the focused element; optional submit is a key such as Enter. With submit the result describes the page it led to (navigating_to: still loading).",
            "block_id text submit include_snapshot",
            "text",
            false,
        ),
        (
            "browser_press",
            "Press a key or chord such as Enter, Control+A or Meta+Shift+R. The result describes the page it led to (navigating_to: still loading).",
            "block_id key include_snapshot",
            "key",
            false,
        ),
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
        (
            "browser_fetch",
            "Load a URL in a hidden page of the Silo app and return the rendered DOM as {url (final), title, status, html}: for pages built by JavaScript or behind bot checks, where plain HTTP fails. The page uses Silo's own agent browser profile (cookies persist across fetches, never the user's), is never shown and closes afterwards. Waits for load and network quiet up to timeout_ms (default 15000, max 60000); timed_out:true means the HTML is what had rendered by then; blocked:true means a bot check did not clear within a few seconds. selector returns that element's outerHTML; screenshot:true adds a viewport image. Fails with no_app when the Silo app is not running on this Mac.",
            "url timeout_ms screenshot selector",
            "url",
            true,
        ),
        ("session_list", "List visible sessions.", "", "", true),
        (
            "files_show",
            "Show the user a file in Silo: opens it in a Files block beside your block (reusing one in your window whose folder holds the file), scrolls to `line` or the first highlight, and marks each `highlight` range (1-based, inclusive lines) with a tinted background and its optional short `note` as a label. Use it to point at code you are explaining, reviewing or just changed. It returns no contents; read files with your own tools. `path` is absolute, ~/…, or relative to your working directory; a folder opens the block there. Calling it again for the same file replaces its highlights (none given clears them); they go when the user closes the tab.",
            "path line highlight block_id",
            "path",
            true,
        ),
        (
            "changes_show",
            "Show the user what you changed, in your own Changes block in Silo (a diff review beside your block, reused on later calls): scope turn (default: this turn's changes), session (everything this session changed), working (the uncommitted work against HEAD) or staged. path opens that file's diff; each of hunks marks a line (new side, 1-based) with an optional short note (check this, the risky part). Calling it again replaces the marks (none given clears them). It returns no diff; the user reviews, comments and sends the review back to you as a message.",
            "path scope hunks block_id",
            "",
            true,
        ),
        ("notify", "Notify the user on the agent's block, or block_id when outside a block.", "block_id title body", "body", false),
    ];
    let schema = serde_json::to_value(schema_for!(Fields)).expect("schema serializes");
    entries.into_iter().map(|(name, description, fields, required, read)| {
        let mut properties = serde_json::Map::new();
        for field in fields.split_whitespace() { properties.insert(field.into(), schema["properties"][field].clone()); }
        if name == "browser_type" { properties.insert("submit".into(), json!({"type":"string"})); }
        if name == "browser_wait_for" { properties.insert("text".into(), json!({"type":"array", "items":{"type":"string"}})); }
        for (field, choices) in [("place", vec!["split", "tab", "session"]), ("button", vec!["left", "right", "middle"]), ("format", vec!["png", "jpeg"]), ("scope", vec!["turn", "session", "working", "staged"])] {
            if let Some(property) = properties.get_mut(field) { property["enum"] = json!(choices); }
        }
        let mut input_schema = json!({"type":"object", "properties": properties, "required": required.split_whitespace().collect::<Vec<_>>(), "additionalProperties": false});
        if let Some(defs) = schema.get("$defs") { input_schema["$defs"] = defs.clone(); }
        ToolDescriptor { name: name.into(), description: description.into(), input_schema, effects: Effects { read, write: !read, process: !read }, deferred: name.starts_with("browser_") || name == "files_show" || name == "changes_show" }
    }).chain(studio())
    .collect()
}

const STUDIO_PATH: &str = "The studio's folder (or any path in it): absolute, ~/…, or relative to your working directory. Default: your working directory. The studio is the nearest folder up with a studio.toml.";

/// The Studio tools (docs/STUDIO.md §6.1), deferred like `changes_show`.
fn studio() -> Vec<ToolDescriptor> {
    let string = |description: &str| json!({"type": "string", "description": description});
    let object = |properties: serde_json::Value, required: &[&str]| json!({"type": "object", "properties": properties, "required": required, "additionalProperties": false});
    let mut ops = serde_json::to_value(schema_for!(Vec<silo_proto::studio::Op>)).expect("schema serializes");
    let defs = ops.as_object_mut().and_then(|o| o.remove("$defs"));
    if let Some(o) = ops.as_object_mut() {
        o.remove("$schema");
        o.remove("title");
        o.insert("description".into(), json!("Edits applied in order, each on its own (a failing one is reported, the rest apply)."));
    }
    let mut edit = object(
        json!({
            "path": string(STUDIO_PATH),
            "canvas": string("The canvas's name (its folder under canvases/, as studio_read lists it)."),
            "ops": ops,
            "base_rev": {"type": "integer", "minimum": 0, "description": "The rev you read (canvas_read); if the canvas changed since, the ops still apply by id and the result says moved."},
        }),
        &["canvas", "ops"],
    );
    if let Some(defs) = defs {
        edit["$defs"] = defs;
    }
    let tool = |name: &str, description: &str, input_schema: serde_json::Value, read: bool| ToolDescriptor {
        name: name.into(),
        description: description.into(),
        input_schema,
        effects: Effects { read, write: !read, process: false },
        deferred: true,
    };
    vec![
        tool(
            "studio_open",
            "Open or focus your own Studio block (beside your block, linked to you) on a studio: a folder with studio.toml holding world/ (the bible: characters, places, props, style) and canvases/ (boards of cards; an episode is a canvas). canvas shows that canvas's tab; world shows the World view on a section (characters, places, props, overview, style, or any folder under world/) or an entity (characters/mara); production: true shows the Production view (where the whole production stands and what is next). Without studio.toml the block shows Setup.",
            object(
                json!({
                    "path": string(STUDIO_PATH),
                    "canvas": string("A canvas to show (its name)."),
                    "world": string("A world section (characters) or entity id (characters/mara) to show."),
                    "production": {"type": "boolean", "description": "Show the Production view (when neither canvas nor world is given)."},
                }),
                &[],
            ),
            false,
        ),
        tool(
            "studio_read",
            "Read a studio as compact text: its name, format presets and the default (16:9, 9:16, …), first a Production block (where the production stands, derived from the files: each asset's sheet, approval, voice and video sheet; each episode's script, shots, keyframes, video, sound, cut and out-of-date, failed or running cards; the model picks; the next steps), then the canvases (name, title, episode, format, node count) and the world: each section's entities with name, summary and the paths of their markdown file, sheet and reference images; each canvas lists its documents (canvases/<name>/*.md, .txt: the script). Read it first when working in a studio. The world is plain files you edit with your file tools (read, write, edit): world/overview.md, world/style.md, and an entity per folder (world/characters/mara/mara.md with frontmatter name, sheet, refs, voice, tags, summary, then the description). Make a sheet with generate_image {path: \"world/characters/mara/sheet.png\"} and name it in the frontmatter's sheet. Generations meant for a canvas take canvas: {name, frame?} on the generation tools (it adds the card); build graphs of cards with canvas_edit. The Studio block follows file changes by itself.",
            object(json!({"path": string(STUDIO_PATH)}), &[]),
            true,
        ),
        tool(
            "canvas_read",
            "Read a canvas as an outline: first its documents (canvases/<name>/*.md, .txt, such as script.md, each marked whether it is pinned on the canvas), then frames (scenes, shots) in order with the nodes in them, then nodes outside frames, then the wires. Each node has its id; cards (kind gen) show what they make, title, model, status, prompt, options, takes (* the picked one) and OUT OF DATE when a wired input changed since its take; assets show their file and size (a document card: `document <path>`); notes their text; entities their id. A card's line ends with its prompt's mentions, each with what it names and whether it is wired (`mentions @characters/mara = \"Mara Venn\" (wired reference from n_ab)`), so you can read a prompt the user wrote. Wires read `n_a → n_b first_frame` (source → card, port). No pixels: look at an image file with read when you need to see it. frame limits it to one frame.",
            object(
                json!({
                    "path": string(STUDIO_PATH),
                    "canvas": string("The canvas's name."),
                    "frame": string("Only this frame (its node id) and what is in it."),
                }),
                &["canvas"],
            ),
            true,
        ),
        tool(
            "canvas_create",
            "Make a canvas (a board; with episode it is that episode of the series, named ep01-<title>) and open its tab in your Studio block. format overrides the project's default for it (a preset name from studio_read: landscape, vertical, square, portrait, or one the project adds). Starts the studio (studio.toml and world/) when the folder has none. Returns its name, folder and assets folder.",
            object(
                json!({
                    "path": string(STUDIO_PATH),
                    "title": string("Its title (Pilot, Moodboard)."),
                    "episode": {"type": "integer", "minimum": 1, "description": "The episode number, for an episode."},
                    "format": string("A format preset name overriding the project's default."),
                }),
                &["title"],
            ),
            false,
        ),
        tool(
            "canvas_edit",
            "Edit a canvas: add, update and remove nodes and edges, and lay nodes out. The canvas is a graph you generate from: a card (body kind gen) makes an image, video, speech, sound or music (makes) from its prompt and from what is wired into its input ports. A new card needs only {kind: gen, makes, prompt} (model?, title?, options? such as aspect, length, voice; model unset: automatic); it is a draft until it runs. Wires are edges from a source to a card whose role is the port: image cards have reference and text; video cards first_frame, middle_frame, last_frame (one wire each; a new one replaces it), reference, video, audio, text; speech, sound and music cards text. Frame and reference ports take images, video a video, audio audio, text text. Sources: an asset (its file; a document's text goes to a text port), an entity node {kind: entity, id: characters/mara} (its sheet), another card (its picked take), a note (its text). Wires never loop; a wire that doesn't fit fails with the reason. A mention in a card's prompt is a wire: @characters/mara (a world entity, @<section>/<id>) or @#n_3f2a9c (a node of this canvas) wires that source into the card when the prompt is written (an entity gets its Entity node, added beside the card when the canvas has none; an image goes to reference, a note or document to text; a video or audio source you wire by hand into the port it is for), and the result's mentions says what was wired, unwired or why not. Deleting the token deletes the wire it made, never one drawn by hand. When the card runs each mention becomes its name with where it went: \"Mara Venn (image 2)\" (its number among the references as sent), \"(the first frame)\", \"(video 1)\", \"(audio 1)\", the bare name for a text or an unwired source. So write @characters/mara in a card's prompt rather than wiring her and numbering \"image 1 is Mara\" by hand; tokens in notes or titles do nothing, an e-mail or an unknown id is plain text. Role next (not a wire) orders shots when frames don't. Build a shot as a keyframe image card (references: the characters' and place's entity nodes) wired into a video card's first_frame, with a frame {kind: frame, role: shot} around them inside a scene frame. Don't pass x, y, w or h for cards: the server sizes cards as the Studio draws them and spaces them so their input ports (left) and output (right) stay clear of their neighbours. A node without x/y goes right of the last node in its frame (a top-level frame goes below the content), sized for its media at the canvas's format; to arrange many, use layout {ids, as: row|column|grid, in?} (no gap: cards get room for their ports). A frame grows to hold what is placed in it and pushes the frames after it (right, below) along. After adding many frames and nodes, finish with {op: \"tidy\"} (frame?: only inside that frame): scenes in a column and shots in a row in order, each frame fitted to its contents, nothing overlapping; canvas_read says when frames overlap. update_node's body merges (pick: a take's index to show another take). Ids of added nodes and edges come back in op order; use them in later ops of the next call. Cards are run with canvas_run {canvas, nodes} (an otto tool), never by this tool. For a quick one-off, the generation tools (generate_image, generate_video, generate_audio, generate_media) take canvas: {name, frame?} and put the result on the canvas as a card with its take, wired from its inputs. To put an existing file (any file under the studio) on the canvas add an asset node {kind: asset, path} (media is inferred from the extension). A document you wrote for the canvas (canvases/<name>/script.md, references.md) is pinned the same way, {kind: asset, path: \"canvases/<name>/script.md\"}: the user reads it on the board as a document card (place it top-left, outside frames), so don't restate it in a note; wire it into a card's text port only when its text should be context for that generation (the first 8,000 characters are used).",
            edit,
            false,
        ),
    ]
}
