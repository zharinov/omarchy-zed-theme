use crate::publish::{atomic_write_file, atomic_write_file_if_unchanged, read_regular_nofollow};
use crate::{Error, Result};
use fs2::FileExt;
use jsonc_parser::ParseOptions;
use jsonc_parser::cst::{CstInputValue, CstObject, CstObjectProp, CstRootNode};
use serde_json::{Map, Value};
use std::fs::{self, File};
use std::path::{Path, PathBuf};

const MANAGED_THEME: &str = "Omarchy";
const STATE_VERSION: u64 = 1;

#[derive(Debug)]
struct ActivationState {
    active: bool,
    previous_theme: Option<Value>,
}

const SETTINGS_PARSE_OPTIONS: ParseOptions = ParseOptions {
    allow_comments: true,
    allow_loose_object_property_names: false,
    allow_trailing_commas: true,
    allow_missing_commas: false,
    allow_single_quoted_strings: false,
    allow_hexadecimal_numbers: false,
    allow_unary_plus_numbers: false,
};

fn parse_settings(source: &str) -> Result<(CstRootNode, CstObject)> {
    let root = CstRootNode::parse(source, &SETTINGS_PARSE_OPTIONS)
        .map_err(|error| Error(format!("invalid Zed settings: {error}")))?;
    let object = root
        .object_value()
        .ok_or_else(|| Error("Zed settings must contain a JSON object".into()))?;
    Ok((root, object))
}

fn theme_property(object: &CstObject) -> Result<Option<CstObjectProp>> {
    let mut found = None;
    for property in object.properties() {
        let name = property
            .name()
            .ok_or_else(|| Error("Zed settings contain an unnamed property".into()))?
            .decoded_value()
            .map_err(|error| Error(format!("invalid Zed setting name: {error}")))?;

        if name != "theme" {
            continue;
        }
        if found.is_some() {
            return Err(Error("Zed settings contain duplicate theme keys".into()));
        }

        found = Some(property);
    }
    Ok(found)
}

fn input_value(value: &Value) -> CstInputValue {
    match value {
        Value::Null => CstInputValue::Null,
        Value::Bool(value) => CstInputValue::Bool(*value),
        Value::Number(value) => CstInputValue::Number(value.to_string()),
        Value::String(value) => CstInputValue::String(value.clone()),
        Value::Array(values) => CstInputValue::Array(values.iter().map(input_value).collect()),
        Value::Object(values) => CstInputValue::Object(
            values
                .iter()
                .map(|(key, value)| (key.clone(), input_value(value)))
                .collect(),
        ),
    }
}

fn current_theme(source: &str) -> Result<Option<Value>> {
    let (_, object) = parse_settings(source)?;
    let Some(property) = theme_property(&object)? else {
        return Ok(None);
    };
    property
        .value()
        .and_then(|value| value.to_serde_value())
        .map(Some)
        .ok_or_else(|| Error("Zed theme setting has no valid value".into()))
}

fn root_object_start(root: &CstRootNode) -> Result<usize> {
    let value_index = root
        .value()
        .ok_or_else(|| Error("Zed settings have no root value".into()))?
        .child_index();
    Ok(root
        .children()
        .iter()
        .take(value_index)
        .map(|node| node.to_string().len())
        .sum())
}

fn append_compact_theme(
    source: &str,
    root: &CstRootNode,
    object: &CstObject,
    theme: &Value,
) -> Result<String> {
    let object_source = object.to_string();
    let close = root_object_start(root)? + object_source.len() - 1;
    let properties = object.properties();
    let uses_trailing_comma = properties
        .last()
        .is_some_and(|property| property.trailing_comma().is_some());
    let separator = if properties.is_empty() {
        ""
    } else if uses_trailing_comma {
        " "
    } else {
        ", "
    };
    let trailing_comma = if uses_trailing_comma { "," } else { "" };
    let mut output = source.to_owned();
    output.insert_str(
        close,
        &format!(
            "{separator}\"theme\": {}{trailing_comma}",
            serde_json::to_string(theme)?
        ),
    );
    Ok(output)
}

fn object_is_multiline(object: &CstObject) -> bool {
    object.children().iter().any(|node| node.is_newline())
}

