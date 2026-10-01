//! JSON Schemas (draft 2020-12) for every oomtop file (UX §12.9): editor autocomplete/validation via taplo or
//! VS Code (`#:schema ./schemas/oomtop-config.schema.json` or a taplo `[[rule]]`).
//!
//! - `config.toml`, `config.d/*.toml` — from [`crate::model::Config`] (schemars)
//! - `themes/*.toml` — generated from [`crate::theme::TOKENS`]
//! - `keymap.toml` — contexts × keys → [`crate::keymap::ACTIONS`]
//! - `layouts/*.toml` — from [`crate::layout::Layout`] (schemars)
//! - `rules.d/*.toml` — mirror of `oomtop_core::attribution::RuleSet` (tested to stay in sync)

use crate::keymap::{ACTIONS, ACTION_ALIASES, CONTEXTS, UNBIND};
use crate::theme::{Style, SECTIONS, TOKENS};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

/// Which file a schema describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchemaKind {
    Config,
    Theme,
    Keymap,
    Layout,
    Rules,
}

pub const SCHEMA_KINDS: &[SchemaKind] = &[
    SchemaKind::Config,
    SchemaKind::Theme,
    SchemaKind::Keymap,
    SchemaKind::Layout,
    SchemaKind::Rules,
];

impl SchemaKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SchemaKind::Config => "config",
            SchemaKind::Theme => "theme",
            SchemaKind::Keymap => "keymap",
            SchemaKind::Layout => "layout",
            SchemaKind::Rules => "rules",
        }
    }
    pub fn parse(s: &str) -> Option<SchemaKind> {
        SCHEMA_KINDS.iter().copied().find(|k| k.as_str() == s)
    }
    /// `oomtop-<kind>.schema.json`.
    pub fn file_name(self) -> String {
        format!("oomtop-{}.schema.json", self.as_str())
    }
    /// Glob of the files this schema applies to, relative to the config dir.
    pub fn applies_to(self) -> &'static [&'static str] {
        match self {
            SchemaKind::Config => &["config.toml", "config.d/*.toml"],
            SchemaKind::Theme => &["themes/*.toml"],
            SchemaKind::Keymap => &["keymap.toml"],
            SchemaKind::Layout => &["layouts/*.toml"],
            SchemaKind::Rules => &["rules.d/*.toml"],
        }
    }
}

fn to_value<T: JsonSchema>() -> Value {
    serde_json::to_value(schemars::schema_for!(T)).unwrap_or(Value::Null)
}

fn with_meta(mut v: Value, kind: SchemaKind, title: &str) -> Value {
    if let Some(o) = v.as_object_mut() {
        o.insert(
            "$id".into(),
            json!(format!("https://oomtop.dev/schemas/{}", kind.file_name())),
        );
        o.insert("title".into(), json!(title));
        o.insert(
            "$schema".into(),
            json!("https://json-schema.org/draft/2020-12/schema"),
        );
    }
    v
}

fn theme_schema() -> Value {
    let style = to_value::<Style>();
    let color = json!({
        "type": "string",
        "description": "ANSI index 0-255 or name (\"yellow\", \"bright-black\"), \"default\", \"#rrggbb\", or a token reference \"@ui.accent\"",
        "anyOf": [
            {"pattern": "^#[0-9a-fA-F]{6}$"},
            {"pattern": "^([01]?[0-9]?[0-9]|2[0-4][0-9]|25[0-5])$"},
            {"enum": ["black","red","green","yellow","blue","magenta","cyan","white","bright-black","bright-red","bright-green","bright-yellow","bright-blue","bright-magenta","bright-cyan","bright-white","default"]},
            {"pattern": format!("^@({})$", TOKENS.join("|").replace('.', "\\."))}
        ]
    });
    let mut style_obj = style.clone();
    if let Some(o) = style_obj.as_object_mut() {
        o.remove("$schema");
        o.remove("title");
        if let Some(Value::Object(props)) = o.get_mut("properties") {
            props.insert("fg".into(), color.clone());
            props.insert("bg".into(), color.clone());
        }
    }
    let token_value = json!({"anyOf": [ {"$ref": "#/$defs/color"}, {"$ref": "#/$defs/style"} ]});
    let mut props = Map::new();
    props.insert("name".into(), json!({"type": "string"}));
    props.insert(
        "inherits".into(),
        json!({"type": "string", "description": "parent theme (built-in or themes/<name>.toml)"}),
    );
    props.insert("appearance".into(), json!({"enum": ["dark", "light"]}));
    for sec in SECTIONS {
        let mut sp = Map::new();
        for t in TOKENS
            .iter()
            .filter_map(|t| t.strip_prefix(&format!("{sec}.") as &str))
        {
            sp.insert(t.to_string(), token_value.clone());
            // nested form: border.focus = { … } → [ui.border] focus
            if let Some((head, tail)) = t.split_once('.') {
                let entry = sp.entry(head.to_string()).or_insert_with(|| token_value.clone());
                let nested = json!({"anyOf": [
                    {"$ref": "#/$defs/color"}, {"$ref": "#/$defs/style"},
                    {"type": "object", "properties": {tail: token_value.clone()}}
                ]});
                *entry = nested;
            }
        }
        props.insert(
            sec.to_string(),
            json!({"type": "object", "properties": sp, "additionalProperties": false}),
        );
    }
    let v = json!({
        "type": "object",
        "properties": props,
        "additionalProperties": false,
        "$defs": { "color": color, "style": style_obj },
    });
    with_meta(v, SchemaKind::Theme, "oomtop theme (themes/<name>.toml)")
}

