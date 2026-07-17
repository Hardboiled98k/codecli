// SPDX-License-Identifier: MPL-2.0
//! Provider / 模型方案：多套保存、切换、回滚（Key 只进 secrets.env）

use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::Read;

use super::config::{apply_config_without_scheme_record, ConfigApplyRequest, ConfigApplyResult};
use super::connectivity::{test_connectivity_sync, ConnectivityRequest};
use super::op_lock::with_op_lock;
use super::platform::{codecli_state_dir, get_persistent_env_strict};
use super::providers::find_provider;
use super::util::{
    atomic_write, atomic_write_mode, chrono_like_now, mask_key, validate_base_url,
    validate_env_value,
};

const SCHEME_TX_VERSION: u8 = 2;
const SCHEME_STORE_TX_VERSION: u8 = 1;
const MAX_SCHEME_TX_BYTES: u64 = 512 * 1024;
const MAX_OWNERSHIP_FINGERPRINT_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Scheme {
    pub id: String,
    pub name: String,
    /// claude | codex
    pub target: String,
    pub provider_id: String,
    pub base_url: String,
    pub model: Option<String>,
    /// 界面仅显示尾号
    pub api_key_masked: String,
    /// secrets.env 里的键名，如 SCHEME_a1b2_KEY
    pub secret_key: String,
    pub updated_at: String,
    pub last_verified_ok: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct SchemeStore {
    active_claude: Option<String>,
    active_codex: Option<String>,
    schemes: Vec<Scheme>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum SchemeTxPhase {
    Apply,
    Commit,
    Clear,
    Abort,
}

/// 仅记录本工具拥有字段的 SHA-256，不把旧 CLI Key 再复制进 journal。
/// `ownership_digest` 用于确认 CLI 成功后内部恢复元数据未被外部改写。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct CliOwnedState {
    target: String,
    fields: BTreeMap<String, Option<String>>,
    ownership_digest: Option<String>,
}

/// 跨 schemes.json / secrets.env / CLI 配置的可恢复前滚日志。
/// 文件含 API Key，必须严格 0600；完成或回滚后立即删除。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SchemeTxJournal {
    version: u8,
    phase: SchemeTxPhase,
    scheme_id: String,
    secret_key: String,
    previous_secret: Option<String>,
    original_store: SchemeStore,
    candidate_store: SchemeStore,
    request: ConfigApplyRequest,
    cli_before: CliOwnedState,
    cli_after: Option<CliOwnedState>,
    #[serde(default)]
    clear_target: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum SchemeStoreTxKind {
    Upsert,
    Delete,
}

/// 不改 CLI 的保存/删除也跨越 schemes.json 与 secrets.env。单独的
/// durable 前滚日志避免“rename 已发生但目录 fsync 报错”时误回滚 Key，
/// 从而把两个资源永久拆开。文件同样含 Key，必须严格 0600。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SchemeStoreTxJournal {
    version: u8,
    kind: SchemeStoreTxKind,
    scheme_id: String,
    secret_key: String,
    previous_secret: Option<String>,
    candidate_secret: Option<String>,
    original_store: SchemeStore,
    candidate_store: SchemeStore,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SchemeListResult {
    pub ok: bool,
    pub active_claude: Option<String>,
    pub active_codex: Option<String>,
    pub schemes: Vec<Scheme>,
    pub message: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SchemeUpsertRequest {
    /// 更新时传 id；新建留空
    pub id: Option<String>,
    pub name: Option<String>,
    pub target: String,
    pub provider_id: String,
    pub api_key: String,
    pub base_url: Option<String>,
    pub model: Option<String>,
    /// true=写入本机并设为当前方案
    pub apply: Option<bool>,
}

fn schemes_path() -> Option<std::path::PathBuf> {
    codecli_state_dir().map(|d| d.join("schemes.json"))
}

fn schemes_tx_path() -> Option<std::path::PathBuf> {
    codecli_state_dir().map(|dir| dir.join("schemes.tx.json"))
}

fn schemes_store_tx_path() -> Option<std::path::PathBuf> {
    codecli_state_dir().map(|dir| dir.join("schemes.store.tx.json"))
}

fn validate_state_and_schemes_file() -> Result<Option<std::path::PathBuf>, String> {
    let Some(path) = schemes_path() else {
        return Ok(None);
    };
    if let Some(dir) = path.parent() {
        match std::fs::symlink_metadata(dir) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err("方案状态目录是符号链接，已拒绝操作".into())
            }
            Ok(meta) if !meta.is_dir() => return Err("方案状态路径不是目录".into()),
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("检查方案状态目录失败: {error}")),
        }
    }
    match std::fs::symlink_metadata(&path) {
        Ok(meta) if meta.file_type().is_symlink() || !meta.is_file() => {
            Err("schemes.json 不是可信普通文件，已拒绝操作".into())
        }
        Ok(_) => Ok(Some(path)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Some(path)),
        Err(error) => Err(format!("检查 schemes.json 失败: {error}")),
    }
}

fn load_store() -> Result<SchemeStore, String> {
    let Some(path) = validate_state_and_schemes_file()? else {
        return Ok(SchemeStore::default());
    };
    if !path.exists() {
        return Ok(SchemeStore::default());
    }
    let raw = std::fs::read_to_string(&path).map_err(|e| format!("读 schemes.json 失败: {}", e))?;
    if raw.trim().is_empty() {
        return Ok(SchemeStore::default());
    }
    let store: SchemeStore = serde_json::from_str(&raw).map_err(|e| {
        format!(
            "schemes.json 损坏，已中止以免覆盖: {}（路径 {}）",
            e,
            path.display()
        )
    })?;
    validate_store_integrity(&store)?;
    Ok(store)
}

fn save_store(store: &SchemeStore) -> Result<(), String> {
    validate_store_integrity(store)?;
    let path = validate_state_and_schemes_file()?.ok_or("找不到状态目录")?;
    let dir = path.parent().ok_or("方案状态路径无父目录")?;
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let body = serde_json::to_string_pretty(store).map_err(|e| e.to_string())?;
    atomic_write(&path, &body)
}

fn restore_secret_value(key: &str, previous: Option<&str>) -> Result<(), String> {
    match previous {
        Some(value) => super::platform::set_user_env(key, value),
        None => super::platform::unset_user_env(key),
    }
}

fn new_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let d = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    // 固定前缀，便于校验
    format!("sch_{:x}_{:09}", d.as_secs(), d.subsec_nanos())
}

fn validate_scheme_id(id: &str) -> Result<(), String> {
    let id = id.trim();
    if id.is_empty() || id.len() > 80 {
        return Err("schemeId 非法".into());
    }
    if !id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err("schemeId 含非法字符".into());
    }
    Ok(())
}

fn default_name(target: &str, provider_id: &str, model: Option<&str>) -> String {
    let m = model.unwrap_or("");
    if m.is_empty() {
        format!("{} · {}", target, provider_id)
    } else {
        format!("{} · {} · {}", target, provider_id, m)
    }
}

fn secret_key_for(id: &str) -> String {
    // 仅 ASCII 安全字符，方便 secrets.env 解析
    let safe: String = id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    format!("SCHEME_{}_KEY", safe.to_uppercase())
}

fn normalized_model(model: Option<&str>) -> Option<&str> {
    model.map(str::trim).filter(|value| !value.is_empty())
}

fn same_endpoint(
    scheme: &Scheme,
    target: &str,
    provider_id: &str,
    base_url: &str,
    model: Option<&str>,
) -> bool {
    scheme.target == target
        && scheme.provider_id == provider_id
        && scheme.base_url == base_url
        && normalized_model(scheme.model.as_deref()) == normalized_model(model)
}

fn resolve_existing_upsert_id(
    store: &SchemeStore,
    requested_id: Option<&str>,
    endpoint_match: Option<usize>,
) -> Result<Option<String>, String> {
    match (requested_id, endpoint_match) {
        (Some(id), Some(position)) if store.schemes[position].id != id => {
            Err("相同 target/provider/Base URL/模型已有方案，不能创建重复密钥记录".into())
        }
        (Some(id), _) => Ok(Some(id.to_string())),
        (None, Some(_)) => Err(
            "新建方案与已有 target/provider/Base URL/模型重复；请编辑原方案，不会静默覆盖其 Key"
                .into(),
        ),
        (None, None) => Ok(None),
    }
}

fn validate_inactive_save_policy(
    store: &SchemeStore,
    requested_id: Option<&str>,
    apply: bool,
) -> Result<(), String> {
    let updates_active = requested_id.is_some_and(|id| {
        store.active_claude.as_deref() == Some(id) || store.active_codex.as_deref() == Some(id)
    });
    if !apply && updates_active {
        return Err(
            "当前生效方案不能只更新不应用，否则列表状态会与真实 CLI 配置不一致；请选择「保存并应用」"
                .into(),
        );
    }
    Ok(())
}

fn validate_existing_scheme_target(
    existing: &Scheme,
    requested_target: &str,
) -> Result<(), String> {
    if existing.target != requested_target {
        return Err("现有方案不能直接变更 target；请新建目标方案，确认切换后再删除旧方案".into());
    }
    Ok(())
}

fn validate_provider_target(provider_id: &str, target: &str) -> Result<(), String> {
    let Some(provider) = find_provider(provider_id) else {
        // 为未来远程/自定义 provider id 保留兼容；真正的协议仍由目标端点
        // 连通测试和 apply_config 做最终校验。
        return Ok(());
    };
    if (target == "claude" && provider.protocol == "openai")
        || (target == "codex" && provider.protocol == "anthropic")
    {
        let protocol_name = if provider.protocol == "openai" {
            "OpenAI"
        } else {
            "Anthropic"
        };
        return Err(format!(
            "服务商「{}」的 {} 协议不能用于 {}",
            provider.name, protocol_name, target
        ));
    }
    Ok(())
}

