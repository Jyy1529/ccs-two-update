//! Three-way, ownership-aware configuration merge. All values stay backend-only.
use crate::error::AppError;
use serde_json::{Map, Value};
use std::collections::BTreeSet;
use std::path::Path;

pub(super) fn format(path: &Path) -> &'static str {
    match path.extension().and_then(|s| s.to_str()).unwrap_or("") {
        "toml" => "toml",
        "yaml" | "yml" => "yaml",
        "json" | "jsonc" | "json5" => "json",
        "md" => "markdown",
        _ if path.file_name().is_some_and(|n| n == ".env") => "env",
        _ => "text",
    }
}

pub(super) fn parse(kind: &str, text: &str) -> Result<Value, AppError> {
    let invalid = || AppError::Config("Configuration cannot be parsed; no content was replaced".into());
    if text.trim().is_empty() && matches!(kind, "json" | "toml" | "yaml") { return Ok(Value::Object(Map::new())); }
    match kind {
        "json" => json5::from_str(text).map_err(|_| invalid()),
        "toml" => {
            let value = text.parse::<toml::Value>().map_err(|_| invalid())?;
            serde_json::to_value(value).map_err(|_| invalid())
        }
        "yaml" => serde_yaml::from_str(text).map_err(|_| invalid()),
        "env" => {
            let mut map = Map::new();
            for line in text.lines() {
                let line = line.trim();
                if line.starts_with('#') || line.is_empty() { continue; }
                let (key, value) = line.strip_prefix("export ").unwrap_or(line).split_once('=').ok_or_else(invalid)?;
                map.insert(key.trim().into(), Value::String(value.trim().into()));
            }
            Ok(Value::Object(map))
        }
        "markdown" => Ok(Value::Object(markdown_sections(text)?.into_iter().map(|(key, text)| (key, Value::String(text))).collect())),
        _ => Ok(Value::String(text.into())),
    }
}

fn markdown_sections(text: &str) -> Result<Vec<(String, String)>, AppError> {
    let mut sections = Vec::new();
    let mut key = "$preamble".to_string();
    let mut part = String::new();
    let mut fence: Option<(char, usize)> = None;
    let mut seen = BTreeSet::new();
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_end();
        let marker = trimmed.trim_start();
        let first = marker.chars().next().unwrap_or(' ');
        let count = marker.chars().take_while(|value| *value == first).count();
        if matches!(first, '`' | '~') && count >= 3 {
            if let Some((open, size)) = fence {
                if first == open && count >= size && marker[count..].trim().is_empty() { fence = None; }
            } else { fence = Some((first, count)); }
        } else if fence.is_none() && trimmed.starts_with('#') && trimmed.trim_start_matches('#').starts_with(' ') {
            sections.push((key, part));
            key = trimmed.to_string();
            if !seen.insert(key.clone()) { return Err(AppError::Config("Repeated Markdown headings need manual review; no content was replaced".into())); }
            part = String::new();
        }
        part.push_str(line);
    }
    sections.push((key, part));
    Ok(sections)
}

fn escape(key: &str) -> String { key.replace('~', "~0").replace('/', "~1") }
pub(super) fn leaves(value: &Value, path: &str, out: &mut BTreeSet<String>) {
    if let Some(object) = value.as_object() {
        for (key, value) in object { leaves(value, &format!("{path}/{}", escape(key)), out); }
    } else if let Some(values) = value.as_array().filter(|values| stable_key(values).is_some()) {
        let key = stable_key(values).unwrap();
        for value in values { leaves(value, &format!("{path}/{}", escape(value[key].as_str().unwrap())), out); }
    } else { out.insert(path.to_string()); }
}

fn stable_key(values: &[Value]) -> Option<&'static str> {
    ["slug", "id", "name"].into_iter().find(|key| {
        if values.is_empty() { return false; }
        let ids: Option<BTreeSet<_>> = values.iter().map(|value| value.get(*key).and_then(Value::as_str)).collect();
        ids.is_some_and(|ids| ids.len() == values.len())
    })
}

