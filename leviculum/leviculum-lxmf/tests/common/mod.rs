/// Extract one scalar field from a named record in the generated fixture.
///
/// The fixture is intentionally consumed without a JSON dependency so the
/// protocol crate's test graph stays small. `gen_vectors.py` owns the stable
/// record and field names used here. Not every test binary that includes
/// this module reads this fixture file, hence the dead-code allowance.
#[allow(dead_code)]
pub fn fixture(id: &str, field: &str) -> &'static str {
    const FIXTURES: &str = include_str!("../../../docs/src/appendix/lxmf/vectors/vectors.json");
    fixture_from(FIXTURES, id, field)
}

/// [`fixture`] against a caller-supplied vectors file of the same layout
/// (`gen_telemetry_vectors.py` writes a second one for the Telemeter codec).
pub fn fixture_from(fixtures: &'static str, id: &str, field: &str) -> &'static str {
    let id_marker = format!("\"id\": \"{id}\"");
    let record_start = fixtures
        .find(&id_marker)
        .unwrap_or_else(|| panic!("missing canonical LXMF fixture {id}"));
    let tail = &fixtures[record_start..];
    let record_end = tail[1..]
        .find("\n    {\n      \"id\":")
        .map_or(tail.len(), |offset| offset + 1);
    let record = &tail[..record_end];
    let field_marker = format!("\"{field}\": ");
    let value_start = record
        .find(&field_marker)
        .unwrap_or_else(|| panic!("fixture {id} has no scalar field {field}"))
        + field_marker.len();
    let value = &record[value_start..];
    if let Some(quoted) = value.strip_prefix('"') {
        let value_end = quoted
            .find('"')
            .unwrap_or_else(|| panic!("fixture {id}.{field} is unterminated"));
        &quoted[..value_end]
    } else {
        let value_end = value
            .find(|character: char| character == ',' || character.is_ascii_whitespace())
            .unwrap_or(value.len());
        &value[..value_end]
    }
}