/// schemes.json 是动态 secret 键的唯一可追踪元数据。读写时都先校验，
/// 避免 ID 映射碰撞、重复 endpoint 或伪造 secret_key 造成覆盖/误删 Key。
fn validate_store_integrity(store: &SchemeStore) -> Result<(), String> {
    use std::collections::BTreeSet;

    let mut ids = BTreeSet::new();
    let mut secret_keys = BTreeSet::new();
    let mut endpoints = BTreeSet::new();
    for scheme in &store.schemes {
        validate_scheme_id(&scheme.id)?;
        if scheme.target != "claude" && scheme.target != "codex" {
            return Err(format!("方案「{}」的 target 非法", scheme.name));
        }
        if !ids.insert(scheme.id.clone()) {
            return Err(format!("发现重复 schemeId: {}", scheme.id));
        }
        let expected = secret_key_for(&scheme.id);
        if scheme.secret_key != expected {
            return Err(format!(
                "方案「{}」的 secret_key 与 id 不匹配，已拒绝操作",
                scheme.name
            ));
        }
        if !secret_keys.insert(expected.clone()) {
            return Err(format!("方案 ID 映射到重复密钥名 {expected}，已拒绝操作"));
        }
        let endpoint = (
            scheme.target.clone(),
            scheme.provider_id.clone(),
            scheme.base_url.clone(),
            normalized_model(scheme.model.as_deref())
                .unwrap_or_default()
                .to_string(),
        );
        if !endpoints.insert(endpoint) {
            return Err(format!(
                "发现重复方案 endpoint（{} / {} / {}），已拒绝覆盖 Key",
                scheme.target, scheme.provider_id, scheme.base_url
            ));
        }
    }

    for (label, active, target) in [
        ("activeClaude", store.active_claude.as_deref(), "claude"),
        ("activeCodex", store.active_codex.as_deref(), "codex"),
    ] {
        if let Some(id) = active {
            let Some(scheme) = store.schemes.iter().find(|scheme| scheme.id == id) else {
                return Err(format!("{label} 指向不存在的方案: {id}"));
            };
            if scheme.target != target {
                return Err(format!("{label} 指向错误 target 的方案: {id}"));
            }
        }
    }
    Ok(())
}

fn rollback_store_and_secret(
    original_store: &SchemeStore,
    key: &str,
    previous_secret: Option<&str>,
) -> Result<(), String> {
    let secret_error = restore_secret_value(key, previous_secret).err();
    let store_error = save_store(original_store).err();
    match (secret_error, store_error) {
        (None, None) => Ok(()),
        (Some(secret), None) => Err(format!("方案 Key 回滚失败: {secret}")),
        (None, Some(store)) => Err(format!("方案元数据回滚失败: {store}")),
        (Some(secret), Some(store)) => Err(format!(
            "方案 Key 回滚失败: {secret}；元数据回滚失败: {store}"
        )),
    }
}

fn digest_tagged(tag: &str, value: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(tag.as_bytes());
    hasher.update([0]);
    hasher.update(value);
    hex::encode(hasher.finalize())
}

fn digest_string(value: &str) -> String {
    digest_tagged("string", value.as_bytes())
}

fn json_owned_value(value: Option<&serde_json::Value>) -> Option<String> {
    value.map(|value| {
        if let Some(text) = value.as_str() {
            digest_string(text)
        } else {
            digest_tagged(
                "json-non-string",
                &serde_json::to_vec(value).unwrap_or_default(),
            )
        }
    })
}

fn toml_owned_value(item: Option<&toml_edit::Item>) -> Option<String> {
    item.map(|item| {
        if let Some(text) = item.as_str() {
            digest_string(text)
        } else {
            digest_tagged("toml-non-string", item.to_string().as_bytes())
        }
    })
}

fn ownership_digest() -> Result<Option<String>, String> {
    let Some(path) = codecli_state_dir().map(|dir| dir.join("ownership.json")) else {
        return Ok(None);
    };
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("检查 ownership.json 指纹失败: {error}")),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err("ownership.json 不是可信普通文件，无法建立事务指纹".into());
    }
    if metadata.len() > MAX_OWNERSHIP_FINGERPRINT_BYTES {
        return Err("ownership.json 超过 1 MiB，无法建立事务指纹".into());
    }
    let bytes =
        std::fs::read(&path).map_err(|error| format!("读取 ownership.json 失败: {error}"))?;
    if bytes.len() as u64 > MAX_OWNERSHIP_FINGERPRINT_BYTES {
        return Err("ownership.json 读取期间变大，已拒绝".into());
    }
    Ok(Some(digest_tagged("ownership", &bytes)))
}

fn capture_cli_owned_state(target: &str) -> Result<CliOwnedState, String> {
    let mut fields = BTreeMap::new();
    match target {
        "claude" => {
            let path = super::platform::claude_config_dir()
                .ok_or("找不到 Claude 配置目录")?
                .join("settings.json");
            super::config::reject_top_level_config_link(&path, "~/.claude/settings.json")?;
            let root = match std::fs::read_to_string(&path) {
                Ok(raw) if raw.trim().is_empty() => serde_json::json!({}),
                Ok(raw) => serde_json::from_str::<serde_json::Value>(&raw).map_err(|error| {
                    format!("Claude settings.json 解析失败，已中止方案事务: {error}")
                })?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    serde_json::json!({})
                }
                Err(error) => return Err(format!("读取 Claude 配置指纹失败: {error}")),
            };
            let object = root
                .as_object()
                .ok_or("Claude settings.json 根节点不是对象，无法建立事务指纹")?;
            let env = match object.get("env") {
                Some(value) => Some(
                    value
                        .as_object()
                        .ok_or("Claude settings.json env 不是对象，无法建立事务指纹")?,
                ),
                None => None,
            };
            for key in [
                "ANTHROPIC_BASE_URL",
                "ANTHROPIC_API_KEY",
                "ANTHROPIC_AUTH_TOKEN",
                "ANTHROPIC_MODEL",
            ] {
                fields.insert(
                    format!("settings.env.{key}"),
                    json_owned_value(env.and_then(|env| env.get(key))),
                );
                fields.insert(
                    format!("persistent.{key}"),
                    get_persistent_env_strict(key)?
                        .as_deref()
                        .map(digest_string),
                );
            }
        }
        "codex" => {
            let path = super::platform::codex_config_toml().ok_or("找不到 Codex 配置路径")?;
            super::config::reject_top_level_config_link(&path, "~/.codex/config.toml")?;
            let document = match std::fs::read_to_string(&path) {
                Ok(raw) if raw.trim().is_empty() => toml_edit::DocumentMut::new(),
                Ok(raw) => raw
                    .parse::<toml_edit::DocumentMut>()
                    .map_err(|error| format!("无法为 Codex 配置建立事务指纹: {error}"))?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    toml_edit::DocumentMut::new()
                }
                Err(error) => return Err(format!("读取 Codex 配置指纹失败: {error}")),
            };
            fields.insert(
                "config.model".into(),
                toml_owned_value(document.get("model")),
            );
            fields.insert(
                "config.model_provider".into(),
                toml_owned_value(document.get("model_provider")),
            );
            let provider = document
                .get("model_providers")
                .and_then(toml_edit::Item::as_table)
                .and_then(|table| table.get("codecli_installer"))
                .and_then(toml_edit::Item::as_table);
            for key in ["name", "base_url", "env_key", "wire_api"] {
                fields.insert(
                    format!("config.provider.{key}"),
                    toml_owned_value(provider.and_then(|table| table.get(key))),
                );
            }
            for key in ["OPENAI_API_KEY", "OPENAI_BASE_URL"] {
                fields.insert(
                    format!("persistent.{key}"),
                    get_persistent_env_strict(key)?
                        .as_deref()
                        .map(digest_string),
                );
            }
        }
        _ => return Err("事务指纹 target 非法".into()),
    }
    Ok(CliOwnedState {
        target: target.to_string(),
        fields,
        ownership_digest: ownership_digest()?,
    })
}

fn expected_cli_fields(
    request: &ConfigApplyRequest,
) -> Result<BTreeMap<String, Option<String>>, String> {
    let base_url = validate_base_url(request.base_url.as_deref().ok_or("方案事务缺少 Base URL")?)?;
    let model = normalized_model(request.model.as_deref()).ok_or("方案事务缺少模型")?;
    let mut fields = BTreeMap::new();
    match request.target.as_str() {
        "claude" => {
            for (key, value) in [
                ("ANTHROPIC_BASE_URL", base_url.as_str()),
                ("ANTHROPIC_API_KEY", request.api_key.as_str()),
                ("ANTHROPIC_AUTH_TOKEN", request.api_key.as_str()),
                ("ANTHROPIC_MODEL", model),
            ] {
                let digest = Some(digest_string(value));
                fields.insert(format!("settings.env.{key}"), digest.clone());
                fields.insert(format!("persistent.{key}"), digest);
            }
        }
        "codex" => {
            for (key, value) in [
                ("config.model", model),
                ("config.model_provider", "codecli_installer"),
                ("config.provider.name", "CodeCLI Installer"),
                ("config.provider.base_url", base_url.as_str()),
                ("config.provider.env_key", "OPENAI_API_KEY"),
                ("config.provider.wire_api", "responses"),
                ("persistent.OPENAI_API_KEY", request.api_key.as_str()),
                ("persistent.OPENAI_BASE_URL", base_url.as_str()),
            ] {
                fields.insert(key.to_string(), Some(digest_string(value)));
            }
        }
        _ => return Err("方案事务 target 非法".into()),
    }
    Ok(fields)
}

