use crate::config::write_json_file_with_contents;
use crate::error::AppError;
use crate::provider::OpenCodeConfigFormat;
use crate::settings::get_opencode_override_dir;
use indexmap::IndexMap;
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

const STANDARD_OMO_PLUGIN_PREFIXES: [&str; 2] = ["oh-my-openagent", "oh-my-opencode"];
const SLIM_OMO_PLUGIN_PREFIXES: [&str; 1] = ["oh-my-opencode-slim"];
fn opencode_config_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn read_config_contents(path: &Path) -> Result<Option<Vec<u8>>, AppError> {
    match std::fs::read(path) {
        Ok(contents) => Ok(Some(contents)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(AppError::io(path, err)),
    }
}

fn matches_plugin_prefix(plugin_name: &str, prefix: &str) -> bool {
    plugin_name == prefix
        || plugin_name
            .strip_prefix(prefix)
            .map(|suffix| suffix.starts_with('@'))
            .unwrap_or(false)
}

fn matches_any_plugin_prefix(plugin_name: &str, prefixes: &[&str]) -> bool {
    prefixes
        .iter()
        .any(|prefix| matches_plugin_prefix(plugin_name, prefix))
}

fn canonicalize_plugin_name(plugin_name: &str) -> String {
    if let Some(suffix) = plugin_name.strip_prefix("oh-my-opencode") {
        if suffix.is_empty() || suffix.starts_with('@') {
            return format!("oh-my-openagent{suffix}");
        }
    }
    plugin_name.to_string()
}

pub fn get_opencode_dir() -> PathBuf {
    if let Some(override_dir) = get_opencode_override_dir() {
        return override_dir;
    }

    crate::config::get_home_dir()
        .join(".config")
        .join("opencode")
}

/// OpenCode 在全局配置目录里认三个文件名，`globalConfigFile()` 按这个顺序取第一个
/// 存在的，全都不存在时新建 `opencode.jsonc`，见
/// https://github.com/sst/opencode/blob/dev/packages/opencode/src/config/config.ts
///
/// 读取侧它三个都读并 `mergeDeep`，后合并的覆盖先合并的，所以 `opencode.jsonc`
/// 优先级最高：写进 `opencode.json` 的同名键会被它静默盖掉。
const OPENCODE_CONFIG_FILE_NAMES: [&str; 3] = ["opencode.jsonc", "opencode.json", "config.json"];

pub fn get_opencode_config_path() -> PathBuf {
    let dir = get_opencode_dir();

    for name in OPENCODE_CONFIG_FILE_NAMES {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return candidate;
        }
    }

    // 一个都不存在时保持建 `opencode.json`：OpenCode 读取时会一并合并它，
    // 之后 `globalConfigFile()` 也会选中它，行为与本函数此前一致。
    dir.join("opencode.json")
}

/// 获取 OpenCode SQLite 数据库路径
/// 优先级: OPENCODE_DB 环境变量 > XDG_DATA_HOME > ~/.local/share/opencode
pub fn get_opencode_db_path() -> PathBuf {
    // 支持 OPENCODE_DB 环境变量覆盖（忽略空字符串）
    if let Ok(custom_path) = std::env::var("OPENCODE_DB") {
        if !custom_path.is_empty() {
            let path = PathBuf::from(&custom_path);
            if path.is_absolute() {
                return path;
            }
            // 相对路径基于数据目录
            return get_opencode_data_dir().join(path);
        }
    }

    get_opencode_data_dir().join("opencode.db")
}

fn get_opencode_data_dir() -> PathBuf {
    // 尊重 XDG_DATA_HOME（按 XDG 规范，空字符串视为未设置）
    if let Ok(xdg_data) = std::env::var("XDG_DATA_HOME") {
        if !xdg_data.is_empty() {
            return PathBuf::from(xdg_data).join("opencode");
        }
    }

    // OpenCode 使用 xdg-basedir，不遵守 macOS/Windows 平台约定，
    // 所有平台默认都落在 ~/.local/share/opencode
    crate::config::get_home_dir()
        .join(".local")
        .join("share")
        .join("opencode")
}

#[allow(dead_code)]
pub fn get_opencode_env_path() -> PathBuf {
    get_opencode_dir().join(".env")
}

fn read_opencode_config_from_path(path: &Path) -> Result<Value, AppError> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(json!({
                "$schema": "https://opencode.ai/config.json"
            }));
        }
        Err(err) => return Err(AppError::io(path, err)),
    };
    let value: Value = json5::from_str(&content).map_err(|e| {
        AppError::Config(format!(
            "Failed to parse OpenCode config: {}: {e}",
            path.display()
        ))
    })?;

    // 根节点必须是对象：下游 set_provider / set_mcp_server / add_plugin 都对它做
    // `config["key"] = …` 索引赋值，而 serde_json 只把 Null 自动升级成对象，
    // 数组或标量会直接 panic（panic 发生在 Tauri command 内、跨 FFI 展开）。
    //
    // 这里选择报错而不是重建根节点：opencode.json 里还有 model / theme 等用户自有
    // 配置，静默重建等于删掉它们。让用户自己修文件，与 read_claude_live 的做法一致。
    if !value.is_object() {
        return Err(AppError::Config(format!(
            "OpenCode 配置文件根节点必须是 JSON 对象: {}",
            path.display()
        )));
    }

    Ok(value)
}

/// 已存在的配置文件，按 OpenCode 的**合并顺序**返回（后者覆盖前者），
/// 即 `OPENCODE_CONFIG_FILE_NAMES` 的倒序。
fn existing_opencode_config_paths() -> Vec<PathBuf> {
    let dir = get_opencode_dir();

    OPENCODE_CONFIG_FILE_NAMES
        .iter()
        .rev()
        .map(|name| dir.join(name))
        .filter(|path| path.is_file())
        .collect()
}