fn remove_compact_theme(
    source: &str,
    root: &CstRootNode,
    object: &CstObject,
    property: &CstObjectProp,
) -> Result<String> {
    let property_index = property.child_index();
    let mut start = root_object_start(root)?
        + object
            .children()
            .iter()
            .take(property_index)
            .map(|node| node.to_string().len())
            .sum::<usize>();
    let mut end = start + property.to_string().len();
    if let Some(comma) = property.trailing_comma() {
        end = root_object_start(root)?
            + object
                .children()
                .iter()
                .take(comma.child_index() + 1)
                .map(|node| node.to_string().len())
                .sum::<usize>();
        if source[..start].ends_with(' ') {
            start -= 1;
        }
    } else if source[..start].ends_with(", ") {
        start -= 2;
    }
    let mut output = source.to_owned();
    output.replace_range(start..end, "");
    Ok(output)
}

fn set_theme(source: &str, theme: Option<&Value>) -> Result<String> {
    let (root, object) = parse_settings(source)?;
    match (theme_property(&object)?, theme) {
        (Some(property), Some(theme)) => property.set_value(input_value(theme)),
        (Some(property), None)
            if !object_is_multiline(&object)
                && property.property_index() + 1 == object.properties().len() =>
        {
            return remove_compact_theme(source, &root, &object, &property);
        }
        (Some(property), None) => property.remove(),
        (None, Some(theme)) if !object_is_multiline(&object) => {
            return append_compact_theme(source, &root, &object, theme);
        }
        (None, Some(theme)) => {
            object.append("theme", input_value(theme));
        }
        (None, None) => {}
    }
    Ok(root.to_string())
}

fn state_content(state: &ActivationState) -> Result<Vec<u8>> {
    let mut object = Map::new();
    object.insert("version".into(), Value::from(STATE_VERSION));
    object.insert("active".into(), Value::from(state.active));
    if let Some(theme) = &state.previous_theme {
        object.insert("previous_theme".into(), theme.clone());
    }
    let mut content = serde_json::to_string_pretty(&Value::Object(object))?;
    content.push('\n');
    Ok(content.into_bytes())
}

fn parse_state(content: &[u8]) -> Result<ActivationState> {
    let value: Value = serde_json::from_slice(content)
        .map_err(|error| Error(format!("invalid saved Zed theme state: {error}")))?;
    let object = value
        .as_object()
        .ok_or_else(|| Error("saved Zed theme state must be an object".into()))?;
    if object.get("version").and_then(Value::as_u64) != Some(STATE_VERSION) {
        return Err(Error("unsupported saved Zed theme state version".into()));
    }
    let active = object
        .get("active")
        .and_then(Value::as_bool)
        .ok_or_else(|| Error("saved Zed theme state has no active flag".into()))?;
    Ok(ActivationState {
        active,
        previous_theme: object.get("previous_theme").cloned(),
    })
}

fn settings_source(content: Option<&[u8]>) -> Result<&str> {
    match content {
        Some(content) => std::str::from_utf8(content)
            .map_err(|_| Error("Zed settings are not valid UTF-8".into())),
        None => Ok("{}\n"),
    }
}

fn activate_settings(
    settings_path: &Path,
    state_path: &Path,
    claim_path: &Path,
    owner: &str,
    state: &mut ActivationState,
    mut needs_initial_snapshot: bool,
) -> Result<bool> {
    let managed = Value::String(MANAGED_THEME.into());
    for _ in 0..5 {
        let original = read_regular_nofollow(settings_path)?;
        let source = settings_source(original.as_deref())?;
        let current = current_theme(source)?;
        state.active = false;
        if needs_initial_snapshot || current.as_ref() != Some(&managed) {
            state.previous_theme = current;
            needs_initial_snapshot = false;
        }
        atomic_write_file(state_path, &state_content(state)?)?;
        if !claim_matches(claim_path, owner)? {
            return Ok(false);
        }

        let updated = set_theme(source, Some(&managed))?;
        if updated.as_bytes() == source.as_bytes() {
            return Ok(true);
        }
        if atomic_write_file_if_unchanged(settings_path, original.as_deref(), updated.as_bytes())?
            .is_some()
        {
            return Ok(true);
        }
    }

    Err(Error(format!(
        "Zed settings did not remain stable: {}",
        settings_path.display()
    )))
}