fn cli_state_is_forward_compatible(
    before: &CliOwnedState,
    current: &CliOwnedState,
    request: &ConfigApplyRequest,
) -> Result<bool, String> {
    if before.target != request.target || current.target != request.target {
        return Ok(false);
    }
    let expected = expected_cli_fields(request)?;
    if before.fields.keys().ne(expected.keys()) || current.fields.keys().ne(expected.keys()) {
        return Ok(false);
    }
    Ok(current.fields.iter().all(|(key, value)| {
        before.fields.get(key) == Some(value) || expected.get(key) == Some(value)
    }))
}

fn cli_state_matches_request(
    current: &CliOwnedState,
    request: &ConfigApplyRequest,
) -> Result<bool, String> {
    Ok(current.target == request.target && current.fields == expected_cli_fields(request)?)
}

fn validate_scheme_tx(journal: &SchemeTxJournal) -> Result<(), String> {
    if journal.version != SCHEME_TX_VERSION {
        return Err(format!("不支持的方案事务版本: {}", journal.version));
    }
    validate_store_integrity(&journal.original_store)?;
    validate_store_integrity(&journal.candidate_store)?;
    validate_scheme_id(&journal.scheme_id)?;
    let scheme = journal
        .candidate_store
        .schemes
        .iter()
        .find(|scheme| scheme.id == journal.scheme_id)
        .ok_or("方案事务的 candidate 缺少目标方案")?;
    let expected_key = secret_key_for(&journal.scheme_id);
    if journal.secret_key != expected_key || scheme.secret_key != expected_key {
        return Err("方案事务的 secret_key 与 schemeId 不匹配".into());
    }
    let request_base = journal
        .request
        .base_url
        .as_deref()
        .ok_or("方案事务缺少 Base URL")?;
    let request_base = validate_base_url(request_base)?;
    if journal.request.target != scheme.target
        || journal.request.provider_id != scheme.provider_id
        || request_base != scheme.base_url
        || normalized_model(journal.request.model.as_deref())
            != normalized_model(scheme.model.as_deref())
    {
        return Err("方案事务请求与 candidate 元数据不一致".into());
    }
    super::util::validate_secret_value("API Key", &journal.request.api_key)?;
    if let Some(previous) = journal.previous_secret.as_deref() {
        super::util::validate_secret_value("旧方案 Key", previous)?;
    }
    let active = if scheme.target == "claude" {
        journal.candidate_store.active_claude.as_deref()
    } else {
        journal.candidate_store.active_codex.as_deref()
    };
    if active != Some(journal.scheme_id.as_str()) {
        return Err("方案事务 candidate 未将目标方案设为 active".into());
    }
    if journal.cli_before.target != journal.request.target
        || journal
            .cli_before
            .fields
            .keys()
            .ne(expected_cli_fields(&journal.request)?.keys())
    {
        return Err("方案事务的 CLI 前态指纹与请求不匹配".into());
    }
    match journal.phase {
        SchemeTxPhase::Apply if journal.cli_after.is_some() => {
            return Err("Apply 阶段不应包含 CLI 完成态指纹".into())
        }
        SchemeTxPhase::Commit => {
            let after = journal
                .cli_after
                .as_ref()
                .ok_or("Commit 阶段缺少 CLI 完成态指纹")?;
            if !cli_state_matches_request(after, &journal.request)? {
                return Err("Commit 阶段的 CLI 完成态指纹与请求不匹配".into());
            }
        }
        SchemeTxPhase::Clear => {
            let target = journal
                .clear_target
                .as_deref()
                .ok_or("Clear 阶段缺少清除 target")?;
            if target != "both" && target != journal.request.target {
                return Err("Clear 阶段 target 未覆盖事务 CLI".into());
            }
        }
        SchemeTxPhase::Apply | SchemeTxPhase::Abort => {}
    }
    if journal.phase != SchemeTxPhase::Clear && journal.clear_target.is_some() {
        return Err("非 Clear 阶段不应包含清除 target".into());
    }
    Ok(())
}

fn validate_scheme_tx_file() -> Result<std::path::PathBuf, String> {
    // 先共用 schemes 的父目录边界校验。
    let _ = validate_state_and_schemes_file()?;
    let path = schemes_tx_path().ok_or("找不到方案事务路径")?;
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err("schemes.tx.json 不是可信普通文件，已拒绝操作".into())
        }
        Ok(metadata) if metadata.len() > MAX_SCHEME_TX_BYTES => {
            Err("schemes.tx.json 超过 512 KiB，已拒绝读取".into())
        }
        Ok(_metadata) => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if _metadata.permissions().mode() & 0o777 != 0o600 {
                    return Err("schemes.tx.json 权限不是 0600，已拒绝读取 Key".into());
                }
            }
            Ok(path)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(path),
        Err(error) => Err(format!("检查 schemes.tx.json 失败: {error}")),
    }
}

fn load_scheme_tx() -> Result<Option<SchemeTxJournal>, String> {
    // 先校验父目录，再用同一个、不跟随链接的句柄完成 metadata + read，
    // 避免 symlink_metadata/read_to_string 之间的 TOCTOU。
    let _ = validate_state_and_schemes_file()?;
    let path = schemes_tx_path().ok_or("找不到方案事务路径")?;
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let mut file = match options.open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("安全打开 schemes.tx.json 失败: {error}")),
    };
    let metadata = file
        .metadata()
        .map_err(|error| format!("读取方案事务元数据失败: {error}"))?;
    if !metadata.is_file() {
        return Err("schemes.tx.json 不是可信普通文件，已拒绝操作".into());
    }
    if metadata.len() > MAX_SCHEME_TX_BYTES {
        return Err("schemes.tx.json 超过 512 KiB，已拒绝读取".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o777 != 0o600 {
            return Err("schemes.tx.json 权限不是 0600，已拒绝读取 Key".into());
        }
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.by_ref()
        .take(MAX_SCHEME_TX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("读取 schemes.tx.json 失败: {error}"))?;
    if bytes.len() as u64 > MAX_SCHEME_TX_BYTES {
        return Err("schemes.tx.json 读取期间变大，已拒绝".into());
    }
    let raw = std::str::from_utf8(&bytes)
        .map_err(|error| format!("schemes.tx.json 不是 UTF-8，已保留现场: {error}"))?;
    let journal: SchemeTxJournal = serde_json::from_str(raw)
        .map_err(|error| format!("schemes.tx.json 损坏，已保留现场: {error}"))?;
    validate_scheme_tx(&journal)?;
    Ok(Some(journal))
}

fn save_scheme_tx(journal: &SchemeTxJournal) -> Result<(), String> {
    validate_scheme_tx(journal)?;
    let path = validate_scheme_tx_file()?;
    let dir = path.parent().ok_or("方案事务路径无父目录")?;
    std::fs::create_dir_all(dir).map_err(|error| error.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("无法将方案状态目录设为 0700: {error}"))?;
    }
    let body = serde_json::to_string_pretty(journal).map_err(|error| error.to_string())?;
    if body.len() as u64 > MAX_SCHEME_TX_BYTES {
        return Err("方案事务超过 512 KiB，已拒绝写入".into());
    }
    atomic_write_mode(&path, &body, true)
}

fn remove_scheme_tx() -> Result<(), String> {
    let path = validate_scheme_tx_file()?;
    match super::util::remove_file_durable(&path) {
        Ok(()) => Ok(()),
        Err(error) => Err(format!("持久删除 schemes.tx.json 失败: {error}")),
    }
}

fn validate_scheme_store_tx(journal: &SchemeStoreTxJournal) -> Result<(), String> {
    if journal.version != SCHEME_STORE_TX_VERSION {
        return Err(format!(
            "不支持的 schemes.store.tx.json 版本 {}",
            journal.version
        ));
    }
    validate_scheme_id(&journal.scheme_id)?;
    if journal.secret_key != secret_key_for(&journal.scheme_id) {
        return Err("元数据事务 secret_key 与 schemeId 不匹配".into());
    }
    if let Some(value) = journal.previous_secret.as_deref() {
        super::util::validate_secret_value("旧方案 Key", value)?;
    }
    if let Some(value) = journal.candidate_secret.as_deref() {
        super::util::validate_secret_value("新方案 Key", value)?;
    }
    validate_store_integrity(&journal.original_store)?;
    validate_store_integrity(&journal.candidate_store)?;

    match journal.kind {
        SchemeStoreTxKind::Upsert => {
            let candidate_secret = journal
                .candidate_secret
                .as_deref()
                .ok_or("Upsert 元数据事务缺少新 Key")?;
            let candidate = journal
                .candidate_store
                .schemes
                .iter()
                .find(|scheme| scheme.id == journal.scheme_id)
                .ok_or("Upsert 元数据事务 candidate 缺少方案")?;
            if candidate.secret_key != journal.secret_key
                || candidate.api_key_masked != mask_key(candidate_secret)
            {
                return Err("Upsert 元数据事务的方案与新 Key 不匹配".into());
            }
            if journal.candidate_store.active_claude != journal.original_store.active_claude
                || journal.candidate_store.active_codex != journal.original_store.active_codex
            {
                return Err("未应用保存不得改变 active 方案".into());
            }
            if let Some(original) = journal
                .original_store
                .schemes
                .iter()
                .find(|scheme| scheme.id == journal.scheme_id)
            {
                if original.secret_key != journal.secret_key || original.target != candidate.target
                {
                    return Err("Upsert 元数据事务不得改变 secret_key 或 target".into());
                }
            }
            let mut original_others = journal.original_store.schemes.clone();
            original_others.retain(|scheme| scheme.id != journal.scheme_id);
            let mut candidate_others = journal.candidate_store.schemes.clone();
            candidate_others.retain(|scheme| scheme.id != journal.scheme_id);
            if original_others != candidate_others {
                return Err("Upsert 元数据事务改动了无关方案".into());
            }
        }
        SchemeStoreTxKind::Delete => {
            if journal.candidate_secret.is_some() {
                return Err("Delete 元数据事务不应包含新 Key".into());
            }
            let original = journal
                .original_store
                .schemes
                .iter()
                .find(|scheme| scheme.id == journal.scheme_id)
                .ok_or("Delete 元数据事务 original 缺少方案")?;
            if original.secret_key != journal.secret_key
                || journal
                    .candidate_store
                    .schemes
                    .iter()
                    .any(|scheme| scheme.id == journal.scheme_id)
            {
                return Err("Delete 元数据事务的方案/Key 绑定不正确".into());
            }
            let mut expected = journal.original_store.clone();
            expected
                .schemes
                .retain(|scheme| scheme.id != journal.scheme_id);
            if expected.active_claude.as_deref() == Some(journal.scheme_id.as_str()) {
                expected.active_claude = None;
            }
            if expected.active_codex.as_deref() == Some(journal.scheme_id.as_str()) {
                expected.active_codex = None;
            }
            if journal.candidate_store != expected {
                return Err("Delete 元数据事务包含目标方案以外的改动".into());
            }
        }
    }
    Ok(())
}

