//! Pins the wire schema to `protocol.json`: hellos, events, every method's
//! params and result, the frame kind table and the bytes of every data
//! frame. A change fails until the file is updated (additive change) or
//! `PROTOCOL` bumped (breaking one).

use schemars::schema_for;
use serde_json::{Map, Value, json};
use silo_proto::{
    Frame, PROTOCOL, api,
    control::{ClientHello, Event, METHODS, ServerHello},
    data::{ClientData, Clip, ClipKind, ClipboardLocation, CommandAnchor, CommandEnd, DataHello, Reject, ServerData, Welcome},
    kind,
    model::{TermSize, TerminalConfig},
    upload::{UploadEntry, UploadHello, Uploaded},
};

const FILE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/protocol.json");

/// Schemas without prose (doc comments may change freely), keys sorted
/// (`serde_json` may preserve insertion order in workspace builds). Only
/// string `description` annotations go; a property named `description`
/// is part of the schema.
fn plain(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<(String, Value)> = map.into_iter().filter(|(k, v)| !(k == "description" && v.is_string())).collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            Value::Object(entries.into_iter().map(|(k, v)| (k.clone(), if k == "properties" { properties(v) } else { plain(v) })).collect())
        }
        Value::Array(items) => Value::Array(items.into_iter().map(plain).collect()),
        other => other,
    }
}

/// A `properties` map: its keys are field names, each value a schema.
fn properties(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<(String, Value)> = map.into_iter().collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            Value::Object(entries.into_iter().map(|(k, v)| (k, plain(v))).collect())
        }
        other => plain(other),
    }
}

/// `pub const NAME: u8 = 0x..;` of `pub mod kind` in `src/lib.rs`, up to
/// the module's closing brace.
fn frame_kinds() -> Map<String, Value> {
    let source = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/lib.rs")).unwrap();
    let body = &source[source.find("pub mod kind {").expect("kind module")..];
    body.lines()
        .take_while(|line| *line != "}")
        .filter_map(|line| {
            let (name, value) = line.trim().strip_prefix("pub const ")?.split_once(": u8 = ")?;
            Some((name.to_owned(), json!(value.trim_end_matches(';'))))
        })
        .collect()
}