fn keymap_schema() -> Value {
    let mut actions: Vec<&str> = ACTIONS.to_vec();
    actions.extend(ACTION_ALIASES.iter().map(|(a, _)| *a));
    actions.push(UNBIND);
    let binds = json!({
        "type": "object",
        "description": "\"key\" = \"action\"; keys like \"x\", \"G\", \"ctrl-k\", \"F9\", chords \"g g\"",
        "additionalProperties": {"enum": actions},
    });
    let mut props = Map::new();
    for c in CONTEXTS {
        props.insert(c.to_string(), binds.clone());
    }
    with_meta(
        json!({"type": "object", "properties": props, "additionalProperties": false}),
        SchemaKind::Keymap,
        "oomtop keymap (keymap.toml)",
    )
}

/// Mirror of `oomtop_core::attribution::MatchSpec` for the schema.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct MatchSpecDoc {
    /// Globs against the executable basename and full path (e.g. "claude", "**/bin/sd-server").
    pub exe: Vec<String>,
    /// Globs against the short process name (comm).
    pub name: Vec<String>,
    /// Globs against any argv element after argv[0] (node/python scripts, java main classes).
    pub script: Vec<String>,
    /// Regexes against the space-joined command line.
    pub cmdline: Vec<String>,
    /// Marker keys (must be in privacy.marker_allowlist); groups carriers into the rule's session.
    pub env: Vec<String>,
    /// Globs against cwd.
    pub cwd: Vec<String>,
    /// Globs against the macOS bundle id.
    pub bundle: Vec<String>,
    /// Globs against the Linux cgroup path.
    pub cgroup: Vec<String>,
}

/// Group kinds accepted in rules (SPEC §5; short aliases too).
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum KindDoc {
    AgentSession,
    App,
    ModelServer,
    Sandbox,
    BuildDaemon,
    System,
    #[default]
    Other,
    Agent,
    Model,
    Daemon,
}

/// Mirror of `oomtop_core::attribution::Rule` for the schema.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct RuleDoc {
    /// Optional stable id; defaults to the label slug.
    pub id: Option<String>,
    pub kind: KindDoc,
    pub label: String,
    /// Identity match; fields are OR'd.
    #[serde(rename = "match")]
    pub matcher: MatchSpecDoc,
    pub exclude: Option<MatchSpecDoc>,
    /// "env:KEY" → one group per distinct (hashed) marker value.
    pub session_key: Option<String>,
    pub priority: i32,
    /// Matching processes are never offered for actions.
    pub protected: bool,
}

/// `rules.d/*.toml`: `[[group]]` tables.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct RulesDoc {
    pub group: Vec<RuleDoc>,
}

/// The schema of one file kind.
pub fn schema_for(kind: SchemaKind) -> Value {
    match kind {
        SchemaKind::Config => with_meta(
            to_value::<crate::model::Config>(),
            kind,
            "oomtop configuration (config.toml, config.d/*.toml)",
        ),
        SchemaKind::Theme => theme_schema(),
        SchemaKind::Keymap => keymap_schema(),
        SchemaKind::Layout => with_meta(
            to_value::<crate::layout::Layout>(),
            kind,
            "oomtop layout (layouts/<name>.toml)",
        ),
        SchemaKind::Rules => with_meta(
            to_value::<RulesDoc>(),
            kind,
            "oomtop detection rules (rules.d/*.toml)",
        ),
    }
}