fn validate_scheme_store_tx_file() -> Result<std::path::PathBuf, String> {
    let _ = validate_state_and_schemes_file()?;
    let path = schemes_store_tx_path().ok_or("找不到方案元数据事务路径")?;
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err("schemes.store.tx.json 不是可信普通文件，已拒绝操作".into())
        }
        Ok(metadata) if metadata.len() > MAX_SCHEME_TX_BYTES => {
            Err("schemes.store.tx.json 超过 512 KiB，已拒绝读取".into())
        }
        Ok(_metadata) => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if _metadata.permissions().mode() & 0o777 != 0o600 {
                    return Err("schemes.store.tx.json 权限不是 0600，已拒绝读取 Key".into());
                }
            }
            Ok(path)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(path),
        Err(error) => Err(format!("检查 schemes.store.tx.json 失败: {error}")),
    }
}

fn load_scheme_store_tx() -> Result<Option<SchemeStoreTxJournal>, String> {
    let _ = validate_state_and_schemes_file()?;
    let path = schemes_store_tx_path().ok_or("找不到方案元数据事务路径")?;
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let mut file = match options.open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("安全打开 schemes.store.tx.json 失败: {error}")),
    };
    let metadata = file
        .metadata()
        .map_err(|error| format!("读取方案元数据事务元数据失败: {error}"))?;
    if !metadata.is_file() || metadata.len() > MAX_SCHEME_TX_BYTES {
        return Err("schemes.store.tx.json 不是可信普通小文件，已拒绝读取".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o777 != 0o600 {
            return Err("schemes.store.tx.json 权限不是 0600，已拒绝读取 Key".into());
        }
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.by_ref()
        .take(MAX_SCHEME_TX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("读取 schemes.store.tx.json 失败: {error}"))?;
    if bytes.len() as u64 > MAX_SCHEME_TX_BYTES {
        return Err("schemes.store.tx.json 读取期间变大，已拒绝".into());
    }
    let raw = std::str::from_utf8(&bytes)
        .map_err(|error| format!("schemes.store.tx.json 不是 UTF-8，已保留现场: {error}"))?;
    let journal: SchemeStoreTxJournal = serde_json::from_str(raw)
        .map_err(|error| format!("schemes.store.tx.json 损坏，已保留现场: {error}"))?;
    validate_scheme_store_tx(&journal)?;
    Ok(Some(journal))
}

fn save_scheme_store_tx(journal: &SchemeStoreTxJournal) -> Result<(), String> {
    validate_scheme_store_tx(journal)?;
    let path = validate_scheme_store_tx_file()?;
    let dir = path.parent().ok_or("方案元数据事务路径无父目录")?;
    std::fs::create_dir_all(dir).map_err(|error| error.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("无法将方案状态目录设为 0700: {error}"))?;
    }
    let body = serde_json::to_string_pretty(journal).map_err(|error| error.to_string())?;
    if body.len() as u64 > MAX_SCHEME_TX_BYTES {
        return Err("方案元数据事务超过 512 KiB，已拒绝写入".into());
    }
    atomic_write_mode(&path, &body, true)
}

fn remove_scheme_store_tx() -> Result<(), String> {
    let path = validate_scheme_store_tx_file()?;
    super::util::remove_file_durable(&path)
        .map_err(|error| format!("持久删除 schemes.store.tx.json 失败: {error}"))
}

fn ensure_scheme_store_tx_state_compatible(journal: &SchemeStoreTxJournal) -> Result<(), String> {
    let current_store = load_store()?;
    if current_store != journal.original_store && current_store != journal.candidate_store {
        return Err("未完成方案元数据事务期间 schemes.json 被外部改动，已拒绝覆盖".into());
    }
    let current_secret = get_persistent_env_strict(&journal.secret_key)?;
    if current_secret != journal.previous_secret && current_secret != journal.candidate_secret {
        return Err("未完成方案元数据事务的 Key 被外部改动，已拒绝覆盖".into());
    }
    Ok(())
}

fn commit_scheme_store_tx(journal: &SchemeStoreTxJournal) -> Result<(), String> {
    ensure_scheme_store_tx_state_compatible(journal)?;
    // journal 一直保留到两个资源均完成；任何 API 的“不确定提交”错误
    // 都只会让下次幂等前滚，不再猜测并回滚另一半。
    restore_secret_value(&journal.secret_key, journal.candidate_secret.as_deref())?;
    save_store(&journal.candidate_store)?;
    remove_scheme_store_tx()
}

fn execute_scheme_store_tx(journal: SchemeStoreTxJournal) -> Result<(), String> {
    if load_scheme_tx()?.is_some() || load_scheme_store_tx()?.is_some() {
        return Err("已存在未完成方案事务，请先重试恢复".into());
    }
    save_scheme_store_tx(&journal)?;
    commit_scheme_store_tx(&journal)
}

fn recover_pending_scheme_store_tx() -> Result<(), String> {
    let journal = load_scheme_store_tx()?;
    if journal.is_some() && load_scheme_tx()?.is_some() {
        return Err("同时存在两类方案事务，已拒绝猜测提交顺序；请保留现场人工处理".into());
    }
    let Some(journal) = journal else {
        return Ok(());
    };
    commit_scheme_store_tx(&journal)
}

/// 完全卸载的用户意图高于“未应用保存/删除”的前滚意图。该事务只涉及
/// 本工具生成的动态 Key 与即将删除的 schemes.json，因此 purge 可安全
/// 丢弃精确 journal；已经写出的孤立 Key 会由 generated key 枚举清掉。
/// 即使 JSON 损坏也只 unlink 这个状态目录内的固定路径，不跟随链接。
fn discard_pending_scheme_store_tx_for_purge() -> Result<(), String> {
    let _ = validate_state_and_schemes_file()?;
    let path = schemes_store_tx_path().ok_or("找不到方案元数据事务路径")?;
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.is_file() || metadata.file_type().is_symlink() => {
            super::util::remove_file_durable(&path)
                .map_err(|error| format!("完全卸载无法隔离方案元数据事务: {error}"))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            super::util::remove_file_durable(&path).map_err(|remove_error| {
                format!("无法确认方案元数据事务删除已持久化: {remove_error}")
            })
        }
        Ok(_) => Err("方案元数据事务路径不是可安全删除的文件".into()),
        Err(error) => Err(format!("检查方案元数据事务清障路径失败: {error}")),
    }
}

fn ensure_scheme_tx_state_compatible(journal: &SchemeTxJournal) -> Result<(), String> {
    let current_store = load_store()?;
    if current_store != journal.original_store && current_store != journal.candidate_store {
        return Err("未完成方案事务期间 schemes.json 被外部改动，已拒绝自动覆盖".into());
    }
    let current_secret = get_persistent_env_strict(&journal.secret_key)?;
    let secret_is_previous = current_secret == journal.previous_secret;
    let secret_is_candidate = current_secret.as_deref() == Some(journal.request.api_key.as_str());
    if !secret_is_previous && !secret_is_candidate {
        return Err("未完成方案事务的 Key 被外部改动，已拒绝自动覆盖".into());
    }
    Ok(())
}

fn abort_scheme_tx(journal: &SchemeTxJournal) -> Result<(), String> {
    ensure_scheme_tx_state_compatible(journal)?;
    rollback_store_and_secret(
        &journal.original_store,
        &journal.secret_key,
        journal.previous_secret.as_deref(),
    )?;
    remove_scheme_tx()
}