fn find_by_id<'a>(values: &'a [Value], key: &str, id: &str) -> Option<&'a Value> {
    values.iter().find(|value| value.get(key).and_then(Value::as_str) == Some(id))
}

pub(super) fn at_path<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    if path.is_empty() || path == "/" { return Some(value); }
    let mut current = value;
    for part in path.strip_prefix('/')?.split('/') {
        let key = part.replace("~1", "/").replace("~0", "~");
        current = match current {
            Value::Object(object) => object.get(&key)?,
            Value::Array(values) => match stable_key(values) {
                Some(id_key) => find_by_id(values, id_key, &key)?,
                None => values.get(key.parse::<usize>().ok()?)?,
            },
            _ => return None,
        };
    }
    Some(current)
}

pub(super) fn changed_paths(before: &Value, after: &Value) -> BTreeSet<String> {
    let mut fields = BTreeSet::new();
    leaves(before, "", &mut fields); leaves(after, "", &mut fields);
    fields.retain(|path| at_path(before, path) != at_path(after, path));
    if fields.is_empty() && before != after { fields.insert("/".into()); }
    fields
}

pub(super) fn matches_path(pattern: &str, path: &str) -> bool {
    if pattern.is_empty() || pattern == "/" || pattern == "*" { return true; }
    let patterns: Vec<_> = pattern.trim_start_matches('/').split('/').collect();
    let parts: Vec<_> = path.trim_start_matches('/').split('/').collect();
    patterns.len() <= parts.len() && patterns.iter().zip(parts.iter()).all(|(p, v)| *p == "*" || p == v)
}

pub(super) struct Merge<'a> {
    pub owned: &'a BTreeSet<String>,
    pub protected: &'a [String],
    pub conflicts: Vec<String>,
    pub changes: BTreeSet<String>,
    pub approved: bool,
}

