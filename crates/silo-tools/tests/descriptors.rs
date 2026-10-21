#[test]
fn descriptors_need_no_connection_or_runtime() {
    let descriptors = silo_tools::descriptors();
    assert_eq!(descriptors.len(), 26);
    assert!(descriptors.iter().any(|tool| tool.name == "terminal_run"));
    assert!(descriptors.iter().any(|tool| tool.name == "browser_snapshot" && tool.deferred));
    for tool in descriptors {
        assert_eq!(tool.input_schema["type"], "object");
        assert!(!tool.description.is_empty());
    }
}