fn apply_and_commit_scheme_tx(mut journal: SchemeTxJournal) -> Result<ConfigApplyResult, String> {
    ensure_scheme_tx_state_compatible(&journal)?;
    let result = match journal.phase {
        SchemeTxPhase::Apply => {
            let current = capture_cli_owned_state(&journal.request.target)?;
            if !cli_state_is_forward_compatible(&journal.cli_before, &current, &journal.request)? {
                return Err(
                    "未完成事务期间 CLI 的受管字段被外部修改，已保留 journal 并拒绝覆盖".into(),
                );
            }
            let result = match apply_config_without_scheme_record(journal.request.clone()) {
                Ok(result) => result,
                Err(error) => {
                    // 一旦 journal 已 durable 落盘，就无法证明 CLI 写入完全没有副作用：
                    // 进程可能曾在 CLI 成功后崩溃，也可能由内层报告“回滚不完整”。
                    // 因而失败时必须保留 Apply journal 前滚重试，不能只回滚 store/key
                    // 后删除唯一恢复凭据，否则会永久制造 CLI/方案分裂。
                    return Err(format!(
                        "应用方案未完成，恢复事务已安全保留；修复配置文件/权限后重试，或在完全卸载时回滚: {error}"
                    ));
                }
            };
            let after = capture_cli_owned_state(&journal.request.target)?;
            if !cli_state_matches_request(&after, &journal.request)? {
                return Err(
                    "CLI 返回成功但受管配置未达到目标状态，已保留事务并拒绝提交方案".into(),
                );
            }
            journal.phase = SchemeTxPhase::Commit;
            journal.cli_after = Some(after);
            // 先 durable 标记 CLI 完成，再改 scheme secret/store。若这里失败，
            // 磁盘上的 Apply 会在下次安全重放，不会误回滚。
            save_scheme_tx(&journal)?;
            result
        }
        SchemeTxPhase::Commit => {
            let current = capture_cli_owned_state(&journal.request.target)?;
            let expected = journal
                .cli_after
                .as_ref()
                .ok_or("Commit 事务缺少 CLI 完成态指纹")?;
            if &current != expected {
                return Err(
                    "CLI 完成后其受管字段或 ownership 被外部修改，已保留事务并拒绝覆盖".into(),
                );
            }
            ConfigApplyResult {
                ok: true,
                message: "已确认上次 CLI 配置写入，继续提交方案状态".into(),
                written: Vec::new(),
            }
        }
        SchemeTxPhase::Clear => return Err("Clear 事务必须由清除配置流程继续".into()),
        SchemeTxPhase::Abort => return Err("Abort 事务不能前滚提交".into()),
    };

    // CLI 已应用且 Commit journal 已持久化；之后任意一步失败都保留
    // journal，下次只校验指纹并继续提交，不再覆盖用户配置。
    super::platform::set_user_env(&journal.secret_key, &journal.request.api_key)?;
    save_store(&journal.candidate_store)?;
    remove_scheme_tx()?;
    Ok(result)
}

fn execute_scheme_tx(journal: SchemeTxJournal) -> Result<ConfigApplyResult, String> {
    if load_scheme_tx()?.is_some() || load_scheme_store_tx()?.is_some() {
        return Err("已存在未完成方案事务，请先重试恢复".into());
    }
    save_scheme_tx(&journal)?;
    apply_and_commit_scheme_tx(journal)
}

fn recover_pending_scheme_tx() -> Result<(), String> {
    let cli_tx = load_scheme_tx()?;
    let store_tx = load_scheme_store_tx()?;
    if cli_tx.is_some() && store_tx.is_some() {
        return Err("同时存在两类方案事务，已拒绝猜测提交顺序；请保留现场人工处理".into());
    }
    if let Some(journal) = store_tx {
        return commit_scheme_store_tx(&journal);
    }
    let Some(journal) = cli_tx else {
        return Ok(());
    };
    match journal.phase {
        SchemeTxPhase::Apply | SchemeTxPhase::Commit => {
            apply_and_commit_scheme_tx(journal)?;
            Ok(())
        }
        SchemeTxPhase::Clear => Err("上次清除配置尚未完成，请重试“清除配置”或执行完全卸载".into()),
        SchemeTxPhase::Abort => abort_scheme_tx(&journal),
    }
}

fn abort_pending_scheme_tx_for_purge() -> Result<(), String> {
    match load_scheme_tx() {
        Ok(Some(journal)) => abort_scheme_tx(&journal),
        Ok(None) => Ok(()),
        Err(load_error) => {
            // 完全卸载在此之前已经恢复/清除了 CLI 配置。此时损坏 journal
            // 已无法安全解析回滚，但也不能让它永久封死 UI 清障：只删除状态
            // 目录内这个精确路径本身（符号链接也只 unlink，不跟随目标）。
            let path = schemes_tx_path().ok_or("找不到方案事务路径")?;
            match std::fs::symlink_metadata(&path) {
                Ok(metadata) if metadata.is_file() || metadata.file_type().is_symlink() => {
                    super::util::remove_file_durable(&path).map_err(|remove_error| {
                        format!(
                            "方案事务已损坏（{load_error}），且完全卸载无法隔离该文件: {remove_error}"
                        )
                    })
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    super::util::remove_file_durable(&path).map_err(|remove_error| {
                        format!(
                            "方案事务已损坏（{load_error}），且无法确认清障删除已持久化: {remove_error}"
                        )
                    })
                }
                Ok(_) => Err(format!(
                    "方案事务已损坏（{load_error}），且事务路径不是可安全删除的文件"
                )),
                Err(error) => Err(format!(
                    "方案事务已损坏（{load_error}），且检查清障路径失败: {error}"
                )),
            }
        }
    }
}

/// 返回 true 表示存在同 target 的事务，已 durable 标为 Clear；调用方
/// 必须先成功清 CLI，再调用 `finish_pending_after_config_clear`。失败或
/// 崩溃时 journal 会保留，绝不能提前删除唯一恢复凭据。
fn clear_covers_pending(clear_target: &str, pending_target: &str) -> bool {
    clear_target == "both" || clear_target == pending_target
}

pub(crate) fn prepare_pending_for_config_clear(clear_target: &str) -> Result<bool, String> {
    // 不改 CLI 的保存/删除事务与清 CLI 没有冲突，先幂等前滚完成，
    // 再决定 CLI 事务的 Clear 阶段。
    recover_pending_scheme_store_tx()?;
    let Some(mut journal) = load_scheme_tx()? else {
        return Ok(false);
    };
    if journal.phase == SchemeTxPhase::Clear {
        let pending_target = journal
            .clear_target
            .as_deref()
            .ok_or("Clear 事务缺少 target")?;
        if clear_target == pending_target || clear_target == "both" {
            return Ok(true);
        }
        return Err(format!(
            "上次正在清除 {pending_target}，请先重试该 target 或选择 both"
        ));
    }
    let covers_pending = clear_covers_pending(clear_target, &journal.request.target);
    if covers_pending {
        journal.phase = SchemeTxPhase::Clear;
        journal.clear_target = Some(clear_target.to_string());
        save_scheme_tx(&journal)?;
        return Ok(true);
    }

    // 清的是另一个 target：不能删除当前事务的唯一恢复凭据后只清
    // 无关 CLI。先完成已经开始的事务，再继续清用户请求的 target。
    match journal.phase {
        SchemeTxPhase::Apply | SchemeTxPhase::Commit => {
            apply_and_commit_scheme_tx(journal)?;
            Ok(false)
        }
        SchemeTxPhase::Abort => {
            abort_scheme_tx(&journal)?;
            Ok(false)
        }
        SchemeTxPhase::Clear => unreachable!("已在上方处理"),
    }
}

pub(crate) fn prepare_pending_for_purge() -> Result<bool, String> {
    // 有效的元数据事务必须先幂等前滚，不能裸删唯一 journal；否则若
    // Key/store 已只完成一半，purge 在下一步前崩溃会永久 split。
    // 只有损坏/不可读 journal 才保留到 CLI clear 成功后，由
    // clear_all_scheme_secrets 的固定路径 fallback 隔离。
    let store_tx = load_scheme_store_tx();
    let cli_tx = load_scheme_tx();
    if matches!(store_tx, Ok(Some(_))) && matches!(cli_tx, Ok(Some(_))) {
        return Err("同时存在两类有效方案事务，已拒绝猜测 purge 提交顺序".into());
    }
    if let Ok(Some(journal)) = store_tx {
        commit_scheme_store_tx(&journal)?;
    }
    match cli_tx {
        Ok(Some(mut journal)) => {
            journal.phase = SchemeTxPhase::Clear;
            journal.clear_target = Some("both".into());
            save_scheme_tx(&journal)?;
            Ok(true)
        }
        Ok(None) => Ok(false),
        // 损坏/不可读 journal 无法安全改写成 Clear，但也不能在 CLI
        // clear 前删除唯一现场。继续幂等清 both；成功后
        // clear_all_scheme_secrets 会通过 purge 专用精确 unlink fallback
        // 隔离它。若中途崩溃，下次 purge 会再次 clear，不会前滚重放。
        Err(_) => Ok(false),
    }
}

pub(crate) fn finish_pending_after_config_clear() -> Result<(), String> {
    let Some(journal) = load_scheme_tx()? else {
        return Ok(());
    };
    if journal.phase != SchemeTxPhase::Clear {
        return Err("清除完成时发现事务并非 Clear 阶段，已拒绝删除 journal".into());
    }
    abort_scheme_tx(&journal)
}

pub(crate) fn deactivate_after_config_clear(target: &str) -> Result<Vec<String>, String> {
    let mut store = load_store()?;
    let mut changed = Vec::new();
    if target == "claude" || target == "both" {
        if let Some(id) = store.active_claude.take() {
            changed.push(format!("deactivated-scheme:claude:{id}"));
        }
    }
    if target == "codex" || target == "both" {
        if let Some(id) = store.active_codex.take() {
            changed.push(format!("deactivated-scheme:codex:{id}"));
        }
    }
    if !changed.is_empty() {
        save_store(&store)?;
    }
    Ok(changed)
}

