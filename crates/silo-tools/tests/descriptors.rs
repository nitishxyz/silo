#[test]
fn descriptors_need_no_connection_or_runtime() {
    let descriptors = silo_tools::descriptors();
    assert_eq!(descriptors.len(), 28);
    assert!(descriptors.iter().any(|tool| tool.name == "terminal_run"));
    assert!(descriptors.iter().any(|tool| tool.name == "browser_snapshot" && tool.deferred));
    let show = descriptors.iter().find(|tool| tool.name == "files_show").expect("files_show");
    assert!(show.deferred && show.effects.read, "loaded on demand; it changes no files");
    assert_eq!(show.input_schema["required"], serde_json::json!(["path"]));
    let item = &show.input_schema["properties"]["highlight"]["items"];
    let item = item.get("$ref").and_then(|r| r.as_str()).map_or(item.clone(), |r| show.input_schema["$defs"][r.rsplit('/').next().unwrap()].clone());
    assert_eq!(item["required"], serde_json::json!(["start", "end"]));
    for tool in descriptors {
        assert_eq!(tool.input_schema["type"], "object");
        assert!(!tool.description.is_empty());
    }
}