/// 对齐 OpenCode 的 `mergeConfig`（remeda `mergeDeep`）：对象逐键递归合并，
/// 其余类型（含数组）由 source 整体替换。
fn merge_config_into(target: &mut Value, source: Value) {
    match source {
        Value::Object(source_obj) if target.is_object() => {
            let target_obj = target.as_object_mut().expect("checked by the guard");

            for (key, value) in source_obj {
                match target_obj.get_mut(&key) {
                    Some(existing) if existing.is_object() && value.is_object() => {
                        merge_config_into(existing, value);
                    }
                    _ => {
                        target_obj.insert(key, value);
                    }
                }
            }
        }
        other => *target = other,
    }
}

/// 读取「有效配置」：OpenCode 会把全局目录下的 `config.json`、`opencode.json`、
/// `opencode.jsonc` 全部读出来依次 `mergeDeep`，只看其中一个文件会漏掉另一个文件里
/// 的供应商——例如旧版 CC Switch 把供应商写进了 `opencode.json`，而 OpenCode 自己
/// 建了 `opencode.jsonc`。
pub fn read_opencode_config() -> Result<Value, AppError> {
    let paths = existing_opencode_config_paths();

    let Some((first, rest)) = paths.split_first() else {
        // 一个都不存在，保持与单文件读取相同的空配置。
        return read_opencode_config_from_path(&get_opencode_config_path());
    };

    let mut merged = read_opencode_config_from_path(first)?;
    for path in rest {
        merge_config_into(&mut merged, read_opencode_config_from_path(path)?);
    }

    Ok(merged)
}

/// Note: writes via `serde_json::to_string_pretty`, which strips comments
/// and trailing commas from `.jsonc` files.
fn write_opencode_config_to_path_with_contents(
    path: &Path,
    config: &Value,
) -> Result<Vec<u8>, AppError> {
    let contents = write_json_file_with_contents(path, config)?;

    log::debug!("OpenCode config written to {path:?}");
    Ok(contents)
}

pub fn get_providers() -> Result<Map<String, Value>, AppError> {
    Ok(get_providers_with_format()?
        .into_iter()
        .map(|(id, (value, _))| (id, value))
        .collect())
}

/// Preserve the declaration's format alongside its JSON, including package-less
/// built-in overrides. Do not recursively mix legacy and native entries.
pub fn get_providers_with_format(
) -> Result<IndexMap<String, (Value, OpenCodeConfigFormat)>, AppError> {
    let config = read_opencode_config()?;
    let mut providers: IndexMap<_, _> = config
        .get("provider")
        .and_then(|v| v.as_object())
        .into_iter()
        .flatten()
        .map(|(id, value)| (id.clone(), (value.clone(), OpenCodeConfigFormat::V1)))
        .collect();
    if let Some(native) = config.get("providers").and_then(Value::as_object) {
        for (id, value) in native {
            if is_native_provider(value) {
                providers.insert(id.clone(), (value.clone(), OpenCodeConfigFormat::V2));
            } else {
                log::warn!("Invalid native OpenCode provider '{id}', leaving its source untouched");
            }
        }
    }
    Ok(providers)
}

/// Validate known native fields before giving V2 precedence over V1. Keep the
/// original JSON (including unknown extensions) rather than projecting it into
/// a partial type. Constraints follow OpenCode v2.0.12, commit 2670273ff17d:
/// packages/schema/src/config/provider.ts, model.ts and provider.ts.
pub fn is_native_provider(value: &Value) -> bool {
    let Some(obj) = value.as_object() else {
        return false;
    };
    if ["npm", "options", "api"]
        .iter()
        .any(|key| obj.contains_key(*key))
    {
        return false;
    }
    native_fields_valid(
        value,
        &[
            ("name", Value::is_string),
            ("package", Value::is_string),
            ("canonical", Value::is_string),
            ("env", native_string_array),
            ("settings", native_provider_settings),
            ("models", |models| {
                models
                    .as_object()
                    .is_some_and(|models| models.values().all(native_model))
            }),
        ],
    ) && native_request_overlays(value)
}

type NativeFieldCheck<'a> = (&'a str, fn(&Value) -> bool);