fn active_scheme_matches_cli(store: &SchemeStore, id: &str, target: &str) -> Result<bool, String> {
    let scheme = store
        .schemes
        .iter()
        .find(|scheme| scheme.id == id && scheme.target == target)
        .ok_or("active 方案不存在或 target 不匹配")?;
    let Some(api_key) =
        get_persistent_env_strict(&scheme.secret_key)?.filter(|value| !value.is_empty())
    else {
        return Ok(false);
    };
    let request = ConfigApplyRequest {
        provider_id: scheme.provider_id.clone(),
        api_key,
        base_url: Some(scheme.base_url.clone()),
        model: scheme.model.clone(),
        target: scheme.target.clone(),
    };
    let current = capture_cli_owned_state(target)?;
    cli_state_matches_request(&current, &request)
}

/// active 只是“当前 CLI 确实等于该方案”的派生状态。用户手改配置或
/// 旧版 clear 后若不再匹配，应清掉指针，不能继续向 UI 宣称已生效。
fn reconcile_active_scheme_markers(store: &mut SchemeStore) -> Result<bool, String> {
    let mut changed = false;
    if let Some(id) = store.active_claude.clone() {
        if !active_scheme_matches_cli(store, &id, "claude")? {
            store.active_claude = None;
            changed = true;
        }
    }
    if let Some(id) = store.active_codex.clone() {
        if !active_scheme_matches_cli(store, &id, "codex")? {
            store.active_codex = None;
            changed = true;
        }
    }
    Ok(changed)
}

/// 普通 API 配置也必须在写 CLI 之前建立同一份 durable journal；否则
/// 进程若在 CLI 成功与 `record_after_apply` 之间退出，active 方案会永久
/// 指向旧配置。调用方已经持有全局 op_lock。
pub(crate) fn apply_config_with_scheme_tx(
    req: ConfigApplyRequest,
) -> Result<ConfigApplyResult, String> {
    recover_pending_scheme_tx()?;

    let target = req.target.trim().to_lowercase();
    if target == "both" {
        return Err(
            "v1 已取消「两者」：请分别配置 Claude（Anthropic 兼容）与 Codex（OpenAI 兼容）".into(),
        );
    }
    if target != "claude" && target != "codex" {
        return Err("target 须为 claude|codex".into());
    }
    let provider_id = req.provider_id.trim().to_string();
    if provider_id.is_empty() {
        return Err("providerId 不能为空".into());
    }
    validate_provider_target(&provider_id, &target)?;
    let api_key = req.api_key.trim().to_string();
    super::util::validate_secret_value("API Key", &api_key)?;
    let provider = find_provider(&provider_id);
    let base_url = req
        .base_url
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .or_else(|| provider.as_ref().map(|provider| provider.base_url.clone()))
        .filter(|value| !value.is_empty())
        .ok_or("Base URL 不能为空，请选择服务商或手动填写")?;
    let base_url = validate_base_url(&base_url)?;
    let model = req
        .model
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .or_else(|| provider.and_then(|provider| provider.default_model))
        .filter(|value| !value.trim().is_empty())
        .ok_or("模型名不能为空；自定义服务商必须填写实际模型")?;
    validate_env_value("model", &model)?;

    let original_store = load_store()?;
    let existing = original_store
        .schemes
        .iter()
        .position(|scheme| same_endpoint(scheme, &target, &provider_id, &base_url, Some(&model)));
    let id = existing
        .map(|position| original_store.schemes[position].id.clone())
        .unwrap_or_else(new_id);
    validate_scheme_id(&id)?;
    let secret_key = secret_key_for(&id);
    let previous_secret = get_persistent_env_strict(&secret_key)?;
    let name = existing
        .map(|position| original_store.schemes[position].name.clone())
        .unwrap_or_else(|| default_name(&target, &provider_id, Some(&model)));

    let mut candidate_store = original_store.clone();
    let scheme = Scheme {
        id: id.clone(),
        name,
        target: target.clone(),
        provider_id: provider_id.clone(),
        base_url: base_url.clone(),
        model: Some(model.clone()),
        api_key_masked: mask_key(&api_key),
        secret_key: secret_key.clone(),
        updated_at: chrono_like_now(),
        // 普通 apply 可能来自“强制保存”，不能伪造已联网验证。
        last_verified_ok: None,
    };
    if let Some(position) = existing {
        candidate_store.schemes[position] = scheme;
    } else {
        candidate_store.schemes.push(scheme);
    }
    if target == "claude" {
        candidate_store.active_claude = Some(id.clone());
    } else {
        candidate_store.active_codex = Some(id.clone());
    }

    let normalized_request = ConfigApplyRequest {
        provider_id,
        api_key,
        base_url: Some(base_url),
        model: Some(model),
        target: target.clone(),
    };
    let cli_before = capture_cli_owned_state(&target)?;
    execute_scheme_tx(SchemeTxJournal {
        version: SCHEME_TX_VERSION,
        phase: SchemeTxPhase::Apply,
        scheme_id: id,
        secret_key,
        previous_secret,
        original_store,
        candidate_store,
        request: normalized_request,
        cli_before,
        cli_after: None,
        clear_target: None,
    })
}

#[tauri::command]
pub async fn list_schemes() -> Result<SchemeListResult, String> {
    super::util::spawn_blocking_result(|| {
        with_op_lock(|| {
            recover_pending_scheme_tx()?;
            let mut store = load_store()?;
            if reconcile_active_scheme_markers(&mut store)? {
                save_store(&store)?;
            }
            Ok(SchemeListResult {
                ok: true,
                active_claude: store.active_claude,
                active_codex: store.active_codex,
                schemes: store.schemes,
                message: "ok".into(),
            })
        })
    })
    .await
}

#[tauri::command]
pub async fn upsert_scheme(req: SchemeUpsertRequest) -> Result<ConfigApplyResult, String> {
    super::util::spawn_blocking_result(move || with_op_lock(|| upsert_scheme_sync(req))).await
}

fn upsert_scheme_sync(req: SchemeUpsertRequest) -> Result<ConfigApplyResult, String> {
    recover_pending_scheme_tx()?;
    let target = req.target.to_lowercase();
    if target != "claude" && target != "codex" {
        return Err("target 须为 claude|codex".into());
    }
    let provider_id = req.provider_id.trim().to_string();
    if provider_id.is_empty() {
        return Err("providerId 不能为空".into());
    }
    validate_provider_target(&provider_id, &target)?;
    let key = req.api_key.trim();
    if key.is_empty() {
        return Err("API Key 不能为空".into());
    }
    validate_env_value("API Key", key)?;
    let base_raw = req
        .base_url
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or("Base URL 不能为空")?;
    let base_url = validate_base_url(base_raw)?;
    let model = req
        .model
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .or_else(|| find_provider(&provider_id).and_then(|provider| provider.default_model))
        .filter(|value| !value.trim().is_empty())
        .ok_or("模型名不能为空；自定义服务商必须填写实际模型")?;

    let original_store = load_store()?;
    let requested_id = req
        .id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if let Some(id) = requested_id {
        validate_scheme_id(id)?;
        let existing = original_store
            .schemes
            .iter()
            .find(|scheme| scheme.id == id)
            .ok_or("schemeId 不存在，不能伪造更新")?;
        // target 变化不是普通字段编辑：它还需要原子清掉旧 target 的
        // CLI 配置。当前事务只覆盖一个 CLI target，因此明确禁止这种
        // 跨 target 更新，避免 store 已迁移而旧 CLI 仍残留。
        validate_existing_scheme_target(existing, &target)?;
    }
    let endpoint_match = original_store
        .schemes
        .iter()
        .position(|scheme| same_endpoint(scheme, &target, &provider_id, &base_url, Some(&model)));
    let id = resolve_existing_upsert_id(&original_store, requested_id, endpoint_match)?
        .unwrap_or_else(new_id);
    let sk = secret_key_for(&id);
    let previous_secret = get_persistent_env_strict(&sk)?;

    let name = req
        .name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(|| default_name(&target, &provider_id, Some(&model)));

    let scheme = Scheme {
        id: id.clone(),
        name,
        target: target.clone(),
        provider_id: provider_id.clone(),
        base_url: base_url.clone(),
        model: Some(model.clone()),
        api_key_masked: mask_key(key),
        secret_key: sk.clone(),
        updated_at: chrono_like_now(),
        last_verified_ok: None,
    };

    let apply = req.apply.unwrap_or(true);
    validate_inactive_save_policy(&original_store, requested_id, apply)?;
    if apply {
        let test = test_connectivity_sync(ConnectivityRequest {
            provider_id: provider_id.clone(),
            api_key: key.to_string(),
            base_url: Some(base_url.clone()),
            protocol: Some(if target == "claude" {
                "anthropic".into()
            } else {
                "openai".into()
            }),
            model: Some(model.clone()),
        })?;
        if !test.ok {
            return Err(format!(
                "方案连通测试未通过，未保存也未切换当前配置: {}",
                test.message
            ));
        }
    }

    let mut store = original_store.clone();
    if let Some(pos) = store.schemes.iter().position(|s| s.id == id) {
        store.schemes[pos] = scheme;
    } else {
        store.schemes.push(scheme);
    }

    if apply {
        if target == "claude" {
            store.active_claude = Some(id.clone());
        } else {
            store.active_codex = Some(id.clone());
        }
        if let Some(saved) = store.schemes.iter_mut().find(|saved| saved.id == id) {
            saved.last_verified_ok = Some(true);
        }
    }

    if apply {
        let cli_before = capture_cli_owned_state(&target)?;
        let journal = SchemeTxJournal {
            version: SCHEME_TX_VERSION,
            phase: SchemeTxPhase::Apply,
            scheme_id: id,
            secret_key: sk,
            previous_secret,
            original_store,
            candidate_store: store,
            request: ConfigApplyRequest {
                provider_id,
                api_key: key.to_string(),
                base_url: Some(base_url),
                model: Some(model),
                target,
            },
            cli_before,
            cli_after: None,
            clear_target: None,
        };
        return execute_scheme_tx(journal);
    }

    execute_scheme_store_tx(SchemeStoreTxJournal {
        version: SCHEME_STORE_TX_VERSION,
        kind: SchemeStoreTxKind::Upsert,
        scheme_id: id,
        secret_key: sk,
        previous_secret,
        candidate_secret: Some(key.to_string()),
        original_store,
        candidate_store: store,
    })?;

    Ok(ConfigApplyResult {
        ok: true,
        message: format!("方案已保存（未应用）· Key {}", mask_key(key)),
        written: vec![schemes_path()
            .map(|p| p.display().to_string())
            .unwrap_or_default()],
    })
}