impl Merge<'_> {
    pub fn value(&mut self, base: Option<&Value>, local: Option<&Value>, desired: Option<&Value>, path: &str) -> Option<Value> {
        if local == desired { return local.cloned(); }
        if self.protected.iter().any(|p| matches_path(p, path)) {
            // Protection never silently mixes connection credentials and routes.
            if is_connection_field(path) { self.conflicts.push(path.to_string()); }
            return local.cloned();
        }
        if matches!(local, Some(Value::Object(_))) && (matches!(desired, Some(Value::Object(_))) || desired.is_none()) {
            let local_map = local.unwrap().as_object().unwrap();
            let empty = Map::new();
            let desired_map = desired.and_then(Value::as_object).unwrap_or(&empty);
            let mut result = Map::new();
            for key in local_map.keys().chain(desired_map.keys()) {
                if result.contains_key(key) { continue; }
                let key_path = format!("{path}/{}", escape(key));
                if let Some(value) = self.value(base.and_then(|v| v.get(key)), local_map.get(key), desired_map.get(key), &key_path) { result.insert(key.clone(), value); }
            }
            return if desired.is_none() && result.is_empty() { None } else { Some(Value::Object(result)) };
        }
        // Replacing a container cannot bypass a protected descendant. This is
        // especially important for removing an auth/provider object as a unit.
        if local.is_some_and(|value| value.is_object() || value.is_array())
            && self.protected.iter().any(|protected| matches_path(path, protected)) {
            self.conflicts.push(if path.is_empty() { "/".into() } else { path.into() });
            return local.cloned();
        }
        if let (Some(Value::Array(local_values)), Some(Value::Array(desired_values))) = (local, desired) {
            // Model catalog records use stable IDs; changing one model must not
            // erase user fields or conflict with an unrelated model's edit.
            if let Some(key) = ["slug", "id", "name"].into_iter().find(|key| {
                (!local_values.is_empty() || !desired_values.is_empty())
                    && [local_values, desired_values].iter().all(|values| {
                        let ids: Option<BTreeSet<_>> = values.iter().map(|v| v.get(*key).and_then(Value::as_str)).collect();
                        ids.is_some_and(|ids| ids.len() == values.len())
                    })
            }) {
                let base_values = base.and_then(Value::as_array);
                let mut ids = BTreeSet::new();
                let mut result = Vec::new();
                for value in desired_values.iter().chain(local_values.iter()) {
                    let id = value.get(key).and_then(Value::as_str).unwrap();
                    if !ids.insert(id) { continue; }
                    let base_value = base_values.and_then(|values| find_by_id(values, key, id));
                    let entry_path = format!("{path}/{}", escape(id));
                    if let Some(mut value) = self.value(base_value, find_by_id(local_values, key, id), find_by_id(desired_values, key, id), &entry_path) {
                        if let Some(object) = value.as_object_mut() { object.entry(key).or_insert_with(|| Value::String(id.into())); }
                        result.push(value);
                    }
                }
                return Some(Value::Array(result));
            }
        }
        let owned = self.owned.iter().any(|p| matches_path(p, path));
        if desired.is_none() && !owned && !self.approved { return local.cloned(); }
        if base == desired && base.is_some() && !self.approved { return local.cloned(); }
        if !owned && !self.approved && base.is_some() { return local.cloned(); }
        if !self.approved && local.is_some() && (base.is_none() || (base != local && base != desired)) {
            self.conflicts.push(if path.is_empty() { "/".into() } else { path.into() });
            return local.cloned();
        }
        // Existing unknown fields belong to the user even after a baseline is
        // established. Only explicit review can transfer them to ccs.
        if !owned && !self.approved && local.is_some() { return local.cloned(); }
        self.changes.insert(path.to_string());
        desired.cloned()
    }
}

pub(super) fn is_connection_field(path: &str) -> bool {
    let key = path.to_ascii_lowercase();
    ["api_key", "apikey", "auth_token", "base_url", "baseurl", "model_provider", "access_token", "bearer_token", "endpoint", "selectedtype", "authscheme", "deploymentmode", "inferenceprovider", "env_key", "wire_api"].iter().any(|v| key.contains(v))
        || key.ends_with("/model") || key.ends_with("/provider") || key.ends_with("/default_model")
}

pub(super) fn render(kind: &str, original: &str, proposed: &str, local: &Value, desired: &Value, merged: &Value) -> Result<String, AppError> {
    if merged == local { return Ok(original.into()); }
    if original.is_empty() && merged == desired { return Ok(proposed.into()); }
    if merged == desired && !matches!(kind, "toml" | "markdown" | "env") { return Ok(proposed.into()); }
    let invalid = || AppError::Config("Cannot safely render merged configuration".into());
    match kind {
        "toml" => {
            let toml_value: toml::Value = serde_json::from_value(merged.clone()).map_err(|_| invalid())?;
            let template = toml::to_string(&toml_value).map_err(|_| invalid())?.parse::<toml_edit::DocumentMut>().map_err(|_| invalid())?;
            let mut document = original.parse::<toml_edit::DocumentMut>().map_err(|_| invalid())?;
            patch_toml(document.as_table_mut(), template.as_table(), local, merged);
            Ok(document.to_string())
        }
        "yaml" => serde_yaml::to_string(merged).map_err(|_| invalid()),
        "markdown" => {
            let remaining = merged.as_object().ok_or_else(invalid)?;
            let mut emitted = BTreeSet::new();
            let mut output = String::new();
            // Preserve local section order, including fenced examples. Append
            // genuinely new sections in proposal order, never map sort order.
            for (key, _) in markdown_sections(original)?.into_iter().chain(markdown_sections(proposed)?) {
                if emitted.insert(key.clone()) {
                    if let Some(value) = remaining.get(&key).and_then(Value::as_str) { output.push_str(value); }
                }
            }
            Ok(output)
        },
        "env" => {
            let mut remaining = merged.as_object().ok_or_else(invalid)?.clone();
            let mut output = String::new();
            for line in original.split_inclusive('\n') {
                if line.trim().starts_with('#') || !line.contains('=') { output.push_str(line); continue; }
                let key = line.trim().strip_prefix("export ").unwrap_or(line.trim()).split_once('=').unwrap().0.trim();
                if let Some(value) = remaining.remove(key) {
                    if local.get(key) == Some(&value) { output.push_str(line); }
                    else { output.push_str(&format!("{key}={}\n", value.as_str().unwrap_or_default())); }
                }
            }
            for (key, value) in remaining { output.push_str(&format!("{key}={}\n", value.as_str().unwrap_or_default())); }
            Ok(output)
        }
        "text" => merged.as_str().map(str::to_string).ok_or_else(invalid),
        _ => serde_json::to_string_pretty(merged).map_err(|source| AppError::JsonSerialize { source }),
    }
}