/// Optional means absent, not null. Unknown keys remain untouched at every level.
fn native_fields_valid(value: &Value, fields: &[NativeFieldCheck<'_>]) -> bool {
    value.as_object().is_some_and(|obj| {
        fields
            .iter()
            .all(|(key, check)| obj.get(*key).is_none_or(check))
    })
}

fn native_string_array(value: &Value) -> bool {
    value
        .as_array()
        .is_some_and(|values| values.iter().all(Value::is_string))
}

fn native_finite(value: &Value) -> bool {
    value.as_f64().is_some_and(f64::is_finite)
}

fn native_integer(value: &Value) -> bool {
    // Effect Schema.Int uses JavaScript's safe integer range, including 1.0.
    value
        .as_f64()
        .is_some_and(|n| n.fract() == 0.0 && n.abs() <= 9_007_199_254_740_991.0)
}

fn native_compaction(value: &Value) -> bool {
    value.as_object().is_some_and(|obj| {
        matches!(
            obj.get("type").and_then(Value::as_str),
            Some("summary" | "native")
        )
    })
}

fn native_provider_settings(value: &Value) -> bool {
    native_fields_valid(
        value,
        &[
            ("timeout", |v| v == &Value::Bool(false) || native_finite(v)),
            ("chunkTimeout", native_finite),
            ("compaction", native_compaction),
            ("transport", |v| {
                matches!(v.as_str(), Some("http" | "websocket"))
            }),
        ],
    )
}

fn native_request_overlays(value: &Value) -> bool {
    native_fields_valid(
        value,
        &[
            ("headers", |v| {
                v.as_object()
                    .is_some_and(|headers| headers.values().all(Value::is_string))
            }),
            ("body", Value::is_object),
        ],
    )
}

fn native_model_overlays(value: &Value) -> bool {
    native_request_overlays(value)
        && native_fields_valid(
            value,
            &[
                // Model/variant settings only constrain compaction; timeout and transport
                // here are package-specific extensions, unlike provider settings.
                ("settings", |v| {
                    native_fields_valid(v, &[("compaction", native_compaction)])
                }),
            ],
        )
}

fn native_model(value: &Value) -> bool {
    native_model_overlays(value)
        && native_fields_valid(
            value,
            &[
                ("modelID", Value::is_string),
                ("family", Value::is_string),
                ("name", Value::is_string),
                ("package", Value::is_string),
                ("disabled", Value::is_boolean),
                ("compatibility", native_compatibility),
                ("capabilities", |v| {
                    v.as_object().is_some_and(|obj| {
                        obj.get("tools").is_some_and(Value::is_boolean)
                            && obj.get("input").is_some_and(native_string_array)
                            && obj.get("output").is_some_and(native_string_array)
                    })
                }),
                ("variants", |v| {
                    v.as_array().is_some_and(|variants| {
                        variants.iter().all(|variant| {
                            variant.get("id").is_some_and(Value::is_string)
                                && native_model_overlays(variant)
                        })
                    })
                }),
                ("cost", |v| match v.as_array() {
                    Some(costs) => costs.iter().all(native_cost),
                    None => native_cost(v),
                }),
                ("limit", |v| {
                    native_fields_valid(
                        v,
                        &[
                            ("context", native_integer),
                            ("input", native_integer),
                            ("output", native_integer),
                        ],
                    )
                }),
            ],
        )
}

fn native_compatibility(value: &Value) -> bool {
    native_fields_valid(
        value,
        &[
            ("reasoningField", Value::is_string),
            ("requireReasoning", Value::is_boolean),
            ("maxTokensField", |v| {
                matches!(v.as_str(), Some("max_completion_tokens" | "max_tokens"))
            }),
            ("requireFinishReason", Value::is_boolean),
            ("requireAssistantAfterTool", Value::is_boolean),
            ("supportsPromptCacheKey", Value::is_boolean),
        ],
    )
}

fn native_cost(value: &Value) -> bool {
    value.get("input").is_some_and(native_finite)
        && value.get("output").is_some_and(native_finite)
        && native_fields_valid(
            value,
            &[
                ("tier", |v| {
                    v.as_object().is_some_and(|obj| {
                        obj.get("type").and_then(Value::as_str) == Some("context")
                            && obj.get("size").is_some_and(native_integer)
                    })
                }),
                ("cache", |v| {
                    native_fields_valid(v, &[("read", native_finite), ("write", native_finite)])
                }),
            ],
        )
}

pub fn provider_format(
    value: &Value,
    source: Option<OpenCodeConfigFormat>,
) -> OpenCodeConfigFormat {
    if let Some(format) = source {
        return format;
    }
    if value.get("npm").is_some() || value.get("options").is_some() || value.get("api").is_some() {
        return OpenCodeConfigFormat::V1;
    }
    if ["package", "settings", "headers", "body", "canonical"]
        .iter()
        .any(|key| value.get(*key).is_some())
    {
        OpenCodeConfigFormat::V2
    } else {
        OpenCodeConfigFormat::V1
    }
}

pub fn set_provider(id: &str, config: Value) -> Result<(), AppError> {
    let format = provider_format(&config, None);
    set_provider_with_format(id, config, format)
}

pub fn set_provider_with_format(
    id: &str,
    config: Value,
    format: OpenCodeConfigFormat,
) -> Result<(), AppError> {
    let _guard = opencode_config_lock().lock()?;
    let path = get_opencode_config_path();
    let mut full_config = read_opencode_config_from_path(&path)?;
    if format == OpenCodeConfigFormat::V1
        && full_config
            .get("providers")
            .and_then(|providers| providers.get(id))
            .is_some_and(is_native_provider)
    {
        return Err(AppError::Config(format!(
            "OpenCode provider '{id}' has a native V2 declaration. Reload providers before editing it."
        )));
    }
    let key = match format {
        OpenCodeConfigFormat::V1 => "provider",
        OpenCodeConfigFormat::V2 => {
            if !is_native_provider(&config) {
                return Err(AppError::Config(format!(
                    "Invalid native OpenCode provider '{id}'"
                )));
            }
            "providers"
        }
    };

    // 判空要连「存在但不是对象」一起算：否则下面 as_object_mut 拿不到，
    // 写入会静默失效——界面显示添加成功而文件里没有。provider 段是 cc-switch
    // 的投影区，归一化不会碰用户自有的 model / theme 等顶层配置。
    if !full_config.get(key).is_some_and(Value::is_object) {
        if key == "providers" && full_config.get(key).is_some() {
            return Err(AppError::Config(format!(
                "OpenCode {key} must be an object"
            )));
        }
        if full_config.get(key).is_some() {
            log::warn!("opencode.json 的 {key} 不是对象，已重置为空对象");
        }
        full_config[key] = json!({});
    }

    if let Some(providers) = full_config.get_mut(key).and_then(|v| v.as_object_mut()) {
        providers.insert(id.to_string(), config);
    }

    write_opencode_config_to_path_with_contents(&path, &full_config).map(|_| ())
}

/// 从**每个**已存在的配置文件里删掉 `<section>.<id>`。
///
/// 只删最高优先级的那份是不够的：OpenCode 合并全部三个文件，低优先级文件里的同名
/// 条目会在删除后重新生效，界面显示已删除而 OpenCode 仍在用它。
///
/// 只回写真正发生变化的文件——未命中的文件保持原样，避免整份重新序列化把用户的
/// 注释和格式抹掉。
fn remove_entry_from_all_configs(section: &str, id: &str) -> Result<(), AppError> {
    let paths = existing_opencode_config_paths();

    if paths.is_empty() {
        // 与此前一致：没有任何配置文件时仍在目标路径落一份空配置。
        let path = get_opencode_config_path();
        let config = read_opencode_config_from_path(&path)?;
        return write_opencode_config_to_path_with_contents(&path, &config).map(|_| ());
    }

    for path in paths {
        let mut config = read_opencode_config_from_path(&path)?;

        let removed = match config.get_mut(section).and_then(|v| v.as_object_mut()) {
            Some(entries) => entries.remove(id).is_some(),
            None => {
                if config.get(section).is_some() {
                    log::warn!("{} 的 {section} 不是对象，无法删除 '{id}'", path.display());
                }
                false
            }
        };

        if removed {
            write_opencode_config_to_path_with_contents(&path, &config)?;
        }
    }

    Ok(())
}

pub fn remove_provider(id: &str) -> Result<(), AppError> {
    let _guard = opencode_config_lock().lock()?;
    // V1 声明在 `provider`，native V2 声明在 `providers`，两边都要清。
    remove_entry_from_all_configs("provider", id)?;
    remove_entry_from_all_configs("providers", id)
}

pub fn get_mcp_servers() -> Result<Map<String, Value>, AppError> {
    let config = read_opencode_config()?;
    Ok(config
        .get("mcp")
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default())
}

pub fn set_mcp_server(id: &str, config: Value) -> Result<(), AppError> {
    let _guard = opencode_config_lock().lock()?;
    let path = get_opencode_config_path();
    let mut full_config = read_opencode_config_from_path(&path)?;

    if !full_config.get("mcp").is_some_and(Value::is_object) {
        if full_config.get("mcp").is_some() {
            log::warn!("opencode.json 的 mcp 不是对象，已重置为空对象");
        }
        full_config["mcp"] = json!({});
    }

    if let Some(mcp) = full_config.get_mut("mcp").and_then(|v| v.as_object_mut()) {
        mcp.insert(id.to_string(), config);
    }

    write_opencode_config_to_path_with_contents(&path, &full_config).map(|_| ())
}

pub fn remove_mcp_server(id: &str) -> Result<(), AppError> {
    let _guard = opencode_config_lock().lock()?;
    remove_entry_from_all_configs("mcp", id)
}

pub fn add_plugin(path: &Path, plugin_name: &str) -> Result<(), AppError> {
    let _guard = opencode_config_lock().lock()?;
    let mut config = read_opencode_config_from_path(path)?;
    let normalized_plugin_name = canonicalize_plugin_name(plugin_name);
    let target_is_omo =
        matches_any_plugin_prefix(&normalized_plugin_name, &STANDARD_OMO_PLUGIN_PREFIXES)
            || matches_any_plugin_prefix(&normalized_plugin_name, &SLIM_OMO_PLUGIN_PREFIXES);
    let mut changed = false;

    let plugins = config.get_mut("plugin").and_then(|v| v.as_array_mut());

    match plugins {
        Some(arr) => {
            let mut found_target = false;
            arr.retain(|value| {
                let Some(existing_name) = value.as_str() else {
                    return true;
                };
                if existing_name == normalized_plugin_name {
                    if found_target {
                        changed = true;
                        return false;
                    }
                    found_target = true;
                    return true;
                }

                // Standard OMO and OMO Slim are mutually exclusive.
                if target_is_omo
                    && (matches_any_plugin_prefix(existing_name, &STANDARD_OMO_PLUGIN_PREFIXES)
                        || matches_any_plugin_prefix(existing_name, &SLIM_OMO_PLUGIN_PREFIXES))
                {
                    changed = true;
                    return false;
                }
                true
            });

            if !found_target {
                arr.push(Value::String(normalized_plugin_name));
                changed = true;
            }
        }
        None => {
            config["plugin"] = json!([normalized_plugin_name]);
            changed = true;
        }
    }

    if !changed {
        return Ok(());
    }

    write_opencode_config_to_path_with_contents(path, &config).map(|_| ())
}

pub fn remove_plugins_by_prefixes(path: &Path, prefixes: &[&str]) -> Result<bool, AppError> {
    let _guard = opencode_config_lock().lock()?;
    let previous_contents = read_config_contents(path)?;
    let mut config = read_opencode_config_from_path(path)?;

    let mut changed = false;
    if let Some(arr) = config.get_mut("plugin").and_then(|v| v.as_array_mut()) {
        let previous_len = arr.len();
        arr.retain(|v| {
            v.as_str()
                .map(|s| !matches_any_plugin_prefix(s, prefixes))
                .unwrap_or(true)
        });
        changed = arr.len() != previous_len;

        if changed && arr.is_empty() {
            config.as_object_mut().map(|obj| obj.remove("plugin"));
        }
    }

    if !changed {
        return Ok(false);
    }

    let current_contents = read_config_contents(path)?;
    if current_contents != previous_contents {
        return Err(AppError::Config(
            "OpenCode config changed on disk. Please reload and try again.".to_string(),
        ));
    }

    write_opencode_config_to_path_with_contents(path, &config)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestHomeGuard(Option<std::ffi::OsString>);
    impl TestHomeGuard {
        fn set(home: &std::path::Path) -> Self {
            let guard = Self(std::env::var_os("CC_SWITCH_TEST_HOME"));
            std::env::set_var("CC_SWITCH_TEST_HOME", home);
            guard
        }
    }
    impl Drop for TestHomeGuard {
        fn drop(&mut self) {
            match self.0.take() {
                Some(value) => std::env::set_var("CC_SWITCH_TEST_HOME", value),
                None => std::env::remove_var("CC_SWITCH_TEST_HOME"),
            }
        }
    }

    fn write_config(home: &std::path::Path, content: &str) {
        write_config_named(home, "opencode.json", content);
    }

    fn write_config_named(home: &std::path::Path, file_name: &str, content: &str) {
        let dir = home.join(".config").join("opencode");
        std::fs::create_dir_all(&dir).expect("create config dir");
        std::fs::write(dir.join(file_name), content).expect("write config");
    }

    #[test]
    #[serial_test::serial]
    fn config_path_prefers_existing_jsonc_over_json() {
        let temp = tempfile::tempdir().expect("tempdir");
        let _guard = TestHomeGuard::set(temp.path());

        write_config_named(temp.path(), "config.json", "{}");
        assert_eq!(
            get_opencode_config_path().file_name().unwrap(),
            "config.json",
            "legacy config.json is used when it is the only file present"
        );

        write_config_named(temp.path(), "opencode.json", "{}");
        assert_eq!(
            get_opencode_config_path().file_name().unwrap(),
            "opencode.json",
            "opencode.json outranks the legacy config.json"
        );

        write_config_named(temp.path(), "opencode.jsonc", "{}");
        assert_eq!(
            get_opencode_config_path().file_name().unwrap(),
            "opencode.jsonc",
            "opencode.jsonc outranks both, matching OpenCode's globalConfigFile()"
        );
    }

    #[test]
    #[serial_test::serial]
    fn config_path_falls_back_to_json_when_no_config_exists() {
        let temp = tempfile::tempdir().expect("tempdir");
        let _guard = TestHomeGuard::set(temp.path());

        assert_eq!(
            get_opencode_config_path().file_name().unwrap(),
            "opencode.json",
            "a fresh install keeps creating opencode.json"
        );
    }

    #[test]
    #[serial_test::serial]
    fn config_path_ignores_a_directory_named_like_a_config_file() {
        let temp = tempfile::tempdir().expect("tempdir");
        let _guard = TestHomeGuard::set(temp.path());

        let dir = temp.path().join(".config").join("opencode");
        std::fs::create_dir_all(dir.join("opencode.jsonc")).expect("create decoy dir");

        assert_eq!(
            get_opencode_config_path().file_name().unwrap(),
            "opencode.json",
            "a directory must not be mistaken for a config file"
        );
    }

    #[test]
    #[serial_test::serial]
    fn set_provider_updates_existing_jsonc_instead_of_creating_a_second_file() {
        let temp = tempfile::tempdir().expect("tempdir");
        let _guard = TestHomeGuard::set(temp.path());

        // OpenCode 新装时建的就是 opencode.jsonc，且允许注释。
        write_config_named(
            temp.path(),
            "opencode.jsonc",
            "{\n  // user config\n  \"model\": \"keep-me\"\n}",
        );

        set_provider("acme", json!({"npm": "@ai-sdk/openai-compatible"}))
            .expect("set_provider must succeed");

        let dir = temp.path().join(".config").join("opencode");
        assert!(
            !dir.join("opencode.json").exists(),
            "writing must not create a second config file next to opencode.jsonc"
        );

        let config = read_opencode_config().expect("reload");
        assert_eq!(
            config["provider"]["acme"]["npm"], "@ai-sdk/openai-compatible",
            "the provider must land in the file OpenCode actually reads last"
        );
        assert_eq!(
            config["model"], "keep-me",
            "unrelated user config must be preserved"
        );
    }

    #[test]
    #[serial_test::serial]
    fn read_sees_providers_declared_only_in_jsonc() {
        let temp = tempfile::tempdir().expect("tempdir");
        let _guard = TestHomeGuard::set(temp.path());

        write_config_named(
            temp.path(),
            "opencode.jsonc",
            "{\n  // trailing commas and comments are valid JSONC\n  \"provider\": {\n    \"acme\": { \"npm\": \"@ai-sdk/openai-compatible\" },\n  },\n}",
        );

        let providers = get_providers().expect("providers must load");
        assert!(
            providers.contains_key("acme"),
            "providers declared in opencode.jsonc must be visible"
        );
    }

    #[test]
    #[serial_test::serial]
    fn read_merges_every_config_file_the_way_opencode_does() {
        let temp = tempfile::tempdir().expect("tempdir");
        let _guard = TestHomeGuard::set(temp.path());

        // 常见的升级态：老版本 CC Switch 把供应商写进了 opencode.json，
        // 而 OpenCode 自己建了 opencode.jsonc。
        write_config_named(
            temp.path(),
            "config.json",
            r#"{"provider": {"legacy": {"npm": "old"}}, "model": "from-config-json"}"#,
        );
        write_config_named(
            temp.path(),
            "opencode.json",
            r#"{"provider": {"written-by-cc-switch": {"npm": "x"}}, "model": "from-opencode-json"}"#,
        );
        write_config_named(
            temp.path(),
            "opencode.jsonc",
            "{\n  // created by OpenCode\n  \"provider\": { \"hand-written\": { \"npm\": \"y\" } }\n}",
        );

        let providers = get_providers().expect("providers must load");
        let mut ids: Vec<&String> = providers.keys().collect();
        ids.sort();
        assert_eq!(
            ids,
            vec!["hand-written", "legacy", "written-by-cc-switch"],
            "providers from every config file must be visible"
        );

        let config = read_opencode_config().expect("read");
        assert_eq!(
            config["model"], "from-opencode-json",
            "later files win on conflicting scalars, as OpenCode's mergeDeep does"
        );
    }

    #[test]
    #[serial_test::serial]
    fn remove_provider_purges_every_config_file() {
        let temp = tempfile::tempdir().expect("tempdir");
        let _guard = TestHomeGuard::set(temp.path());

        write_config_named(
            temp.path(),
            "opencode.json",
            r#"{"provider": {"acme": {"npm": "stale"}, "keep": {"npm": "z"}}}"#,
        );
        write_config_named(
            temp.path(),
            "opencode.jsonc",
            r#"{"provider": {"acme": {"npm": "current"}}}"#,
        );

        remove_provider("acme").expect("remove must succeed");

        let providers = get_providers().expect("reload");
        assert!(
            !providers.contains_key("acme"),
            "a provider present in several files must not come back from a lower-priority one"
        );
        assert!(
            providers.contains_key("keep"),
            "unrelated providers must survive"
        );
    }

    #[test]
    #[serial_test::serial]
    fn remove_provider_leaves_files_without_that_entry_untouched() {
        let temp = tempfile::tempdir().expect("tempdir");
        let _guard = TestHomeGuard::set(temp.path());

        let untouched = "{\n  // hand-written, keep my comments\n  \"model\": \"m\",\n}";
        write_config_named(temp.path(), "opencode.jsonc", untouched);
        write_config_named(
            temp.path(),
            "opencode.json",
            r#"{"provider": {"acme": {"npm": "x"}}}"#,
        );

        remove_provider("acme").expect("remove must succeed");

        let dir = temp.path().join(".config").join("opencode");
        assert_eq!(
            std::fs::read_to_string(dir.join("opencode.jsonc")).unwrap(),
            untouched,
            "a file that does not hold the entry must not be rewritten"
        );
        assert!(
            !get_providers().unwrap().contains_key("acme"),
            "the entry must still be gone"
        );
    }

    #[test]
    fn merge_config_recurses_into_objects_and_replaces_everything_else() {
        let mut target = json!({
            "provider": {"a": {"npm": "1"}, "b": {"npm": "2"}},
            "instructions": ["one"],
            "model": "old",
        });
        merge_config_into(
            &mut target,
            json!({
                "provider": {"b": {"npm": "3"}, "c": {"npm": "4"}},
                "instructions": ["two"],
                "model": "new",
            }),
        );

        assert_eq!(
            target["provider"]["a"]["npm"], "1",
            "untouched key survives"
        );
        assert_eq!(target["provider"]["b"]["npm"], "3", "conflicting key wins");
        assert_eq!(target["provider"]["c"]["npm"], "4", "new key is added");
        assert_eq!(
            target["instructions"],
            json!(["two"]),
            "arrays are replaced, not concatenated, matching mergeDeep"
        );
        assert_eq!(target["model"], "new");
    }

    #[test]
    #[serial_test::serial]
    fn native_providers_take_precedence_independently_of_key_order() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = TestHomeGuard::set(temp.path());
        let legacy = r#""provider":{"shared":{"npm":"@ai-sdk/anthropic"},"legacy":{"npm":"@ai-sdk/openai"}}"#;
        let native = r#""providers":{"shared":{"settings":{"baseURL":"https://native.example"}},"builtin":{"models":{"alias":{"modelID":"upstream"}}}}"#;
        for text in [
            format!("{{{legacy},{native}}}"),
            format!("{{{native},{legacy}}}"),
        ] {
            write_config(temp.path(), &text);
            let providers = get_providers_with_format().unwrap();
            assert_eq!(providers.len(), 3);
            assert_eq!(providers["legacy"].1, OpenCodeConfigFormat::V1);
            assert_eq!(providers["builtin"].1, OpenCodeConfigFormat::V2);
            assert_eq!(
                providers["shared"].0,
                json!({"settings":{"baseURL":"https://native.example"}})
            );
        }
    }

    #[test]
    #[serial_test::serial]
    fn native_provider_write_and_remove_preserve_other_declarations() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = TestHomeGuard::set(temp.path());
        let original = json!({
            "model": "shared/model",
            "provider": {"shared": {"npm": "@ai-sdk/anthropic"}, "legacy": {"npm": "@ai-sdk/openai"}},
            "providers": {"shared": {}, "builtin": {"body": {"metadata": {"keep": true}}}},
            "mcp": {"servers": {"example": {"type": "remote", "url": "https://mcp.example"}}},
            "plugins": [{"package": "example", "options": {"keep": true}}]
        });
        write_config(temp.path(), &original.to_string());
        let native = json!({"models": {"model": {"variants": [
            {"id": "low", "settings": {"reasoningEffort": "low"}},
            {"id": "high", "body": {"reasoning": {"effort": "high"}}}
        ]}}});
        set_provider_with_format("shared", native.clone(), OpenCodeConfigFormat::V2).unwrap();
        let mut expected = original;
        expected["providers"]["shared"] = native;
        assert_eq!(read_opencode_config().unwrap(), expected);

        let before = std::fs::read(get_opencode_config_path()).unwrap();
        assert!(set_provider("shared", json!({"npm": "@ai-sdk/anthropic"})).is_err());
        assert!(set_provider_with_format(
            "shared",
            json!({"settings": []}),
            OpenCodeConfigFormat::V2
        )
        .is_err());
        assert_eq!(std::fs::read(get_opencode_config_path()).unwrap(), before);

        remove_provider("shared").unwrap();
        expected["providers"]
            .as_object_mut()
            .unwrap()
            .remove("shared");
        expected["provider"]
            .as_object_mut()
            .unwrap()
            .remove("shared");
        assert_eq!(read_opencode_config().unwrap(), expected);
        assert!(!get_providers().unwrap().contains_key("shared"));
    }

    #[test]
    #[serial_test::serial]
    fn malformed_native_provider_does_not_hide_legacy_or_rewrite_source() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = TestHomeGuard::set(temp.path());
        let original = r#"{
            "provider": {"shared": {"npm": "@ai-sdk/openai"}},
            "providers": {"shared": {"package": false}, "valid": {}}
        }"#;
        write_config(temp.path(), original);
        let providers = get_providers_with_format().unwrap();
        assert_eq!(providers["shared"].1, OpenCodeConfigFormat::V1);
        assert_eq!(providers["valid"].1, OpenCodeConfigFormat::V2);
        assert_eq!(
            std::fs::read_to_string(get_opencode_config_path()).unwrap(),
            original
        );
        let updated = json!({"npm": "@ai-sdk/openai", "options": {"apiKey": "fake-new"}});
        set_provider_with_format("shared", updated.clone(), OpenCodeConfigFormat::V1).unwrap();
        let mut expected: Value = serde_json::from_str(original).unwrap();
        expected["provider"]["shared"] = updated;
        assert_eq!(read_opencode_config().unwrap(), expected);
    }

    #[test]
    #[serial_test::serial]
    fn malformed_native_nested_fields_fall_back_to_legacy() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = TestHomeGuard::set(temp.path());
        let legacy = json!({"npm": "@ai-sdk/openai", "options": {"apiKey": "fake-old"}});
        let mut invalid = vec![
            json!({"models": {"m": {"variants": {}}}}),
            json!({"settings": {"timeout": "1000"}}),
            json!({"settings": {"timeout": true}}),
            json!({"settings": {"timeout": null}}),
            json!({"settings": {"chunkTimeout": false}}),
            json!({"settings": {"compaction": {}}}),
            json!({"settings": {"compaction": {"type": "unknown"}}}),
            json!({"settings": {"transport": "sse"}}),
            json!({"headers": {"X-Tenant": 1}}),
            json!({"env": [1]}),
        ];
        for model in [
            json!(null),
            json!({"modelID": false}),
            json!({"family": []}),
            json!({"name": null}),
            json!({"package": {}}),
            json!({"disabled": "false"}),
            json!({"settings": null}),
            json!({"settings": {"compaction": "native"}}),
            json!({"headers": {"X-Tenant": false}}),
            json!({"body": []}),
            json!({"variants": [null]}),
            json!({"variants": [{}]}),
            json!({"variants": [{"id": 1}]}),
            json!({"variants": [{"id": "low", "settings": {"compaction": {"type": false}}}]}),
            json!({"variants": [{"id": "low", "headers": {"X-Tenant": null}}]}),
            json!({"variants": [{"id": "low", "body": []}]}),
            json!({"limit": []}),
            json!({"limit": {"context": "1000"}}),
            json!({"limit": {"input": 1.5}}),
            json!({"limit": {"output": null}}),
            json!({"limit": {"context": 9007199254740992_u64}}),
            json!({"capabilities": {"tools": true}}),
            json!({"capabilities": {"tools": "true", "input": [], "output": []}}),
            json!({"capabilities": {"tools": true, "input": "text", "output": []}}),
            json!({"capabilities": {"tools": true, "input": [], "output": [false]}}),
            json!({"compatibility": false}),
            json!({"compatibility": {"reasoningField": false}}),
            json!({"compatibility": {"maxTokensField": "tokens"}}),
            json!({"cost": {}}),
            json!({"cost": [{"input": 1}]}),
            json!({"cost": {"input": "1", "output": 2}}),
            json!({"cost": {"input": 1, "output": 2, "cache": {"read": false}}}),
            json!({"cost": {"input": 1, "output": 2, "cache": {"write": null}}}),
            json!({"cost": {"input": 1, "output": 2, "tier": {"type": "context"}}}),
            json!({"cost": {"input": 1, "output": 2, "tier": {"type": "other", "size": 10}}}),
            json!({"cost": {"input": 1, "output": 2, "tier": {"type": "context", "size": 1.5}}}),
        ] {
            invalid.push(json!({"models": {"m": model}}));
        }
        for key in [
            "requireReasoning",
            "requireFinishReason",
            "requireAssistantAfterTool",
            "supportsPromptCacheKey",
        ] {
            invalid.push(json!({"models": {"m": {"compatibility": {key: "true"}}}}));
        }
        for native in invalid {
            let original = json!({
                "provider": {"shared": legacy},
                "providers": {"shared": native, "native-only": native}
            });
            let source = original.to_string();
            write_config(temp.path(), &source);
            let providers = get_providers_with_format().unwrap();
            assert_eq!(providers.len(), 1, "{native}");
            assert_eq!(
                providers["shared"],
                (legacy.clone(), OpenCodeConfigFormat::V1),
                "{native}"
            );
            assert!(
                set_provider_with_format("shared", native.clone(), OpenCodeConfigFormat::V2)
                    .is_err(),
                "{native}"
            );
            assert_eq!(
                std::fs::read_to_string(get_opencode_config_path()).unwrap(),
                source
            );
            let updated = json!({"npm": "@ai-sdk/openai", "options": {"apiKey": "fake-new"}});
            set_provider_with_format("shared", updated.clone(), OpenCodeConfigFormat::V1).unwrap();
            let mut expected = original;
            expected["provider"]["shared"] = updated;
            assert_eq!(read_opencode_config().unwrap(), expected);
        }
    }

    #[test]
    #[serial_test::serial]
    fn native_nested_fields_and_unknown_extensions_roundtrip() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = TestHomeGuard::set(temp.path());
        let native = json!({
            "name": "Native", "package": "@opencode/ai/providers/openai", "canonical": "openai",
            "env": ["TEST_API_KEY"], "extension": [null, {"keep": true}],
            "settings": {"timeout": false, "chunkTimeout": 1.5, "transport": "websocket",
                "compaction": {"type": "native", "extension": true}, "custom": {"keep": null}},
            "headers": {"X-Tenant": "example"}, "body": {"metadata": {"keep": true}},
            "models": {"m": {
                "modelID": "upstream", "family": "gpt", "name": "Model", "package": "custom",
                "disabled": false, "extension": {"keep": [1, 2]},
                "settings": {"compaction": {"type": "summary"}, "timeout": "package-specific"},
                "headers": {"X-Model": "example"}, "body": {"custom": [null]},
                "limit": {"context": 1000.0, "input": -1, "output": 0, "extension": true},
                "capabilities": {"tools": false, "input": ["text", "custom"], "output": [], "extension": []},
                "compatibility": {"reasoningField": "custom", "requireReasoning": true,
                    "maxTokensField": "max_tokens", "requireFinishReason": false,
                    "requireAssistantAfterTool": true, "supportsPromptCacheKey": false, "extension": null},
                "cost": [{"input": 1.5, "output": 2, "cache": {"read": 0.5, "extension": true},
                    "tier": {"type": "context", "size": 1000, "extension": null}, "extension": []}],
                "variants": [
                    {"id": "high", "settings": {"compaction": {"type": "native"}, "transport": "custom"},
                        "headers": {"X-Variant": "example"}, "body": {"custom": true}, "extension": null},
                    {"id": "low"}
                ]
            }}
        });
        for value in [
            native,
            json!({}),
            json!({"settings": {"timeout": 1000.5, "transport": "http"}}),
            json!({"models": {"m": {"cost": {"input": 0, "output": 0, "cache": {}}, "variants": []}}}),
            json!({"models": {"m": {"cost": [], "limit": {}, "compatibility": {"maxTokensField": "max_completion_tokens"}}}}),
        ] {
            let mut expected = json!({"provider": {"shared": {"npm": "@ai-sdk/openai"}}});
            write_config(temp.path(), &expected.to_string());
            set_provider_with_format("shared", value.clone(), OpenCodeConfigFormat::V2).unwrap();
            expected["providers"] = json!({"shared": value});
            assert_eq!(read_opencode_config().unwrap(), expected);
            assert_eq!(
                get_providers_with_format().unwrap()["shared"],
                (value, OpenCodeConfigFormat::V2)
            );
            assert!(set_provider_with_format(
                "shared",
                json!({"npm": "@ai-sdk/openai"}),
                OpenCodeConfigFormat::V1
            )
            .is_err());
            assert_eq!(read_opencode_config().unwrap(), expected);
        }
    }

    #[test]
    #[serial_test::serial]
    fn read_rejects_non_object_root_instead_of_panicking_downstream() {
        let temp = tempfile::tempdir().expect("tempdir");
        let _guard = TestHomeGuard::set(temp.path());

        // 顶层数组/标量会让下游 `config["provider"] = …` 触发 serde_json panic。
        // 顶层 null 例外——serde_json 会把它自动升级成对象，本来就不炸。
        for malformed in ["[]", "[{\"a\":1}]", "42", "\"oops\""] {
            write_config(temp.path(), malformed);
            let result = read_opencode_config();
            assert!(
                result.is_err(),
                "non-object root must be rejected: {malformed}"
            );
        }

        write_config(temp.path(), "{\"model\": \"x\"}");
        assert!(
            read_opencode_config().is_ok(),
            "a normal object config must still load"
        );
    }

    #[test]
    #[serial_test::serial]
    fn set_mcp_server_normalizes_non_object_section() {
        let temp = tempfile::tempdir().expect("tempdir");
        let _guard = TestHomeGuard::set(temp.path());

        // `"mcp": []` 时旧代码的 as_object_mut 返回 None → 写入静默失效
        write_config(temp.path(), "{\"model\": \"keep-me\", \"mcp\": []}");

        set_mcp_server("echo", json!({"command": "npx"})).expect("set must succeed");

        let config = read_opencode_config().expect("reload");
        assert_eq!(
            config["mcp"]["echo"]["command"], "npx",
            "server must actually be written"
        );
        assert_eq!(
            config["model"], "keep-me",
            "unrelated user config must be preserved"
        );
    }

    #[test]
    fn remove_missing_plugin_does_not_create_config_file() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("opencode.json");

        let result = remove_plugins_by_prefixes(&path, &["oh-my-openagent"]).unwrap();

        assert!(!result);
        assert!(!path.exists());
    }

    #[test]
    fn remove_missing_plugin_preserves_existing_source() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("opencode.json");
        let original = r#"{
  // Keep formatting when the target plugin is absent.
  "plugin": ["unrelated-plugin"],
  "theme": "dark",
}"#;
        std::fs::write(&path, original).unwrap();

        let result = remove_plugins_by_prefixes(&path, &["oh-my-openagent"]).unwrap();

        assert!(!result);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    }

    #[test]
    fn add_existing_plugin_preserves_existing_source() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("opencode.json");
        let original = r#"{
  // Keep comments and formatting when the plugin is already configured.
  plugin: ['oh-my-openagent@latest'],
  theme: 'dark',
}"#;
        std::fs::write(&path, original).unwrap();

        add_plugin(&path, "oh-my-openagent@latest").unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    }
}