/// Every data frame as sent (`len | kind | payload`): the header and
/// binary payloads in hex, JSON payloads as text.
fn data_frames() -> Map<String, Value> {
    let size = TermSize { columns: 0x0102, rows: 0x0304, cell_width: 0x0506, cell_height: 0x0708 };
    let image = Clip::Image { mime: "image/png".into(), data: vec![0x89, b'P', 0, 0xff] };
    let server = [
        ("SnapshotHead", ServerData::SnapshotHead(b"head".to_vec())),
        ("SnapshotHistory", ServerData::SnapshotHistory(b"hist".to_vec())),
        ("SnapshotDone", ServerData::SnapshotDone),
        ("Output", ServerData::Output { offset: 0x0102_0304_0506_0708, bytes: b"hi".to_vec() }),
        ("Resized", ServerData::Resized { offset: 9, size }),
        ("Exited/code", ServerData::Exited { code: Some(-2) }),
        ("Exited/unknown", ServerData::Exited { code: None }),
        ("Configured", ServerData::Configured(TerminalConfig::default())),
        ("InputAck", ServerData::InputAck { count: 3, offset: 0x0a0b }),
        ("TtyMode/echo", ServerData::TtyMode { offset: 5, echo: true, icanon: false }),
        ("TtyMode/icanon", ServerData::TtyMode { offset: 5, echo: false, icanon: true }),
        ("ClipboardWrite/standard", ServerData::ClipboardWrite { location: ClipboardLocation::Standard, text: "é".into() }),
        ("ClipboardWrite/primary", ServerData::ClipboardWrite { location: ClipboardLocation::Primary, text: "x".into() }),
        ("ClipboardRead/text", ServerData::ClipboardRead { request: 7, kind: ClipKind::Text }),
        ("ClipboardRead/image", ServerData::ClipboardRead { request: 7, kind: ClipKind::Image }),
        ("ClipboardRead/types", ServerData::ClipboardRead { request: 7, kind: ClipKind::Types }),
    ]
    .map(|(name, data)| (format!("server/{name}"), data.to_frame()));
    // Decoded by the clients that use them (Phase 7 prompts); pinned here.
    let anchors = [CommandAnchor { record: 6, rows_from_bottom: 40 }, CommandAnchor { record: 7, rows_from_bottom: 3 }];
    let commands = [
        ("server/CommandEnd".to_owned(), Frame::json(kind::COMMAND_END, &CommandEnd { offset: 0x0102, record: 7, rows_from_bottom: 3 }).unwrap()),
        ("server/Commands".to_owned(), Frame::json(kind::COMMANDS, &anchors).unwrap()),
    ];
    let client = [
        ("Input", ClientData::Input(b"ls\r".to_vec())),
        ("Resize", ClientData::Resize(size)),
        ("Ack", ClientData::Ack(0x0102)),
        ("Config", ClientData::Config(TerminalConfig::default())),
        ("ClipboardContent/none", ClientData::ClipboardContent { request: 4, content: None }),
        ("ClipboardContent/text", ClientData::ClipboardContent { request: 4, content: Some(Clip::Text("a".into())) }),
        ("ClipboardContent/image", ClientData::ClipboardContent { request: 4, content: Some(image.clone()) }),
        ("ClipboardContent/types", ClientData::ClipboardContent { request: 4, content: Some(Clip::Types(vec!["a/b".into(), "c/d".into()])) }),
        ("ClipboardPush/text", ClientData::ClipboardPush { content: Clip::Text("p".into()) }),
        ("ClipboardPush/image", ClientData::ClipboardPush { content: image }),
    ]
    .map(|(name, data)| (format!("client/{name}"), data.to_frame()));
    let hex = |bytes: &[u8]| bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    let show = |frame: Frame| {
        let encoded = frame.encode();
        let (header, payload) = encoded.split_at(5);
        let json = serde_json::from_slice::<Value>(payload).is_ok() && matches!(payload.first(), Some(b'{' | b'['));
        json!(format!("{} {}", hex(header), if json { String::from_utf8_lossy(payload).into_owned() } else { hex(payload) }))
    };
    server.into_iter().chain(commands).chain(client).map(|(name, frame)| (name, show(frame))).collect()
}

fn schema() -> Value {
    let methods: Map<String, Value> = METHODS
        .iter()
        .map(|(name, _)| {
            let described = api::describe(name).expect("described");
            ((*name).to_owned(), json!({"params": plain(described.params_schema), "result": plain(described.result_schema)}))
        })
        .collect();
    plain(json!({
        "protocol": PROTOCOL,
        "hellos": {
            "ClientHello": plain(schema_for!(ClientHello).to_value()),
            "ServerHello": plain(schema_for!(ServerHello).to_value()),
            "DataHello": plain(schema_for!(DataHello).to_value()),
            "Welcome": plain(schema_for!(Welcome).to_value()),
            "Reject": plain(schema_for!(Reject).to_value()),
            "UploadHello": plain(schema_for!(UploadHello).to_value()),
            "UploadEntry": plain(schema_for!(UploadEntry).to_value()),
            "Uploaded": plain(schema_for!(Uploaded).to_value()),
        },
        "events": plain(schema_for!(Event).to_value()),
        "terminal_config": plain(schema_for!(TerminalConfig).to_value()),
        "methods": methods,
        "frame_kinds": frame_kinds(),
        "data_frames": data_frames(),
    }))
}

#[test]
fn wire_schema_matches_protocol_json() {
    let current = serde_json::to_string_pretty(&schema()).unwrap() + "\n";
    if std::env::var_os("SILO_UPDATE_PROTOCOL").is_some() {
        std::fs::write(FILE, &current).unwrap();
        return;
    }
    let pinned = std::fs::read_to_string(FILE).unwrap_or_default();
    if pinned != current {
        let line =
            pinned.lines().zip(current.lines()).position(|(a, b)| a != b).unwrap_or_else(|| pinned.lines().count().min(current.lines().count()));
        panic!(
            "the wire schema changed (protocol.json line {}).\n\
             Additive? update protocol.json: SILO_UPDATE_PROTOCOL=1 cargo test -p silo-proto --test protocol\n\
             Breaking? bump PROTOCOL in crates/silo-proto/src/lib.rs (and update protocol.json).",
            line + 1
        );
    }
}