fn patch_toml(target: &mut dyn toml_edit::TableLike, template: &dyn toml_edit::TableLike, before: &Value, after: &Value) {
    let removed: Vec<String> = target.iter().filter(|(key, _)| after.get(*key).is_none()).map(|(key, _)| key.to_string()).collect();
    for key in removed { target.remove(&key); }
    for (key, item) in template.iter() {
        if before.get(key) == after.get(key) { continue; }
        if let (Some(current), Some(next)) = (target.get_mut(key).and_then(toml_edit::Item::as_table_like_mut), item.as_table_like()) {
            patch_toml(current, next, before.get(key).unwrap_or(&Value::Null), after.get(key).unwrap_or(&Value::Null));
        } else { target.insert(key, item.clone()); }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn safety_native_document_unknown_overlap_is_preserved_without_false_conflict() {
        let owned = BTreeSet::from(["/model".into()]);
        let mut merge = Merge { owned: &owned, protected: &[], conflicts: vec![], changes: BTreeSet::new(), approved: false };
        let result = merge.value(Some(&json!({"custom":1})), Some(&json!({"custom":2})), Some(&json!({"custom":3})), "");
        assert_eq!(result, Some(json!({"custom":2})));
        assert!(merge.conflicts.is_empty());
    }
    #[test]
    fn safety_native_document_parent_deletion_cannot_remove_protected_child() {
        let owned = BTreeSet::from(["/connection".into()]);
        let protected = vec!["/connection/api_key".into()];
        let base = json!({"connection":{"api_key":"synthetic","base_url":"old"}});
        let mut merge = Merge { owned: &owned, protected: &protected, conflicts: vec![], changes: BTreeSet::new(), approved: true };
        let result = merge.value(Some(&base), Some(&base), Some(&json!({})), "").unwrap();
        assert_eq!(result["connection"]["api_key"], "synthetic");
        assert!(merge.conflicts.iter().any(|path| path == "/connection/api_key"));
    }
    #[test]
    fn safety_native_document_parent_type_change_cannot_remove_protected_child() {
        let owned = BTreeSet::from(["/connection".into()]);
        let protected = vec!["/connection/custom".into()];
        let base = json!({"connection":{"custom":7}});
        let mut merge = Merge { owned: &owned, protected: &protected, conflicts: vec![], changes: BTreeSet::new(), approved: true };
        assert_eq!(merge.value(Some(&base), Some(&base), Some(&json!({"connection":"replacement"})), ""), Some(base.clone()));
        assert!(!merge.conflicts.is_empty());
    }
    #[test]
    fn safety_native_document_markdown_preserves_order_and_fenced_headings() {
        let original = "intro\n# Z\nold\n```sh\n# not a heading\n```\n# A\nuser\n";
        let proposed = "intro\n# Z\nnew\n```sh\n# not a heading\n```\n# A\nuser\n";
        let local = parse("markdown", original).unwrap();
        let desired = parse("markdown", proposed).unwrap();
        assert!(local.get("# not a heading").is_none());
        let owned = BTreeSet::from(["/# Z".into()]);
        let mut merge = Merge { owned: &owned, protected: &[], conflicts: vec![], changes: BTreeSet::new(), approved: false };
        let result = merge.value(Some(&local), Some(&local), Some(&desired), "").unwrap();
        assert_eq!(render("markdown", original, proposed, &local, &desired, &result).unwrap(), proposed);
    }
    #[test]
    fn independent_changes_preserve_user_fields() {
        let base = json!({"model":"a","custom":1});
        let local = json!({"model":"a","custom":2,"new":true});
        let desired = json!({"model":"b"});
        let owned = BTreeSet::from(["/model".into()]);
        let mut merge = Merge { owned: &owned, protected: &[], conflicts: vec![], changes: BTreeSet::new(), approved: false };
        assert_eq!(merge.value(Some(&base), Some(&local), Some(&desired), ""), Some(json!({"model":"b","custom":2,"new":true})));
        assert!(merge.conflicts.is_empty());
    }
    #[test]
    fn safety_native_document_removes_the_last_owned_array_record() {
        let base = json!({"entries":[{"id":"managed","name":"Managed"}]});
        let desired = json!({"entries":[]});
        let owned = BTreeSet::from(["/entries/*/id".into(), "/entries/*/name".into()]);
        let mut merge = Merge { owned: &owned, protected: &[], conflicts: vec![], changes: BTreeSet::new(), approved: false };
        assert_eq!(merge.value(Some(&base), Some(&base), Some(&desired), ""), Some(desired));
        assert!(merge.conflicts.is_empty());
    }
    #[test]
    fn safety_native_document_preserves_record_identity_with_user_owned_fields() {
        let base = json!({"models":[{"slug":"synthetic","input_modalities":["text","image"],"extension":7}]});
        let owned = BTreeSet::from(["/models/*/slug".into()]);
        let mut merge = Merge { owned: &owned, protected: &[], conflicts: vec![], changes: BTreeSet::new(), approved: false };
        assert_eq!(merge.value(Some(&base), Some(&base), Some(&json!({"models":[]})), ""), Some(base));
        assert!(merge.conflicts.is_empty());
    }
    #[test]
    fn overlapping_external_change_needs_review() {
        let owned = BTreeSet::from(["/model".into()]);
        let mut merge = Merge { owned: &owned, protected: &[], conflicts: vec![], changes: BTreeSet::new(), approved: false };
        merge.value(Some(&json!({"model":"a"})), Some(&json!({"model":"local"})), Some(&json!({"model":"remote"})), "");
        assert_eq!(merge.conflicts, vec!["/model"]);
    }
    #[test]
    fn catalog_records_merge_by_slug() {
        let base = json!({"models":[{"slug":"a","input_modalities":["text"]},{"slug":"b","limit":10}]});
        let local = json!({"models":[{"slug":"a","input_modalities":["text","image"],"extension":42},{"slug":"b","limit":10}]});
        let desired = json!({"models":[{"slug":"a","input_modalities":["text"]},{"slug":"b","limit":20}]});
        let owned = BTreeSet::from(["/models".into()]);
        let mut merge = Merge { owned: &owned, protected: &[], conflicts: vec![], changes: BTreeSet::new(), approved: false };
        let result = merge.value(Some(&base), Some(&local), Some(&desired), "").unwrap();
        assert_eq!(result["models"][0]["input_modalities"], json!(["text","image"]));
        assert_eq!(result["models"][0]["extension"], 42);
        assert_eq!(result["models"][1]["limit"], 20);
        assert!(merge.conflicts.is_empty());
    }
}