/// Pretty JSON of one schema.
pub fn schema_text(kind: SchemaKind) -> String {
    serde_json::to_string_pretty(&schema_for(kind)).unwrap_or_else(|_| "{}".into()) + "\n"
}

/// A taplo config (`.taplo.toml`) associating every file with its schema.
pub fn taplo_config(schema_dir: &str) -> String {
    let mut out = String::from("# generated by `oomtop config schema --write`\n");
    for k in SCHEMA_KINDS {
        out.push_str("\n[[rule]]\n");
        let globs: Vec<String> = k.applies_to().iter().map(|g| format!("\"{g}\"")).collect();
        out.push_str(&format!("include = [{}]\n", globs.join(", ")));
        out.push_str(&format!(
            "schema = {{ path = \"{schema_dir}/{}\" }}\n",
            k.file_name()
        ));
    }
    out
}

/// Writes every schema to `dir` (created); returns the files written.
pub fn write_schemas(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    std::fs::create_dir_all(dir)?;
    let mut out = Vec::new();
    for k in SCHEMA_KINDS {
        let p = dir.join(k.file_name());
        std::fs::write(&p, schema_text(*k))?;
        out.push(p);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_schema_is_an_object_with_id() {
        for k in SCHEMA_KINDS {
            let v = schema_for(*k);
            assert!(v.is_object(), "{k:?}");
            assert!(v["$id"].as_str().unwrap().ends_with(&k.file_name()));
            assert_eq!(SchemaKind::parse(k.as_str()), Some(*k));
        }
        let theme = schema_text(SchemaKind::Theme);
        assert!(theme.contains("\"selection\""));
        assert!(theme.contains("\"spark\""));
        let km = schema_text(SchemaKind::Keymap);
        assert!(km.contains("\"open:cwd\"") && km.contains("\"details\""));
        let layout = schema_text(SchemaKind::Layout);
        assert!(layout.contains("\"split\"") && layout.contains("\"custom\""));
        let rules = schema_text(SchemaKind::Rules);
        assert!(rules.contains("\"session_key\"") && rules.contains("agent_session"));
    }

    #[test]
    fn rules_mirror_matches_core() {
        // a rule file valid for the mirror is valid for core, and every core field is in the mirror
        let text = r#"
[[group]]
id = "user:studio"
kind = "model_server"
label = "Qwen-Image studio"
match.exe = ["sd-server"]
match.script = ["**/studio.py"]
match.env = ["CLAUDECODE"]
exclude.name = ["sd-cli"]
session_key = "env:CLAUDE_CODE_SESSION_ID"
priority = 5
protected = false

[[group]]
kind = "daemon"
label = "Gradle"
match.cmdline = ["GradleDaemon"]
"#;
        let _: RulesDoc = toml::from_str(text).unwrap();
        let core: oomtop_core::attribution::RuleSet = toml::from_str(text).unwrap();
        assert_eq!(core.rules.len(), 2);
        let core_rule = toml::Value::try_from(oomtop_core::attribution::Rule {
            exclude: Some(Default::default()),
            session_key: Some(String::new()),
            id: Some(String::new()),
            ..Default::default()
        })
        .unwrap();
        let mirror_rule = toml::Value::try_from(RuleDoc {
            exclude: Some(Default::default()),
            session_key: Some(String::new()),
            id: Some(String::new()),
            ..Default::default()
        })
        .unwrap();
        let keys = |v: &toml::Value| {
            let mut k: Vec<String> = v.as_table().unwrap().keys().cloned().collect();
            k.sort();
            k
        };
        assert_eq!(keys(&core_rule), keys(&mirror_rule));
        assert_eq!(keys(&core_rule["match"]), keys(&mirror_rule["match"]));
    }

    #[test]
    fn writes_all() {
        let d = tempfile::tempdir().unwrap();
        let files = write_schemas(d.path()).unwrap();
        assert_eq!(files.len(), 5);
        for f in files {
            let v: Value = serde_json::from_str(&std::fs::read_to_string(f).unwrap()).unwrap();
            assert!(v.is_object());
        }
        assert!(taplo_config("schemas").contains("themes/*.toml"));
    }
}
