//! Test-only JSON Schema generation for `.jjunction/config.toml`.
//!
//! The schema is generated from the **same serde types** the runtime
//! deserializes with ([`LinkEntry`], [`RepoEntry`], [`WorkspaceConfig`],
//! [`ReposConfig`]), so editors (taplo / Even Better TOML / Zed) and `jjn`
//! itself can never disagree about the file's shape. The generated document
//! is checked in at `docs/schema/config.schema.json`;
//! [`local_config_schema_stays_in_sync`] fails when they diverge.
//!
//! Regenerate after changing any config type:
//!
//! ```sh
//! just schema   # UPDATE_SCHEMA=1 cargo test schema
//! ```
//!
//! The **global** config (`~/.config/jjunction/config.toml`) has only two
//! keys read at scattered call sites (`machine`, `trusted-repos`), so its
//! schema is hand-maintained at `docs/schema/global.schema.json` — update it
//! alongside `crate::config::resolve_machine` and `crate::config::trust`.

use serde_json::Value;
use serde_json::json;

use crate::config::ReposConfig;
use crate::config::WorkspaceConfig;
use crate::link::LinkEntry;
use crate::repo::RepoEntry;

const SCHEMA_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/docs/schema/config.schema.json"
);

/// Generates the subschema for `T`, dropping the per-type `$schema` draft
/// marker (only meaningful at the document root).
fn subschema<T: schemars::JsonSchema>() -> Value {
    let mut value = serde_json::to_value(schemars::schema_for!(T)).unwrap();
    value
        .as_object_mut()
        .expect("schema_for! yields an object")
        .remove("$schema");
    value
}

/// Assembles the root schema of `.jjunction/config.toml`.
///
/// The runtime reads each section by its own key, so the root object is
/// assembled here rather than deserialized; `additionalProperties: false`
/// mirrors the `deny_unknown_fields` philosophy of every section struct —
/// this file is jjunction-owned, typos should be flagged. No section is
/// required.
fn local_config_schema() -> Value {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": "jjunction local config",
        "description": ".jjunction/config.toml — the project manifest. \
            Declarative and untrusted until `jjn trust` (see docs/design/config.md). \
            Editors: a `#:schema` directive is stamped by `jjn init`.",
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "link": subschema::<Vec<LinkEntry>>(),
            "repo": subschema::<Vec<RepoEntry>>(),
            "workspace": subschema::<WorkspaceConfig>(),
            "repos": subschema::<ReposConfig>(),
        },
    })
}

#[test]
fn local_config_schema_stays_in_sync() {
    let generated = local_config_schema();
    if std::env::var_os("UPDATE_SCHEMA").is_some() {
        std::fs::write(SCHEMA_PATH, format!("{generated:#}\n")).unwrap();
        return;
    }
    let raw = std::fs::read_to_string(SCHEMA_PATH)
        .expect("docs/schema/config.schema.json is missing; run `just schema`");
    let checked_in: Value = serde_json::from_str(&raw)
        .expect("docs/schema/config.schema.json is not valid JSON; run `just schema`");
    assert_eq!(
        checked_in, generated,
        "docs/schema/config.schema.json is stale; regenerate with `just schema`"
    );
}

#[test]
fn local_config_schema_shape_is_sane() {
    let schema = local_config_schema();
    let props = &schema["properties"];
    for section in ["link", "repo", "workspace", "repos"] {
        assert!(
            props.get(section).is_some_and(|s| !s.is_null()),
            "missing {section}"
        );
    }
    // `[[link]]` and `[[repo]]` are arrays of tables; schemars nests each
    // entry's definition under `$defs` with `items` as a `$ref`.
    assert_eq!(props["link"]["type"], "array");
    assert_eq!(props["repo"]["type"], "array");
    let link_entry = &props["link"]["$defs"]["LinkEntry"];
    // deny_unknown_fields must reach the generated entries.
    assert_eq!(link_entry["additionalProperties"], false);
    // Selectors and required fields round-trip from the serde types.
    assert!(
        link_entry["properties"]
            .get("machines")
            .is_some_and(|s| !s.is_null())
    );
    let required = link_entry["required"].as_array().unwrap();
    assert!(required.iter().any(|v| v == "type"));
    // Section defaults surface for editor hints.
    assert_eq!(props["repos"]["properties"]["secondary"]["default"], "link");
}