#[cfg(test)]
mod tests_jsonc {
    use super::*;
    use serial_test::serial;
    use std::fs;
    use tempfile::TempDir;

    /// RAII helper that points `get_home_dir()` at a temp directory by
    /// setting `CC_SWITCH_TEST_HOME` (the only env var the codebase honors —
    /// see `config.rs` `get_home_dir`; `HOME` is intentionally *not* used
    /// because it's unreliable on Windows). Saves and restores the prior
    /// value via `OsString` so non-UTF-8 paths survive the round-trip.
    struct TempHome {
        dir: TempDir,
        original_test_home: Option<std::ffi::OsString>,
    }

    impl TempHome {
        fn new() -> Self {
            let dir = TempDir::new().expect("failed to create temp home");
            let original_test_home = std::env::var_os("CC_SWITCH_TEST_HOME");
            std::env::set_var("CC_SWITCH_TEST_HOME", dir.path());
            Self {
                dir,
                original_test_home,
            }
        }
    }

    impl Drop for TempHome {
        fn drop(&mut self) {
            match &self.original_test_home {
                Some(value) => std::env::set_var("CC_SWITCH_TEST_HOME", value),
                None => std::env::remove_var("CC_SWITCH_TEST_HOME"),
            }
        }
    }

    fn setup_test_env() -> (TempHome, PathBuf) {
        let home = TempHome::new();
        let opencode_dir = home.dir.path().join(".config").join("opencode");
        fs::create_dir_all(&opencode_dir).unwrap();
        (home, opencode_dir)
    }