fn restore_settings(
    settings_path: &Path,
    claim_path: &Path,
    owner: &str,
    previous_theme: Option<&Value>,
) -> Result<Option<bool>> {
    let managed = Value::String(MANAGED_THEME.into());
    for _ in 0..5 {
        let original = read_regular_nofollow(settings_path)?;
        let source = settings_source(original.as_deref())?;
        if current_theme(source)?.as_ref() != Some(&managed) {
            return Ok(Some(false));
        }
        if !claim_matches(claim_path, owner)? {
            return Ok(None);
        }

        let updated = set_theme(source, previous_theme)?;
        if updated.as_bytes() == source.as_bytes() {
            return Ok(Some(true));
        }
        if atomic_write_file_if_unchanged(settings_path, original.as_deref(), updated.as_bytes())?
            .is_some()
        {
            return Ok(Some(true));
        }
    }

    Err(Error(format!(
        "Zed settings did not remain stable: {}",
        settings_path.display()
    )))
}

fn state_path(home: &Path) -> PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| home.join(".local/state"));
    base.join("omarchy-zed-theme/zed-settings.json")
}

pub fn config_home(home: &Path) -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| home.join(".config"))
}

fn activation_directory_lock(state_path: &Path) -> Result<File> {
    let root = state_path
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| Error("saved Zed theme state has no stable lock directory".into()))?;
    fs::create_dir_all(root)?;
    let lock = File::open(root)?;
    if !lock.metadata()?.is_dir() {
        return Err(Error(format!(
            "saved Zed theme lock path is not a directory: {}",
            root.display()
        )));
    }
    lock.lock_exclusive()
        .map_err(|error| Error(format!("cannot lock saved Zed theme state: {error}")))?;
    Ok(lock)
}

fn claim_matches(claim_path: &Path, owner: &str) -> Result<bool> {
    let Some(content) = read_regular_nofollow(claim_path)? else {
        return Ok(false);
    };
    let claim = std::str::from_utf8(&content)
        .map_err(|_| Error("activation claim is not valid UTF-8".into()))?;
    Ok(claim.trim() == owner)
}

fn activate_paths(
    settings_path: &Path,
    state_path: &Path,
    claim_path: &Path,
    owner: &str,
) -> Result<()> {
    if owner.is_empty() {
        return Err(Error("activation owner cannot be empty".into()));
    }

    let _lock = activation_directory_lock(state_path)?;
    if !claim_matches(claim_path, owner)? {
        return Ok(());
    }
    let (mut state, needs_initial_snapshot) = match read_regular_nofollow(state_path)? {
        Some(content) => (parse_state(&content)?, false),
        None => (
            ActivationState {
                active: false,
                previous_theme: None,
            },
            true,
        ),
    };

    if state.active {
        return Ok(());
    }
    if !activate_settings(
        settings_path,
        state_path,
        claim_path,
        owner,
        &mut state,
        needs_initial_snapshot,
    )? {
        return Ok(());
    }
    if !claim_matches(claim_path, owner)? {
        return Ok(());
    }
    state.active = true;
    atomic_write_file(state_path, &state_content(&state)?)?;
    Ok(())
}

fn restore_paths(
    settings_path: &Path,
    state_path: &Path,
    claim_path: &Path,
    owner: &str,
) -> Result<()> {
    if owner.is_empty() {
        return Err(Error("activation owner cannot be empty".into()));
    }

    let _lock = activation_directory_lock(state_path)?;
    if !claim_matches(claim_path, owner)? {
        return Ok(());
    }
    let Some(content) = read_regular_nofollow(state_path)? else {
        return Ok(());
    };
    let mut state = parse_state(&content)?;

    if !claim_matches(claim_path, owner)? {
        return Ok(());
    }
    let Some(restored) = restore_settings(
        settings_path,
        claim_path,
        owner,
        state.previous_theme.as_ref(),
    )?
    else {
        return Ok(());
    };

    if !claim_matches(claim_path, owner)? {
        if restored {
            state.active = false;
            atomic_write_file(state_path, &state_content(&state)?)?;
        }
        return Ok(());
    }

    fs::remove_file(state_path)?;
    if let Some(parent) = state_path.parent() {
        let _ = fs::remove_dir(parent);
    }
    Ok(())
}

pub fn activate(home: &Path, owner: &str) -> Result<()> {
    activate_paths(
        &config_home(home).join("zed/settings.json"),
        &state_path(home),
        &home.join(".local/state/omarchy/.zed-theme-owner"),
        owner,
    )
}

