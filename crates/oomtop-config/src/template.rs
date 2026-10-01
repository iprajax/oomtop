//! `oomtop config init`: a fully commented `config.toml` listing every option with its default value and a
//! doc line, in the order of the `Config` struct; and the JSON Schema for editor completion (UX §12.9).
//!
//! Line convention: `## …` lines are documentation, `# …` lines are settings — uncommenting every `# ` line
//! yields exactly the defaults (tested).

use crate::docs::{doc_for, examples_for, section_doc};
use crate::model::Config;
use toml_edit::{DocumentMut, Item, Table};

const HEADER: &str = "\
## oomtop configuration — every option with its default value.
## Uncomment (remove the leading \"# \") and edit what you want to change; `##` lines are documentation.
##
## Precedence (later wins): built-in defaults → /etc/oomtop/ → this file → config.d/*.toml (alphabetical) →
##   config.d/host-<hostname>.toml → OOMTOP_* env vars (OOMTOP_APPEARANCE__THEME=ember) → CLI flags
##   (--set appearance.theme=ember) → runtime toggles.
## `oomtop config print --effective --origin` shows every value and which layer set it.
## Other files next to this one: themes/<name>.toml · keymap.toml · layouts/<name>.toml · rules.d/*.toml.
## This file is live-reloaded on save; an invalid edit keeps the last good config and reports file:line.
";

fn doc_lines(out: &mut String, doc: &str) {
    for l in doc.lines() {
        out.push_str("## ");
        out.push_str(l);
        out.push('\n');
    }
}

fn emit_table(out: &mut String, prefix: &str, t: &Table) {
    let header_needed = !prefix.is_empty();
    if header_needed {
        out.push('\n');
        if let Some(d) = section_doc(prefix) {
            doc_lines(out, d);
        }
        out.push_str(&format!("# [{prefix}]\n"));
    }
    let mut subtables = Vec::new();
    let mut deferred = Vec::new();
    for (k, item) in t.iter() {
        let key = if prefix.is_empty() {
            k.to_string()
        } else {
            format!("{prefix}.{k}")
        };
        match item {
            Item::Table(sub) => subtables.push((key, sub)),
            Item::ArrayOfTables(_) => subtables.push((key, t_empty())),
            Item::Value(v) => {
                if let Some(arr) = v.as_array() {
                    if arr.is_empty() && examples_for(&key).first().is_some_and(|e| e.starts_with("[[")) {
                        // an array of tables (e.g. views): documented by example only, after the tables
                        deferred.push(key);
                        continue;
                    }
                }
                if let Some(d) = doc_for(&key) {
                    doc_lines(out, d);
                }
                let rendered = v.to_string();
                out.push_str(&format!("# {k} = {}\n", rendered.trim()));
            }
            Item::None => {}
        }
    }
    let examples = examples_for(prefix);
    if header_needed && !examples.is_empty() {
        if t.is_empty() {
            if let Some(d) = doc_for(prefix) {
                doc_lines(out, d);
            }
        }
        doc_lines(out, "for example:");
        for e in examples {
            out.push_str(&format!("##   {e}\n"));
        }
    }
    for (key, sub) in subtables {
        emit_table(out, &key, sub);
    }
    for key in deferred {
        out.push('\n');
        if let Some(d) = section_doc(&key) {
            doc_lines(out, d);
        }
        if let Some(d) = doc_for(&key) {
            doc_lines(out, d);
        }
        for e in examples_for(&key) {
            out.push_str(&format!("##   {e}\n"));
        }
    }
}

fn t_empty() -> &'static Table {
    static EMPTY: std::sync::OnceLock<Table> = std::sync::OnceLock::new();
    EMPTY.get_or_init(Table::new)
}

/// Renders the commented template from `Config::default()` and the doc tables.
pub fn init_template() -> String {
    let mut out = String::from(HEADER);
    let text = toml::to_string(&Config::default()).unwrap_or_default();
    let Ok(doc) = text.parse::<DocumentMut>() else {
        return out;
    };
    emit_table(&mut out, "", doc.as_table());
    out
}

/// JSON Schema (draft 2020-12) of `config.toml`.
pub fn json_schema() -> String {
    let schema = schemars::schema_for!(Config);
    serde_json::to_string_pretty(&schema).unwrap_or_else(|_| "{}".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layered::leaves;

    #[test]
    fn every_setting_is_documented() {
        let table = match toml::Value::try_from(Config::default()) {
            Ok(toml::Value::Table(t)) => t,
            _ => panic!("default config must serialize"),
        };
        let missing: Vec<String> = leaves(&table)
            .into_iter()
            .map(|(k, _)| k)
            .filter(|k| doc_for(k).is_none())
            .collect();
        assert!(missing.is_empty(), "undocumented settings: {missing:?}");
        let stale: Vec<&str> = crate::docs::DOCS
            .iter()
            .map(|(k, _)| *k)
            .filter(|k| !leaves(&table).iter().any(|(l, _)| l == k))
            .collect();
        assert!(stale.is_empty(), "docs for settings that do not exist: {stale:?}");
        for sec in table.keys() {
            assert!(section_doc(sec).is_some(), "section {sec} lacks a doc line");
        }
    }

    #[test]
    fn template_documents_every_setting_and_uncomments_to_defaults() {
        let t = init_template();
        assert!(t.contains("# theme = \"terminal\""));
        let table = match toml::Value::try_from(Config::default()) {
            Ok(toml::Value::Table(t)) => t,
            _ => panic!(),
        };
        for (k, _) in leaves(&table) {
            let leaf = k.rsplit('.').next().unwrap();
            if let Some(doc) = doc_for(&k) {
                assert!(t.contains(doc), "doc for {k} missing from template");
            }
            assert!(
                t.contains(&format!("# {leaf} = ")) || t.contains(&format!("# [{k}]")) || k == "views",
                "{k} missing from template"
            );
        }
        // Uncommenting every setting line yields the defaults.
        let uncommented: String = t
            .lines()
            .filter(|l| l.starts_with("# "))
            .map(|l| l.trim_start_matches("# ").to_string() + "\n")
            .collect();
        let parsed: Config = toml::from_str(&uncommented).expect(&uncommented);
        assert_eq!(parsed, Config::default());
        // The documentation-only view parses as an empty config too.
        let parsed: Config = toml::from_str(&t).unwrap();
        assert_eq!(parsed, Config::default());
    }

    #[test]
    fn template_golden() {
        insta::assert_snapshot!("config_init", init_template());
    }

    #[test]
    fn schema_mentions_sections() {
        let s = json_schema();
        assert!(s.contains("\"appearance\""));
        assert!(s.contains("truecolor"));
        assert!(s.contains("live_reload"));
    }
}