    #[test]
    #[serial]
    fn test_read_opencode_config_prefers_jsonc() {
        let (_home, opencode_dir) = setup_test_env();

        let json_path = opencode_dir.join("opencode.json");
        let jsonc_path = opencode_dir.join("opencode.jsonc");

        fs::write(&json_path, r#"{"provider": {"test-json": {}}}"#).unwrap();
        fs::write(&jsonc_path, r#"{"provider": {"test-jsonc": {}}}"#).unwrap();

        let path = get_opencode_config_path();
        assert!(path.ends_with("opencode.jsonc"));
    }

    #[test]
    #[serial]
    fn test_read_opencode_config_fallback_to_json() {
        let (_home, opencode_dir) = setup_test_env();

        let json_path = opencode_dir.join("opencode.json");
        fs::write(&json_path, r#"{"provider": {"test": {}}}"#).unwrap();

        let path = get_opencode_config_path();
        assert!(path.ends_with("opencode.json"));
    }

    #[test]
    #[serial]
    fn test_read_opencode_config_with_comments() {
        let (_home, opencode_dir) = setup_test_env();

        let jsonc_path = opencode_dir.join("opencode.jsonc");
        let config_with_comments = r#"{
            // This is a comment
            "$schema": "https://opencode.ai/config.json",
            /* Multi-line
               comment */
            "provider": {
                "test-provider": {
                    "apiKey": "test-key" // Inline comment
                }
            }
        }"#;
        fs::write(&jsonc_path, config_with_comments).unwrap();

        let config = read_opencode_config().unwrap();
        assert!(config.get("provider").is_some());
        assert!(config["provider"]
            .get("test-provider")
            .and_then(|p| p.get("apiKey"))
            .and_then(|k| k.as_str())
            == Some("test-key"));
    }

    #[test]
    #[serial]
    fn test_read_opencode_config_trailing_commas() {
        let (_home, opencode_dir) = setup_test_env();

        let jsonc_path = opencode_dir.join("opencode.jsonc");
        let config_with_trailing = r#"{
            "$schema": "https://opencode.ai/config.json",
            "provider": {
                "test-provider": {
                    "apiKey": "test-key",
                    "baseUrl": "https://api.example.com",
                },
            },
        }"#;
        fs::write(&jsonc_path, config_with_trailing).unwrap();

        let config = read_opencode_config().unwrap();
        assert!(config.get("provider").is_some());
    }
}