pub fn restore(home: &Path, owner: &str) -> Result<()> {
    restore_paths(
        &config_home(home).join("zed/settings.json"),
        &state_path(home),
        &home.join(".local/state/omarchy/.zed-theme-owner"),
        owner,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static SEQUENCE: AtomicU64 = AtomicU64::new(0);

    fn temporary() -> PathBuf {
        std::env::temp_dir().join(format!(
            "omarchy-zed-settings-{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn claim_and_activate(settings: &Path, state: &Path, owner: &str) {
        let claim = state.parent().unwrap().join("owner");
        fs::create_dir_all(claim.parent().unwrap()).unwrap();
        fs::write(&claim, format!("{owner}\n")).unwrap();
        activate_paths(settings, state, &claim, owner).unwrap();
    }

    fn restore_claim(settings: &Path, state: &Path, owner: &str) {
        let claim = state.parent().unwrap().join("owner");
        restore_paths(settings, state, &claim, owner).unwrap();
    }

    #[test]
    fn replaces_only_theme_in_jsonc() {
        let source = "{\n  // keep this\n  \"theme\": { \"mode\": \"system\", },\n  \"buffer_font_size\": 15,\n}\n";
        let changed = set_theme(source, Some(&Value::String(MANAGED_THEME.into()))).unwrap();
        assert_eq!(
            changed,
            "{\n  // keep this\n  \"theme\": \"Omarchy\",\n  \"buffer_font_size\": 15,\n}\n"
        );
    }

    #[test]
    fn adds_and_removes_theme_without_rewriting_other_settings() {
        let source = "{\n  // font\n  \"buffer_font_size\": 15\n}\n";
        let added = set_theme(source, Some(&Value::String(MANAGED_THEME.into()))).unwrap();
        assert_eq!(
            added,
            "{\n  // font\n  \"buffer_font_size\": 15,\n  \"theme\": \"Omarchy\"\n}\n"
        );
        assert_eq!(set_theme(&added, None).unwrap(), source);
    }

    #[test]
    fn adds_theme_after_an_existing_trailing_comma() {
        let source = "{\n  \"buffer_font_size\": 15, // keep\n}\n";
        let added = set_theme(source, Some(&Value::String(MANAGED_THEME.into()))).unwrap();
        assert_eq!(
            added,
            "{\n  \"buffer_font_size\": 15, // keep\n  \"theme\": \"Omarchy\",\n}\n"
        );
    }

    #[test]
    fn inserted_theme_uses_the_existing_line_endings() {
        let source = "{\r\n  \"buffer_font_size\": 15\r\n}\r\n";
        let added = set_theme(source, Some(&Value::String(MANAGED_THEME.into()))).unwrap();
        assert_eq!(
            added,
            "{\r\n  \"buffer_font_size\": 15,\r\n  \"theme\": \"Omarchy\"\r\n}\r\n"
        );
    }

    #[test]
    fn single_line_insertion_and_removal_preserve_spacing() {
        let source = "{\"buffer_font_size\":14}";
        let added = set_theme(source, Some(&Value::String(MANAGED_THEME.into()))).unwrap();
        assert_eq!(set_theme(&added, None).unwrap(), source);
    }

    #[test]
    fn compact_comments_and_trailing_commas_survive_a_round_trip() {
        for source in [
            "{\"buffer_font_size\":14 /* keep */}",
            "{\"buffer_font_size\":14, /* keep */}",
            "{\"buffer_font_size\":14,   /* keep */   }",
            "{ /* empty */ }",
            "// root\n{\"buffer_font_size\":14} // tail\n",
            "{\"nested\":{\n  \"value\":true\n}}",
            "{\"value\":true /* first\nsecond */}",
        ] {
            let added = set_theme(source, Some(&Value::String(MANAGED_THEME.into()))).unwrap();
            assert_eq!(set_theme(&added, None).unwrap(), source);
        }
    }

    #[test]
    fn rejects_non_jsonc_extensions_and_duplicate_themes() {
        for source in [
            "{\"theme\": \"A\" \"other\": true}",
            "{'theme': 'A'}",
            "{theme: \"A\"}",
            "{\"theme\": \"A\", \"theme\": \"B\"}",
        ] {
            assert!(current_theme(source).is_err(), "accepted {source}");
        }
    }

    #[test]
    fn activation_state_contains_a_json_value() {
        let state = ActivationState {
            active: true,
            previous_theme: Some(serde_json::json!({
                "mode": "system",
                "light": "One Light",
                "dark": "One Dark"
            })),
        };
        let value: Value = serde_json::from_slice(&state_content(&state).unwrap()).unwrap();
        assert!(value["previous_theme"].is_object());
    }

    #[test]
    fn missing_and_null_themes_remain_distinct() {
        let missing = ActivationState {
            active: true,
            previous_theme: None,
        };
        let missing_value: Value =
            serde_json::from_slice(&state_content(&missing).unwrap()).unwrap();
        assert!(
            !missing_value
                .as_object()
                .unwrap()
                .contains_key("previous_theme")
        );

        let null = ActivationState {
            active: true,
            previous_theme: Some(Value::Null),
        };
        let null_value: Value = serde_json::from_slice(&state_content(&null).unwrap()).unwrap();
        assert!(
            null_value
                .as_object()
                .unwrap()
                .contains_key("previous_theme")
        );
        assert!(null_value["previous_theme"].is_null());
    }

    #[test]
    fn absent_theme_is_removed_again_on_restore() {
        let home = temporary();
        let settings = home.join(".config/zed/settings.json");
        let state = home.join("state/zed-settings.json");
        fs::create_dir_all(settings.parent().unwrap()).unwrap();
        fs::write(&settings, "{\n  // keep\n  \"buffer_font_size\": 14\n}\n").unwrap();

        claim_and_activate(&settings, &state, "test-owner");
        restore_claim(&settings, &state, "test-owner");

        assert_eq!(
            fs::read_to_string(&settings).unwrap(),
            "{\n  // keep\n  \"buffer_font_size\": 14\n}\n"
        );
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn preexisting_managed_theme_is_preserved_on_first_restore() {
        let home = temporary();
        let settings = home.join("settings.json");
        let state = home.join("state/zed-settings.json");
        fs::create_dir_all(&home).unwrap();
        fs::write(&settings, "{\"theme\":\"Omarchy\"}\n").unwrap();

        claim_and_activate(&settings, &state, "test-owner");
        restore_claim(&settings, &state, "test-owner");

        assert_eq!(
            current_theme(&fs::read_to_string(&settings).unwrap()).unwrap(),
            Some(Value::String(MANAGED_THEME.into()))
        );
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn removal_handles_a_file_without_a_final_newline() {
        let source = "{\"theme\":\"Omarchy\"}";
        assert_eq!(set_theme(source, None).unwrap(), "{}");
    }

    #[test]
    fn compact_removal_uses_the_structural_trailing_comma() {
        let source = "{\"a\":1, \"theme\":\"Omarchy\" /* comment, with comma */,}";
        assert_eq!(set_theme(source, None).unwrap(), "{\"a\":1,}");
    }

    #[test]
    fn restore_preserves_changes_to_unrelated_settings() {
        let home = temporary();
        let settings = home.join(".config/zed/settings.json");
        fs::create_dir_all(settings.parent().unwrap()).unwrap();
        fs::write(
            &settings,
            "{\n  \"theme\": \"One Dark\",\n  \"buffer_font_size\": 14\n}\n",
        )
        .unwrap();

        let state = home.join("state/zed-settings.json");
        claim_and_activate(&settings, &state, "test-owner");
        let active = fs::read_to_string(&settings).unwrap();
        fs::write(&settings, active.replace("14", "18")).unwrap();
        restore_claim(&settings, &state, "test-owner");

        assert_eq!(
            fs::read_to_string(&settings).unwrap(),
            "{\n  \"theme\": \"One Dark\",\n  \"buffer_font_size\": 18\n}\n"
        );
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn restore_does_not_override_a_manual_theme_change() {
        let home = temporary();
        let settings = home.join(".config/zed/settings.json");
        fs::create_dir_all(settings.parent().unwrap()).unwrap();
        fs::write(&settings, "{ \"theme\": \"One Dark\" }\n").unwrap();

        let state = home.join("state/zed-settings.json");
        claim_and_activate(&settings, &state, "test-owner");
        fs::write(&settings, "{ \"theme\": \"Ayu\" }\n").unwrap();
        restore_claim(&settings, &state, "test-owner");

        assert_eq!(
            fs::read_to_string(&settings).unwrap(),
            "{ \"theme\": \"Ayu\" }\n"
        );
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn delayed_restore_cannot_undo_a_new_owner() {
        let home = temporary();
        let settings = home.join(".config/zed/settings.json");
        let state = home.join("state/omarchy-zed-theme/zed-settings.json");
        fs::create_dir_all(settings.parent().unwrap()).unwrap();
        fs::write(&settings, "{\"theme\":\"One Dark\"}\n").unwrap();

        claim_and_activate(&settings, &state, "old-service");
        claim_and_activate(&settings, &state, "new-service");
        let claim = state.parent().unwrap().join("owner");
        restore_paths(&settings, &state, &claim, "old-service").unwrap();

        assert_eq!(
            current_theme(&fs::read_to_string(&settings).unwrap()).unwrap(),
            Some(Value::String(MANAGED_THEME.into()))
        );
        assert!(parse_state(&fs::read(&state).unwrap()).unwrap().active);

        restore_claim(&settings, &state, "new-service");
        assert_eq!(
            current_theme(&fs::read_to_string(&settings).unwrap()).unwrap(),
            Some(Value::String("One Dark".into()))
        );
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn a_new_service_can_restore_before_it_activates() {
        let home = temporary();
        let settings = home.join("settings.json");
        let state = home.join("state/omarchy-zed-theme/zed-settings.json");
        let claim = state.parent().unwrap().join("owner");
        fs::create_dir_all(&home).unwrap();
        fs::write(&settings, "{\"theme\":\"One Dark\"}\n").unwrap();

        claim_and_activate(&settings, &state, "old-service");
        fs::write(&claim, "new-service\n").unwrap();
        restore_paths(&settings, &state, &claim, "new-service").unwrap();

        assert_eq!(
            current_theme(&fs::read_to_string(&settings).unwrap()).unwrap(),
            Some(Value::String("One Dark".into()))
        );
        assert!(!state.exists());
        assert_eq!(fs::read_to_string(&claim).unwrap(), "new-service\n");
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn interrupted_activation_is_still_restorable() {
        let home = temporary();
        let settings = home.join("settings.json");
        let state_path = home.join("state/omarchy-zed-theme/zed-settings.json");
        fs::create_dir_all(state_path.parent().unwrap()).unwrap();
        fs::write(&settings, "{\"theme\":\"Omarchy\"}\n").unwrap();
        let state = ActivationState {
            active: false,
            previous_theme: Some(Value::String("One Dark".into())),
        };
        fs::write(&state_path, state_content(&state).unwrap()).unwrap();
        let claim = state_path.parent().unwrap().join("owner");
        fs::write(&claim, "service\n").unwrap();

        restore_paths(&settings, &state_path, &claim, "service").unwrap();

        assert_eq!(
            current_theme(&fs::read_to_string(&settings).unwrap()).unwrap(),
            Some(Value::String("One Dark".into()))
        );
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn retry_preserves_a_theme_changed_after_interruption() {
        let home = temporary();
        let settings = home.join("settings.json");
        let state_path = home.join("state/omarchy-zed-theme/zed-settings.json");
        let claim = state_path.parent().unwrap().join("owner");
        fs::create_dir_all(state_path.parent().unwrap()).unwrap();
        fs::write(&settings, "{\"theme\":\"Ayu\"}\n").unwrap();
        let state = ActivationState {
            active: false,
            previous_theme: Some(Value::String("One Dark".into())),
        };
        fs::write(&state_path, state_content(&state).unwrap()).unwrap();
        fs::write(&claim, "service\n").unwrap();

        activate_paths(&settings, &state_path, &claim, "service").unwrap();
        restore_paths(&settings, &state_path, &claim, "service").unwrap();

        assert_eq!(
            current_theme(&fs::read_to_string(&settings).unwrap()).unwrap(),
            Some(Value::String("Ayu".into()))
        );
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn activation_refuses_a_settings_symlink() {
        let home = temporary();
        let settings = home.join("settings.json");
        let victim = home.join("victim.json");
        let state = home.join("state/zed-settings.json");
        fs::create_dir_all(&home).unwrap();
        fs::write(&victim, "{ \"theme\": \"One Dark\" }\n").unwrap();
        symlink(&victim, &settings).unwrap();

        let claim = state.parent().unwrap().join("owner");
        fs::create_dir_all(claim.parent().unwrap()).unwrap();
        fs::write(&claim, "test-owner\n").unwrap();
        let error = activate_paths(&settings, &state, &claim, "test-owner").unwrap_err();

        assert!(error.to_string().contains("cannot read"));
        assert_eq!(
            fs::read_to_string(&victim).unwrap(),
            "{ \"theme\": \"One Dark\" }\n"
        );
        assert!(!state.exists());
        fs::remove_dir_all(home).unwrap();
    }
}