#[tauri::command]
pub async fn switch_scheme(scheme_id: String) -> Result<ConfigApplyResult, String> {
    super::util::spawn_blocking_result(move || with_op_lock(|| switch_scheme_sync(scheme_id))).await
}

fn switch_scheme_sync(scheme_id: String) -> Result<ConfigApplyResult, String> {
    recover_pending_scheme_tx()?;
    let id = scheme_id.trim();
    if id.is_empty() {
        return Err("schemeId 不能为空".into());
    }
    validate_scheme_id(id)?;
    let original_store = load_store()?;
    let scheme = original_store
        .schemes
        .iter()
        .find(|s| s.id == id)
        .ok_or("方案不存在")?
        .clone();
    validate_provider_target(&scheme.provider_id, &scheme.target)?;

    let api_key = get_persistent_env_strict(&scheme.secret_key)?
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            format!(
                "方案「{}」的 Key 已丢失（secrets 无 {}）。请重新配置 API。",
                scheme.name, scheme.secret_key
            )
        })?;

    let connectivity = test_connectivity_sync(ConnectivityRequest {
        provider_id: scheme.provider_id.clone(),
        api_key: api_key.clone(),
        base_url: Some(scheme.base_url.clone()),
        protocol: Some(if scheme.target == "claude" {
            "anthropic".into()
        } else {
            "openai".into()
        }),
        model: scheme.model.clone(),
    });
    match connectivity {
        Ok(test) if test.ok => {}
        Ok(test) => {
            let mut failed_store = original_store.clone();
            if let Some(saved) = failed_store
                .schemes
                .iter_mut()
                .find(|saved| saved.id == scheme.id)
            {
                saved.last_verified_ok = Some(false);
            }
            save_store(&failed_store)?;
            return Err(format!(
                "方案连通测试未通过，已保留当前配置: {}",
                test.message
            ));
        }
        Err(error) => {
            let mut failed_store = original_store.clone();
            if let Some(saved) = failed_store
                .schemes
                .iter_mut()
                .find(|saved| saved.id == scheme.id)
            {
                saved.last_verified_ok = Some(false);
            }
            save_store(&failed_store)?;
            return Err(format!("方案连通测试出错，已保留当前配置: {error}"));
        }
    }

    let mut store = original_store.clone();
    if scheme.target == "claude" {
        store.active_claude = Some(scheme.id.clone());
    } else {
        store.active_codex = Some(scheme.id.clone());
    }
    if let Some(saved) = store.schemes.iter_mut().find(|saved| saved.id == scheme.id) {
        saved.updated_at = chrono_like_now();
        saved.last_verified_ok = Some(true);
    }
    let cli_before = capture_cli_owned_state(&scheme.target)?;
    let journal = SchemeTxJournal {
        version: SCHEME_TX_VERSION,
        phase: SchemeTxPhase::Apply,
        scheme_id: scheme.id.clone(),
        secret_key: scheme.secret_key.clone(),
        previous_secret: Some(api_key.clone()),
        original_store,
        candidate_store: store,
        request: ConfigApplyRequest {
            provider_id: scheme.provider_id.clone(),
            api_key,
            base_url: Some(scheme.base_url.clone()),
            model: scheme.model.clone(),
            target: scheme.target.clone(),
        },
        cli_before,
        cli_after: None,
        clear_target: None,
    };
    let result = execute_scheme_tx(journal)?;

    Ok(ConfigApplyResult {
        ok: true,
        message: format!(
            "已切换到「{}」→ {}（{}）· {}",
            scheme.name, scheme.target, scheme.provider_id, result.message
        ),
        written: result.written,
    })
}

#[tauri::command]
pub async fn delete_scheme(scheme_id: String) -> Result<ConfigApplyResult, String> {
    super::util::spawn_blocking_result(move || with_op_lock(|| delete_scheme_sync(scheme_id))).await
}

fn delete_scheme_sync(scheme_id: String) -> Result<ConfigApplyResult, String> {
    recover_pending_scheme_tx()?;
    let id = scheme_id.trim();
    validate_scheme_id(id)?;
    let original_store = load_store()?;
    let mut store = original_store.clone();
    let Some(pos) = store.schemes.iter().position(|s| s.id == id) else {
        return Err("方案不存在".into());
    };
    let removed = store.schemes.remove(pos);
    if store.active_claude.as_deref() == Some(id) {
        store.active_claude = None;
    }
    if store.active_codex.as_deref() == Some(id) {
        store.active_codex = None;
    }
    let expected_secret_key = secret_key_for(&removed.id);
    if removed.secret_key != expected_secret_key {
        return Err("方案中的 secret_key 与 id 不匹配，已拒绝删除".into());
    }
    let previous_secret = get_persistent_env_strict(&expected_secret_key)?;
    execute_scheme_store_tx(SchemeStoreTxJournal {
        version: SCHEME_STORE_TX_VERSION,
        kind: SchemeStoreTxKind::Delete,
        scheme_id: removed.id.clone(),
        secret_key: expected_secret_key,
        previous_secret,
        candidate_secret: None,
        original_store,
        candidate_store: store,
    })?;
    Ok(ConfigApplyResult {
        ok: true,
        message: format!("已删除方案「{}」（未改当前 CLI 配置）", removed.name),
        written: vec![json!({"deleted": removed.id}).to_string()],
    })
}

