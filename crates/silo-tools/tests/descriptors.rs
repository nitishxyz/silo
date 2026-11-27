#[test]
fn descriptors_need_no_connection_or_runtime() {
    let descriptors = silo_tools::descriptors();
    assert_eq!(descriptors.len(), 35);
    // The Studio tools (docs/STUDIO.md §6.1): deferred, reads marked as reads.
    for (name, read, required) in [
        ("studio_open", false, serde_json::json!([])),
        ("studio_read", true, serde_json::json!([])),
        ("canvas_read", true, serde_json::json!(["canvas"])),
        ("canvas_create", false, serde_json::json!(["title"])),
        ("canvas_edit", false, serde_json::json!(["canvas", "ops"])),
    ] {
        let tool = descriptors.iter().find(|tool| tool.name == name).unwrap_or_else(|| panic!("{name}"));
        assert!(tool.deferred && tool.effects.read == read && !tool.effects.process, "{name}");
        assert_eq!(tool.input_schema["required"], required, "{name}");
        assert!(tool.input_schema["properties"]["path"].is_object(), "{name} takes path");
    }
    let edit = descriptors.iter().find(|tool| tool.name == "canvas_edit").unwrap();
    assert_eq!(edit.input_schema["properties"]["ops"]["type"], "array");
    assert!(edit.input_schema["$defs"].get("Op").is_some(), "the ops' schema travels with it");
    assert!(
        edit.description.contains("Don't pass x, y, w or h for cards") && edit.description.contains("use layout"),
        "cards are placed by the server"
    );
    assert!(edit.description.contains("After adding many frames and nodes, finish with {op: \"tidy\"}"), "tidy is offered");
    assert!(serde_json::to_string(&edit.input_schema).unwrap().contains("\"tidy\""), "and in the ops' schema");
    assert!(descriptors.iter().any(|tool| tool.name == "terminal_run"));
    assert!(descriptors.iter().any(|tool| tool.name == "browser_snapshot" && tool.deferred));
    let upload = descriptors.iter().find(|tool| tool.name == "browser_upload").expect("browser_upload");
    assert!(upload.deferred && !upload.effects.read);
    assert_eq!(upload.input_schema["required"], serde_json::json!(["target", "paths"]));
    assert_eq!(upload.input_schema["properties"]["paths"]["items"]["type"], "string");
    let show = descriptors.iter().find(|tool| tool.name == "files_show").expect("files_show");
    assert!(show.deferred && show.effects.read, "loaded on demand; it changes no files");
    assert_eq!(show.input_schema["required"], serde_json::json!(["path"]));
    let item = &show.input_schema["properties"]["highlight"]["items"];
    let item = item.get("$ref").and_then(|r| r.as_str()).map_or(item.clone(), |r| show.input_schema["$defs"][r.rsplit('/').next().unwrap()].clone());
    assert_eq!(item["required"], serde_json::json!(["start", "end"]));
    let changes = descriptors.iter().find(|tool| tool.name == "changes_show").expect("changes_show");
    assert!(changes.deferred && changes.effects.read, "loaded on demand; it changes no files");
    assert_eq!(changes.input_schema["properties"]["scope"]["enum"], serde_json::json!(["turn", "session", "working", "staged"]));
    for tool in descriptors {
        assert_eq!(tool.input_schema["type"], "object");
        assert!(!tool.description.is_empty());
    }
}