/// 完全卸载前清理动态 `SCHEME_*_KEY`。先严格解析/校验全部
/// 记录，再执行删除；任一键失败时尽量回滚已删键，避免删掉
/// schemes.json 后在注册表/profile 里留下无法追踪的 Key。
pub(crate) fn clear_all_scheme_secrets() -> Result<Vec<String>, String> {
    discard_pending_scheme_store_tx_for_purge()?;
    abort_pending_scheme_tx_for_purge()?;
    let store = load_store()?;
    let mut keys = std::collections::BTreeSet::new();
    for scheme in &store.schemes {
        validate_scheme_id(&scheme.id)?;
        let expected = secret_key_for(&scheme.id);
        if scheme.secret_key != expected {
            return Err(format!(
                "方案「{}」的 secret_key 与 id 不匹配，已拒绝完全卸载",
                scheme.name
            ));
        }
        keys.insert(expected);
    }
    keys.extend(super::platform::generated_scheme_secret_keys()?);

    let snapshots: Vec<(String, Option<String>)> = keys
        .into_iter()
        .map(|key| {
            let previous = get_persistent_env_strict(&key)?;
            Ok((key, previous))
        })
        .collect::<Result<Vec<_>, String>>()?;
    let mut removed = Vec::new();
    for (key, _) in &snapshots {
        if let Err(error) = super::platform::unset_user_env(key) {
            let mut rollback_errors = Vec::new();
            // 当前 unset 虽返回错误也可能已部分落盘，一并回滚。
            let mut restore_keys = removed.clone();
            restore_keys.push(key.clone());
            for removed_key in &restore_keys {
                if let Some((_, previous)) = snapshots
                    .iter()
                    .find(|(snapshot_key, _)| snapshot_key == removed_key)
                {
                    if let Err(restore_error) =
                        restore_secret_value(removed_key, previous.as_deref())
                    {
                        rollback_errors.push(format!("{removed_key}: {restore_error}"));
                    }
                }
            }
            return if rollback_errors.is_empty() {
                Err(format!("清理方案 Key {key} 失败: {error}"))
            } else {
                Err(format!(
                    "清理方案 Key {key} 失败: {error}；回滚不完整: {}",
                    rollback_errors.join("；")
                ))
            };
        }
        removed.push(key.clone());
    }
    Ok(removed
        .into_iter()
        .map(|key| format!("removed-env:{key}"))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_key_safe() {
        let k = secret_key_for("ab-12");
        assert!(k.starts_with("SCHEME_"));
        assert!(!k.contains('-'));
    }

    #[test]
    fn store_serde_roundtrip() {
        // 不碰 HOME，避免与 config 并行测试互踩
        let store = SchemeStore {
            active_claude: Some("1".into()),
            schemes: vec![Scheme {
                id: "1".into(),
                name: "t".into(),
                target: "claude".into(),
                provider_id: "deepseek-anthropic".into(),
                base_url: "https://api.deepseek.com/anthropic".into(),
                model: Some("deepseek-v4-pro".into()),
                api_key_masked: "****".into(),
                secret_key: "SCHEME_1_KEY".into(),
                updated_at: "now".into(),
                last_verified_ok: None,
            }],
            ..SchemeStore::default()
        };
        let body = serde_json::to_string_pretty(&store).unwrap();
        let loaded: SchemeStore = serde_json::from_str(&body).unwrap();
        assert_eq!(loaded.schemes.len(), 1);
        assert_eq!(loaded.schemes[0].provider_id, "deepseek-anthropic");
        assert_eq!(loaded.active_claude.as_deref(), Some("1"));
    }

    fn test_scheme(id: &str, base_url: &str) -> Scheme {
        Scheme {
            id: id.into(),
            name: id.into(),
            target: "claude".into(),
            provider_id: "custom-anthropic".into(),
            base_url: base_url.into(),
            model: Some("model-a".into()),
            api_key_masked: "****".into(),
            secret_key: secret_key_for(id),
            updated_at: "now".into(),
            last_verified_ok: None,
        }
    }

    #[test]
    fn store_rejects_duplicate_endpoint() {
        let store = SchemeStore {
            schemes: vec![
                test_scheme("one", "https://example.com/v1"),
                test_scheme("two", "https://example.com/v1"),
            ],
            ..SchemeStore::default()
        };
        assert!(validate_store_integrity(&store)
            .unwrap_err()
            .contains("重复方案 endpoint"));
    }

    #[test]
    fn store_rejects_colliding_secret_key_mapping() {
        let store = SchemeStore {
            schemes: vec![
                test_scheme("ab-12", "https://one.example/v1"),
                test_scheme("ab_12", "https://two.example/v1"),
            ],
            ..SchemeStore::default()
        };
        assert!(validate_store_integrity(&store)
            .unwrap_err()
            .contains("重复密钥名"));
    }

    #[test]
    fn new_duplicate_endpoint_never_reuses_existing_id() {
        let store = SchemeStore {
            schemes: vec![test_scheme("one", "https://example.com/v1")],
            ..SchemeStore::default()
        };
        let error = resolve_existing_upsert_id(&store, None, Some(0)).unwrap_err();
        assert!(error.contains("不会静默覆盖"));
        assert_eq!(
            resolve_existing_upsert_id(&store, Some("one"), Some(0)).unwrap(),
            Some("one".into())
        );
    }

    #[test]
    fn inactive_save_rejects_updates_to_active_scheme() {
        let store = SchemeStore {
            active_claude: Some("one".into()),
            schemes: vec![test_scheme("one", "https://example.com/v1")],
            ..SchemeStore::default()
        };
        assert!(validate_inactive_save_policy(&store, Some("one"), false).is_err());
        assert!(validate_inactive_save_policy(&store, Some("one"), true).is_ok());
        assert!(validate_inactive_save_policy(&store, None, false).is_ok());
    }

    #[test]
    fn existing_scheme_cannot_change_target_in_place() {
        let scheme = test_scheme("one", "https://example.com/v1");
        assert!(validate_existing_scheme_target(&scheme, "claude").is_ok());
        assert!(validate_existing_scheme_target(&scheme, "codex")
            .unwrap_err()
            .contains("不能直接变更 target"));
    }

    #[test]
    fn store_transaction_binds_candidate_store_and_secret() {
        let key = "sk-test-store-transaction-123456";
        let mut scheme = test_scheme("one", "https://example.com/v1");
        scheme.api_key_masked = mask_key(key);
        let candidate = SchemeStore {
            schemes: vec![scheme.clone()],
            ..SchemeStore::default()
        };
        let journal = SchemeStoreTxJournal {
            version: SCHEME_STORE_TX_VERSION,
            kind: SchemeStoreTxKind::Upsert,
            scheme_id: scheme.id.clone(),
            secret_key: scheme.secret_key.clone(),
            previous_secret: None,
            candidate_secret: Some(key.into()),
            original_store: SchemeStore::default(),
            candidate_store: candidate.clone(),
        };
        validate_scheme_store_tx(&journal).unwrap();
        let encoded = serde_json::to_string(&journal).unwrap();
        let decoded: SchemeStoreTxJournal = serde_json::from_str(&encoded).unwrap();
        validate_scheme_store_tx(&decoded).unwrap();

        let mut bad = decoded;
        bad.candidate_store.schemes[0].api_key_masked = "wrong".into();
        assert!(validate_scheme_store_tx(&bad).is_err());

        let delete = SchemeStoreTxJournal {
            version: SCHEME_STORE_TX_VERSION,
            kind: SchemeStoreTxKind::Delete,
            scheme_id: scheme.id.clone(),
            secret_key: scheme.secret_key,
            previous_secret: Some(key.into()),
            candidate_secret: None,
            original_store: candidate,
            candidate_store: SchemeStore::default(),
        };
        validate_scheme_store_tx(&delete).unwrap();
    }

    #[test]
    fn target_specific_clear_never_covers_other_pending_cli() {
        assert!(clear_covers_pending("claude", "claude"));
        assert!(clear_covers_pending("codex", "codex"));
        assert!(clear_covers_pending("both", "claude"));
        assert!(clear_covers_pending("both", "codex"));
        assert!(!clear_covers_pending("claude", "codex"));
        assert!(!clear_covers_pending("codex", "claude"));
    }

    #[test]
    fn scheme_transaction_roundtrip_and_binding_validation() {
        let scheme = test_scheme("one", "https://example.com/v1");
        let candidate = SchemeStore {
            active_claude: Some("one".into()),
            schemes: vec![scheme.clone()],
            ..SchemeStore::default()
        };
        let request = ConfigApplyRequest {
            provider_id: scheme.provider_id.clone(),
            api_key: "sk-test-scheme-transaction-1234".into(),
            base_url: Some(scheme.base_url.clone()),
            model: scheme.model.clone(),
            target: scheme.target.clone(),
        };
        let cli_before = CliOwnedState {
            target: request.target.clone(),
            fields: expected_cli_fields(&request)
                .unwrap()
                .into_keys()
                .map(|key| (key, None))
                .collect(),
            ownership_digest: None,
        };
        let journal = SchemeTxJournal {
            version: SCHEME_TX_VERSION,
            phase: SchemeTxPhase::Apply,
            scheme_id: scheme.id.clone(),
            secret_key: scheme.secret_key.clone(),
            previous_secret: None,
            original_store: SchemeStore::default(),
            candidate_store: candidate,
            request,
            cli_before,
            cli_after: None,
            clear_target: None,
        };
        validate_scheme_tx(&journal).unwrap();
        let encoded = serde_json::to_string(&journal).unwrap();
        let decoded: SchemeTxJournal = serde_json::from_str(&encoded).unwrap();
        validate_scheme_tx(&decoded).unwrap();

        let mut bad = decoded;
        bad.secret_key = "SCHEME_OTHER_KEY".into();
        assert!(validate_scheme_tx(&bad).unwrap_err().contains("secret_key"));
    }

    #[test]
    fn cli_fingerprint_allows_only_before_or_candidate_values() {
        let request = ConfigApplyRequest {
            provider_id: "custom-anthropic".into(),
            api_key: "sk-test-fingerprint-123456".into(),
            base_url: Some("https://example.com/v1".into()),
            model: Some("model-a".into()),
            target: "claude".into(),
        };
        let expected = expected_cli_fields(&request).unwrap();
        let before = CliOwnedState {
            target: "claude".into(),
            fields: expected
                .keys()
                .map(|key| (key.clone(), Some(digest_string("old"))))
                .collect(),
            ownership_digest: Some("before".into()),
        };
        let mut partial = before.clone();
        let first_key = partial.fields.keys().next().unwrap().clone();
        partial.fields.insert(
            first_key.clone(),
            expected.get(&first_key).cloned().unwrap(),
        );
        assert!(cli_state_is_forward_compatible(&before, &partial, &request).unwrap());

        partial
            .fields
            .insert(first_key, Some(digest_string("external-edit")));
        assert!(!cli_state_is_forward_compatible(&before, &partial, &request).unwrap());
    }

    #[test]
    fn commit_phase_requires_matching_after_fingerprint() {
        let scheme = test_scheme("one", "https://example.com/v1");
        let candidate = SchemeStore {
            active_claude: Some("one".into()),
            schemes: vec![scheme.clone()],
            ..SchemeStore::default()
        };
        let request = ConfigApplyRequest {
            provider_id: scheme.provider_id.clone(),
            api_key: "sk-test-commit-transaction-1234".into(),
            base_url: Some(scheme.base_url.clone()),
            model: scheme.model.clone(),
            target: scheme.target.clone(),
        };
        let expected = expected_cli_fields(&request).unwrap();
        let before = CliOwnedState {
            target: "claude".into(),
            fields: expected.keys().map(|key| (key.clone(), None)).collect(),
            ownership_digest: None,
        };
        let mut journal = SchemeTxJournal {
            version: SCHEME_TX_VERSION,
            phase: SchemeTxPhase::Commit,
            scheme_id: scheme.id.clone(),
            secret_key: scheme.secret_key.clone(),
            previous_secret: None,
            original_store: SchemeStore::default(),
            candidate_store: candidate,
            request,
            cli_before: before,
            cli_after: Some(CliOwnedState {
                target: "claude".into(),
                fields: expected,
                ownership_digest: Some("after".into()),
            }),
            clear_target: None,
        };
        validate_scheme_tx(&journal).unwrap();
        journal
            .cli_after
            .as_mut()
            .unwrap()
            .fields
            .insert("settings.env.ANTHROPIC_MODEL".into(), None);
        assert!(validate_scheme_tx(&journal).is_err());
    }
}
