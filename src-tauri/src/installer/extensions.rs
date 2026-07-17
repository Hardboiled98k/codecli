// SPDX-License-Identifier: MPL-2.0
//! 精选 Skill / MCP / 飞书 CLI — 仅白名单，可装可卸

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use super::cmd::{check_cancelled, run_timed};
use super::op_lock::with_op_lock;
use super::platform::{
    add_user_path_segment_windows, claude_config_dir, codecli_state_dir, ensure_tool_path_block,
    remove_tool_path_block, remove_user_path_segment_windows, which_cmd,
};
use super::runtime::npm_command;
#[cfg(windows)]
use super::runtime::validate_cli_launchers;
use super::util::{
    atomic_replace_file, atomic_write_mode, chrono_like_now, remove_file_durable,
    shell_single_quote, sync_parent_dir,
};

const MAX_OWNERSHIP_BYTES: u64 = 64 * 1024;
const MAX_FEISHU_TREE_ENTRIES: usize = 100_000;
const MAX_FEISHU_TREE_FILE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_FEISHU_TREE_TOTAL_BYTES: u64 = 512 * 1024 * 1024;
const MAX_FEISHU_TREE_DEPTH: usize = 64;
const OWNERSHIP_VERSION: u8 = 3;
const FEISHU_PACKAGE: &str = "@larksuite/cli";
const FEISHU_COMMAND: &str = "lark-cli";
/// 只执行经过本版本审计的安装脚本，避免 `@latest` 在发货后静默改变供应链行为。
const FEISHU_PACKAGE_VERSION: &str = "1.0.70";
const FEISHU_INSTALL_SCRIPT_SHA256: &str =
    "c057a117af60f1bf908507ee799dd2d17acc582f315153e996de1bfedd7618de";
const FEISHU_RUN_SCRIPT_SHA256: &str =
    "b6b575a31d62ea45f55155f1090a49d31e79a1b0e5c70af15f9431ab850ca577";
const FEISHU_CHECKSUMS_SHA256: &str =
    "106ac4329692a2d339145d4e08d905f50310733c02ef2783f29dfdc690c13ea7";
const FEISHU_PATH_TAG: &str = "extension-feishu";
const FEISHU_PENDING_MARKER: &str = ".codecli-feishu-reservation";
const FEISHU_PENDING_MARKER_BODY: &str = "codecli-feishu-reservation-v1\n";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtensionItem {
    pub id: String,
    /// skill | mcp | cli
    pub kind: String,
    pub name: String,
    pub description: String,
    pub risk: String,
    pub source: String,
    pub installed: bool,
    /// 当前条目是否有本工具的 durable ownership 记录。
    pub owned_by_tool: bool,
    /// 只有本工具拥有的条目才允许自动卸载；用户自装永不删除。
    pub can_uninstall: bool,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtensionListResult {
    pub ok: bool,
    pub items: Vec<ExtensionItem>,
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtensionActionResult {
    pub ok: bool,
    pub message: String,
    pub written: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
enum OwnershipState {
    Pending,
    Installed,
    /// 精确删除 receipt 绑定的 package/launcher 前持久化；未知 sibling 永不删除。
    Uninstalling,
    /// 兼容旧版中断事务：只允许删精确 marker 与空骨架。
    CleanupPending,
    /// v1 只记录 ID，无法证明实际副作用位置/内容；只能安全迁移或放弃归属。
    Legacy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct OwnershipRecord {
    state: OwnershipState,
    kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    target: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    fingerprint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    package: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    receipt: Option<super::pinned_npm::PinnedBundleReceipt>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct OwnershipStore {
    version: u8,
    items: BTreeMap<String, OwnershipRecord>,
}

impl Default for OwnershipStore {
    fn default() -> Self {
        Self {
            version: OWNERSHIP_VERSION,
            items: BTreeMap::new(),
        }
    }
}

fn skills_dir() -> Option<PathBuf> {
    claude_config_dir().map(|d| d.join("skills"))
}

fn mcp_config_path() -> Option<PathBuf> {
    claude_config_dir().map(|d| d.join("codecli-installer").join("mcp-whitelist.json"))
}

fn feishu_prefix() -> Result<PathBuf, String> {
    Ok(codecli_state_dir()
        .ok_or("找不到 CodeCLI 状态目录")?
        .join("extensions-npm"))
}

fn feishu_bin_dir(prefix: &Path) -> PathBuf {
    if cfg!(windows) {
        prefix.to_path_buf()
    } else {
        prefix.join("bin")
    }
}

fn feishu_package_dir(prefix: &Path) -> PathBuf {
    if cfg!(windows) {
        prefix.join("node_modules").join("@larksuite").join("cli")
    } else {
        prefix
            .join("lib")
            .join("node_modules")
            .join("@larksuite")
            .join("cli")
    }
}

fn feishu_bundle_prefix(
    bundle: &super::pinned_npm::PinnedBundleInstall,
) -> Result<PathBuf, String> {
    let launcher = bundle
        .launcher_paths
        .first()
        .ok_or("飞书 bundle 没有 launcher")?;
    let launcher_parent = launcher.parent().ok_or("飞书 launcher 没有父目录")?;
    let prefix = if cfg!(windows) {
        launcher_parent.to_path_buf()
    } else {
        launcher_parent
            .parent()
            .ok_or("飞书 Unix launcher 没有 prefix")?
            .to_path_buf()
    };
    if feishu_package_dir(&prefix) != bundle.package_dir
        || bundle
            .launcher_paths
            .iter()
            .any(|path| !path_is_within(&path.display().to_string(), &feishu_bin_dir(&prefix)))
    {
        return Err("飞书 bundle staging 路径不在同一受控 prefix".into());
    }
    Ok(prefix)
}

/// 白名单：只这些可一键装。
fn catalog() -> Vec<ExtensionItem> {
    vec![
        ExtensionItem {
            id: "skill-explain".into(),
            kind: "skill".into(),
            name: "代码解释".into(),
            description: "用中文解释当前文件/函数，适合新手读代码".into(),
            risk: "低 · 仅写本地 SKILL.md，无网络".into(),
            source: "builtin:skill-explain".into(),
            installed: false,
            owned_by_tool: false,
            can_uninstall: false,
            detail: None,
        },
        ExtensionItem {
            id: "skill-bugfix".into(),
            kind: "skill".into(),
            name: "Bug 排查".into(),
            description: "结构化排查：复现 → 假设 → 验证 → 最小修复".into(),
            risk: "低 · 仅写本地 SKILL.md".into(),
            source: "builtin:skill-bugfix".into(),
            installed: false,
            owned_by_tool: false,
            can_uninstall: false,
            detail: None,
        },
        ExtensionItem {
            id: "skill-pr".into(),
            kind: "skill".into(),
            name: "PR 描述".into(),
            description: "根据 diff 生成 PR 标题与说明草稿".into(),
            risk: "低 · 仅写本地 SKILL.md".into(),
            source: "builtin:skill-pr".into(),
            installed: false,
            owned_by_tool: false,
            can_uninstall: false,
            detail: None,
        },
        ExtensionItem {
            id: "mcp-filesystem-note".into(),
            kind: "mcp".into(),
            name: "MCP 安全说明".into(),
            description: "不自动装任意 MCP。只提供官方安全指引与手动添加模板".into(),
            risk: "无 · 不执行远程包".into(),
            source: "docs".into(),
            installed: false,
            owned_by_tool: false,
            can_uninstall: false,
            detail: None,
        },
        ExtensionItem {
            id: "cli-feishu".into(),
            kind: "cli".into(),
            name: "飞书 CLI (lark-cli)".into(),
            description: "可选：从官方固定 tarball 安装到 CodeCLI 独占目录（需自行登录授权）"
                .into(),
            risk:
                "中 · 固定 SRI bundle 仅执行已审计下载器并校验原生程序；装后仍需扫码并授权组织数据"
                    .into(),
            source: format!(
                "registry.npmjs.org:{FEISHU_PACKAGE}@{FEISHU_PACKAGE_VERSION} + SHA-512 SRI"
            ),
            installed: false,
            owned_by_tool: false,
            can_uninstall: false,
            detail: None,
        },
    ]
}

fn skill_body(id: &str) -> Option<(&'static str, &'static str)> {
    match id {
        "skill-explain" => Some((
            "code-explain",
            r#"---
name: code-explain
description: 用中文解释代码
---

# 代码解释

当用户要求解释代码时：

1. 先定位文件与符号
2. 用中文说明「做什么 / 为什么 / 关键路径」
3. 标出边界情况与风险
4. 不擅自大改代码，除非用户明确要求修改
"#,
        )),
        "skill-bugfix" => Some((
            "bug-fix",
            r#"---
name: bug-fix
description: 结构化 Bug 排查
---

# Bug 排查

1. 复现步骤与期望/实际
2. 列 2–3 个假设，按廉价验证排序
3. 用日志/最小复现验证
4. 最小修复 + 回归点
5. 不猜，不扩大改动面
"#,
        )),
        "skill-pr" => Some((
            "pr-write",
            r#"---
name: pr-write
description: 根据 diff 写 PR 说明
---

# PR 描述

输出：

- 标题（≤70 字）
- 背景 / 改动 / 测试 / 风险
- 中文优先，技术术语可英文
"#,
        )),
        _ => None,
    }
}

fn mcp_note_body() -> Result<String, String> {
    let body = serde_json::json!({
        "updatedAt": chrono_like_now(),
        "policy": "whitelist-only",
        "note": "CodeCLI 不自动安装任意 MCP。请用官方 claude mcp 命令手动添加，并阅读权限说明。",
        "docs": [
            "https://modelcontextprotocol.io/docs/tutorials/security/security_best_practices",
            "https://docs.anthropic.com/en/docs/claude-code/mcp"
        ],
        "manualTemplate": {
            "command": "claude mcp add --help",
            "warning": "任意 npx/远程 MCP 可能读文件或执行命令，务必确认来源"
        }
    });
    serde_json::to_string_pretty(&body).map_err(|error| error.to_string())
}

fn ownership_path() -> Option<PathBuf> {
    codecli_state_dir().map(|d| d.join("extensions-owned.json"))
}

fn known_extension_id(id: &str) -> bool {
    matches!(
        id,
        "skill-explain" | "skill-bugfix" | "skill-pr" | "mcp-filesystem-note" | "cli-feishu"
    )
}

fn expected_kind(id: &str) -> Option<&'static str> {
    match id {
        "skill-explain" | "skill-bugfix" | "skill-pr" => Some("skill"),
        "mcp-filesystem-note" => Some("mcp"),
        "cli-feishu" => Some("cli"),
        _ => None,
    }
}

fn expected_target(id: &str) -> Result<PathBuf, String> {
    match id {
        "skill-explain" | "skill-bugfix" | "skill-pr" => {
            let (slug, _) = skill_body(id).ok_or("未知 Skill")?;
            Ok(skills_dir()
                .ok_or("找不到 ~/.claude")?
                .join(slug)
                .join("SKILL.md"))
        }
        "mcp-filesystem-note" => mcp_config_path().ok_or("找不到状态目录".into()),
        "cli-feishu" => feishu_prefix(),
        _ => Err(format!("不在白名单: {id}")),
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn pending_marker_fingerprint() -> String {
    sha256_hex(FEISHU_PENDING_MARKER_BODY.as_bytes())
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn new_file_record(
    id: &str,
    state: OwnershipState,
    fingerprint: String,
) -> Result<OwnershipRecord, String> {
    Ok(OwnershipRecord {
        state,
        kind: expected_kind(id).ok_or("未知扩展")?.into(),
        target: Some(expected_target(id)?.display().to_string()),
        fingerprint: Some(fingerprint),
        package: None,
        receipt: None,
    })
}

fn new_feishu_record(
    state: OwnershipState,
    fingerprint: Option<String>,
    receipt: Option<super::pinned_npm::PinnedBundleReceipt>,
) -> Result<OwnershipRecord, String> {
    match state {
        OwnershipState::Pending
            if fingerprint.as_deref() != Some(pending_marker_fingerprint().as_str()) =>
        {
            return Err("飞书 CLI Pending ownership 缺少精确 reservation marker 指纹".into());
        }
        OwnershipState::Installed | OwnershipState::Uninstalling
            if !fingerprint.as_deref().map(valid_sha256).unwrap_or(false) || receipt.is_none() =>
        {
            return Err(
                "飞书 CLI Installed/Uninstalling ownership 缺少可信整树指纹或发布收据".into(),
            );
        }
        OwnershipState::CleanupPending
            if fingerprint.as_deref() != Some(pending_marker_fingerprint().as_str())
                || receipt.is_some() =>
        {
            return Err("飞书 CLI CleanupPending 缺少精确 marker 指纹".into());
        }
        OwnershipState::Legacy => return Err("飞书 CLI legacy 记录必须由专用迁移创建".into()),
        _ => {}
    }
    if let Some(receipt) = &receipt {
        super::pinned_npm::validate_receipt_shape(
            &feishu_prefix()?,
            FEISHU_PACKAGE,
            FEISHU_COMMAND,
            receipt,
        )?;
    }
    Ok(OwnershipRecord {
        state,
        kind: "cli".into(),
        target: Some(feishu_prefix()?.display().to_string()),
        fingerprint,
        package: Some(FEISHU_PACKAGE.into()),
        receipt,
    })
}

fn legacy_record(id: &str) -> Result<OwnershipRecord, String> {
    Ok(OwnershipRecord {
        state: OwnershipState::Legacy,
        kind: expected_kind(id).ok_or("未知扩展")?.into(),
        target: None,
        fingerprint: None,
        package: None,
        receipt: None,
    })
}

fn validate_store(store: &OwnershipStore) -> Result<(), String> {
    if store.version != OWNERSHIP_VERSION {
        return Err(format!(
            "扩展 ownership 版本 {} 不受支持，已拒绝自动安装/删除",
            store.version
        ));
    }
    for (id, record) in &store.items {
        if !known_extension_id(id) {
            return Err(format!(
                "扩展 ownership 含未知条目 {id}，已拒绝自动安装/删除"
            ));
        }
        if record.kind != expected_kind(id).unwrap_or_default() {
            return Err(format!("扩展 {id} ownership 类型不匹配，已拒绝操作"));
        }
        if record.state == OwnershipState::Legacy {
            if record.target.is_some()
                || record.fingerprint.is_some()
                || record.package.is_some()
                || record.receipt.is_some()
            {
                return Err(format!("扩展 {id} legacy ownership 结构异常"));
            }
            continue;
        }
        let expected = expected_target(id)?.display().to_string();
        if record.target.as_deref() != Some(expected.as_str()) {
            return Err(format!("扩展 {id} ownership 目标路径被篡改，已拒绝操作"));
        }
        if id == "cli-feishu" {
            let fingerprint_ok = match record.state {
                OwnershipState::Pending => {
                    record.fingerprint.as_deref() == Some(pending_marker_fingerprint().as_str())
                }
                OwnershipState::Installed | OwnershipState::Uninstalling => record
                    .fingerprint
                    .as_deref()
                    .map(valid_sha256)
                    .unwrap_or(false),
                OwnershipState::CleanupPending => {
                    record.fingerprint.as_deref() == Some(pending_marker_fingerprint().as_str())
                }
                OwnershipState::Legacy => unreachable!("legacy handled above"),
            };
            let receipt_ok = match record.state {
                OwnershipState::Pending => true,
                OwnershipState::Installed | OwnershipState::Uninstalling => {
                    record.receipt.is_some()
                }
                OwnershipState::CleanupPending | OwnershipState::Legacy => record.receipt.is_none(),
            };
            if record.package.as_deref() != Some(FEISHU_PACKAGE) || !fingerprint_ok || !receipt_ok {
                return Err("飞书 CLI ownership 包名/指纹异常，已拒绝操作".into());
            }
            if let Some(receipt) = &record.receipt {
                super::pinned_npm::validate_receipt_shape(
                    &feishu_prefix()?,
                    FEISHU_PACKAGE,
                    FEISHU_COMMAND,
                    receipt,
                )?;
            }
        } else if matches!(
            record.state,
            OwnershipState::Uninstalling | OwnershipState::CleanupPending
        ) || record.package.is_some()
            || record.receipt.is_some()
            || !record
                .fingerprint
                .as_deref()
                .map(valid_sha256)
                .unwrap_or(false)
        {
            return Err(format!("扩展 {id} ownership 内容指纹异常，已拒绝操作"));
        }
    }
    Ok(())
}

fn parse_owned_body(raw: &str) -> Result<OwnershipStore, String> {
    if let Ok(mut store) = serde_json::from_str::<OwnershipStore>(raw) {
        if store.version == 2 {
            // v2 没有首个 final rename 前持久化的 bundle receipt。
            // marker-only Pending/CleanupPending 仍可按最小权限恢复；
            // Installed/Uninstalling 绝不把旧整树 hash 冒充新收据。
            let downgrade_feishu = store.items.get("cli-feishu").is_some_and(|record| {
                matches!(
                    record.state,
                    OwnershipState::Installed | OwnershipState::Uninstalling
                ) || record.receipt.is_some()
                    || (record.state == OwnershipState::Pending
                        && record.fingerprint.as_deref()
                            != Some(pending_marker_fingerprint().as_str()))
            });
            if downgrade_feishu {
                store
                    .items
                    .insert("cli-feishu".into(), legacy_record("cli-feishu")?);
            }
            store.version = OWNERSHIP_VERSION;
        }
        validate_store(&store)?;
        return Ok(store);
    }

    // v1 是 ID 集合。只迁移成不可直接删除的 Legacy；后续必须用实际内容证明归属。
    let ids: BTreeSet<String> = serde_json::from_str(raw)
        .map_err(|error| format!("扩展 ownership 损坏，已拒绝继续: {error}"))?;
    let mut store = OwnershipStore::default();
    for id in ids {
        if !known_extension_id(&id) {
            return Err(format!(
                "扩展 ownership 含未知条目 {id}，已拒绝自动安装/删除"
            ));
        }
        store.items.insert(id.clone(), legacy_record(&id)?);
    }
    validate_store(&store)?;
    Ok(store)
}

fn read_small_regular_file(path: &Path, label: &str) -> Result<Option<String>, String> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("读取 {label} 元数据失败: {error}")),
    };
    if metadata.file_type().is_symlink() || metadata_is_reparse(&metadata) || !metadata.is_file() {
        return Err(format!("{label} 不是可信普通文件，已拒绝操作"));
    }
    if metadata.len() > MAX_OWNERSHIP_BYTES {
        return Err(format!("{label} 过大，已拒绝操作"));
    }

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
    let file = options
        .open(path)
        .map_err(|error| format!("安全打开 {label} 失败: {error}"))?;
    let opened = file
        .metadata()
        .map_err(|error| format!("复查 {label} 失败: {error}"))?;
    if metadata_is_reparse(&opened) || !opened.is_file() || opened.len() > MAX_OWNERSHIP_BYTES {
        return Err(format!("{label} 打开后不是可信普通小文件"));
    }
    let mut bytes = Vec::new();
    file.take(MAX_OWNERSHIP_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("读取 {label} 失败: {error}"))?;
    if bytes.len() as u64 > MAX_OWNERSHIP_BYTES {
        return Err(format!("{label} 读取期间变大"));
    }
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| format!("{label} 不是 UTF-8"))
}

fn file_fingerprint(path: &Path, label: &str) -> Result<Option<String>, String> {
    Ok(read_small_regular_file(path, label)?.map(|body| sha256_hex(body.as_bytes())))
}

fn load_owned() -> Result<OwnershipStore, String> {
    let Some(path) = ownership_path() else {
        return Ok(Default::default());
    };
    let Some(raw) = read_small_regular_file(&path, "extensions-owned.json")? else {
        return Ok(Default::default());
    };
    parse_owned_body(&raw)
}

#[cfg(windows)]
fn metadata_is_reparse(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn metadata_is_reparse(_metadata: &std::fs::Metadata) -> bool {
    false
}

fn ensure_real_directory(path: &Path, label: &str) -> Result<(), String> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink()
                || metadata_is_reparse(&metadata)
                || !metadata.is_dir()
            {
                return Err(format!("{label} 不是可信真实目录，已拒绝操作"));
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(path).map_err(|error| format!("创建 {label} 失败: {error}"))?;
            let metadata = std::fs::symlink_metadata(path)
                .map_err(|error| format!("复查 {label} 失败: {error}"))?;
            if metadata.file_type().is_symlink()
                || metadata_is_reparse(&metadata)
                || !metadata.is_dir()
            {
                return Err(format!("{label} 创建后不是可信真实目录"));
            }
        }
        Err(error) => return Err(format!("检查 {label} 失败: {error}")),
    }
    Ok(())
}

fn ensure_private_directory(path: &Path, label: &str) -> Result<(), String> {
    ensure_real_directory(path, label)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("收紧 {label} 权限失败: {error}"))?;
    }
    Ok(())
}

fn trusted_directory_exists(path: &Path, label: &str) -> Result<bool, String> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink()
                || metadata_is_reparse(&metadata)
                || !metadata.is_dir()
            {
                Err(format!("{label} 不是可信真实目录，已拒绝操作"))
            } else {
                Ok(true)
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!("检查 {label} 失败: {error}")),
    }
}

fn load_feishu_package_json(prefix: &Path) -> Result<Option<(PathBuf, serde_json::Value)>, String> {
    if !trusted_directory_exists(prefix, "飞书 CLI 独占 prefix")? {
        return Ok(None);
    }
    let components: &[&str] = if cfg!(windows) {
        &["node_modules", "@larksuite", "cli"]
    } else {
        &["lib", "node_modules", "@larksuite", "cli"]
    };
    let mut current = prefix.to_path_buf();
    for (index, component) in components.iter().enumerate() {
        current.push(component);
        if !trusted_directory_exists(&current, "飞书 CLI npm 包目录")? {
            return Ok(None);
        }
        if index + 1 == components.len() {
            let package_json = current.join("package.json");
            let Some(raw) = read_small_regular_file(&package_json, "飞书 CLI package.json")?
            else {
                return Ok(None);
            };
            let value: serde_json::Value = serde_json::from_str(&raw)
                .map_err(|error| format!("飞书 CLI package.json 损坏: {error}"))?;
            if value.get("name").and_then(serde_json::Value::as_str) != Some(FEISHU_PACKAGE) {
                return Err("飞书 CLI package.json 包名不匹配，已拒绝取得归属".into());
            }
            return Ok(Some((current, value)));
        }
    }
    Ok(None)
}

fn validate_package_chain(prefix: &Path) -> Result<bool, String> {
    Ok(load_feishu_package_json(prefix)?.is_some())
}

fn join_relative_within(root: &Path, start: &Path, raw: &Path) -> Result<PathBuf, String> {
    if raw.is_absolute() || !path_is_within(&start.display().to_string(), root) {
        return Err("飞书 CLI 路径不是 prefix 内可信相对路径".into());
    }
    let mut current = start.to_path_buf();
    for component in raw.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(value) => current.push(value),
            Component::ParentDir => {
                if normalized_path(&current.display().to_string())
                    == normalized_path(&root.display().to_string())
                {
                    return Err("飞书 CLI 路径试图越出独占 prefix".into());
                }
                current.pop();
            }
            Component::Prefix(_) | Component::RootDir => {
                return Err("飞书 CLI 路径含绝对路径前缀".into());
            }
        }
    }
    if !path_is_within(&current.display().to_string(), root) {
        return Err("飞书 CLI 路径越出独占 prefix".into());
    }
    Ok(current)
}

fn normalize_absolute_lexically(raw: &Path) -> Result<PathBuf, String> {
    if !raw.is_absolute() {
        return Err("飞书 CLI 符号链接目标不是绝对路径".into());
    }
    let mut normalized = PathBuf::new();
    for component in raw.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::Normal(value) => normalized.push(value),
            Component::ParentDir => {
                if !normalized.pop() {
                    return Err("飞书 CLI 绝对链接目标越过文件系统根".into());
                }
            }
        }
    }
    Ok(normalized)
}

fn ensure_real_directory_chain(root: &Path, directory: &Path) -> Result<(), String> {
    let relative = directory
        .strip_prefix(root)
        .map_err(|_| "飞书 CLI launcher 父目录越出 npm 包目录")?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(value) = component else {
            return Err("飞书 CLI launcher 路径结构异常".into());
        };
        current.push(value);
        if !trusted_directory_exists(&current, "飞书 CLI launcher 父目录")? {
            return Err("飞书 CLI launcher 父目录不存在".into());
        }
    }
    Ok(())
}

fn trusted_regular_file(path: &Path, label: &str, max_bytes: u64) -> Result<(), String> {
    let metadata =
        std::fs::symlink_metadata(path).map_err(|error| format!("检查 {label} 失败: {error}"))?;
    if metadata.file_type().is_symlink()
        || metadata_is_reparse(&metadata)
        || !metadata.is_file()
        || metadata.len() == 0
        || metadata.len() > max_bytes
    {
        return Err(format!("{label} 不是可信有界普通文件"));
    }
    Ok(())
}

#[derive(Debug, Clone)]
struct FeishuInstallProof {
    package_dir: PathBuf,
    package_version: String,
    package_bin_target: PathBuf,
    native_binary: PathBuf,
    global_launcher: PathBuf,
    postinstall: Option<String>,
}

fn feishu_bin_target(value: &serde_json::Value) -> Result<&str, String> {
    let bin = value.get("bin").ok_or("飞书 CLI package.json 缺少 bin")?;
    bin.as_object()
        .and_then(|items| items.get("lark-cli"))
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| "飞书 CLI package.json 未声明 lark-cli launcher".into())
}

fn validate_feishu_install(prefix: &Path) -> Result<Option<FeishuInstallProof>, String> {
    let Some((package_dir, package_json)) = load_feishu_package_json(prefix)? else {
        return Ok(None);
    };
    let package_version = package_json
        .get("version")
        .and_then(serde_json::Value::as_str)
        .filter(|value| {
            !value.trim().is_empty()
                && value.len() <= 64
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'+'))
        })
        .ok_or("飞书 CLI package.json 缺少可信版本号")?
        .to_string();
    let postinstall = package_json
        .get("scripts")
        .and_then(|value| value.get("postinstall"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    let bin_relative = Path::new(feishu_bin_target(&package_json)?);
    let package_bin_target = join_relative_within(&package_dir, &package_dir, bin_relative)?;
    let parent = package_bin_target
        .parent()
        .ok_or("飞书 CLI package bin 没有父目录")?;
    ensure_real_directory_chain(&package_dir, parent)?;
    trusted_regular_file(
        &package_bin_target,
        "飞书 CLI package bin",
        MAX_FEISHU_TREE_FILE_BYTES,
    )?;

    let bin_dir = feishu_bin_dir(prefix);
    if !trusted_directory_exists(&bin_dir, "飞书 CLI 全局 bin 目录")? {
        return Err("飞书 CLI 全局 bin 目录不存在".into());
    }
    #[cfg(not(windows))]
    let global_launcher = {
        let launcher = bin_dir.join("lark-cli");
        let metadata = std::fs::symlink_metadata(&launcher)
            .map_err(|error| format!("检查飞书 CLI 全局 launcher 失败: {error}"))?;
        if !metadata.file_type().is_symlink() {
            return Err("飞书 CLI 全局 launcher 不是 npm 创建的可信符号链接".into());
        }
        let target = std::fs::read_link(&launcher)
            .map_err(|error| format!("读取飞书 CLI 全局 launcher 失败: {error}"))?;
        let resolved = join_relative_within(prefix, &bin_dir, &target)?;
        if normalized_path(&resolved.display().to_string())
            != normalized_path(&package_bin_target.display().to_string())
        {
            return Err("飞书 CLI 全局 launcher 未指向 package.json 声明的 bin".into());
        }
        let canonical_root = std::fs::canonicalize(prefix)
            .map_err(|error| format!("解析飞书 CLI prefix 失败: {error}"))?;
        let canonical_target = std::fs::canonicalize(&resolved)
            .map_err(|error| format!("解析飞书 CLI launcher 目标失败: {error}"))?;
        if !path_is_within(&canonical_target.display().to_string(), &canonical_root) {
            return Err("飞书 CLI 全局 launcher 最终越出独占 prefix".into());
        }
        launcher
    };
    #[cfg(windows)]
    let global_launcher = {
        let launcher = bin_dir.join("lark-cli.cmd");
        // PowerShell 可优先命中 .ps1，Git Bash/WSL 会命中无扩展名
        // sh launcher；三份必须同时存在且整文件匹配已审计
        // cmd-shim 模板，不能只验 .cmd。
        validate_cli_launchers(prefix, "lark-cli", &package_bin_target)?;
        launcher
    };

    let native_binary = package_dir.join("bin").join(if cfg!(windows) {
        "lark-cli.exe"
    } else {
        "lark-cli"
    });

    Ok(Some(FeishuInstallProof {
        package_dir,
        package_version,
        package_bin_target,
        native_binary,
        global_launcher,
        postinstall,
    }))
}

fn require_file_fingerprint(path: &Path, label: &str, expected: &str) -> Result<(), String> {
    let actual = file_fingerprint(path, label)?.ok_or_else(|| format!("{label} 不存在"))?;
    if actual != expected {
        return Err(format!("{label} 指纹不匹配，已拒绝执行"));
    }
    Ok(())
}

/// 只对白名单固定版本中已人工审计过的下载器放行。npm lifecycle 始终关闭，
/// 因而这里是唯一会执行的包内 JavaScript。
fn validate_pinned_feishu_support_files(proof: &FeishuInstallProof) -> Result<PathBuf, String> {
    if proof.package_version != FEISHU_PACKAGE_VERSION {
        return Err(format!(
            "飞书 CLI 包版本 {} 不在本安装器白名单（仅支持 {}）",
            proof.package_version, FEISHU_PACKAGE_VERSION
        ));
    }
    if proof.postinstall.as_deref() != Some("node scripts/install.js") {
        return Err("飞书 CLI postinstall 声明与已审计版本不一致".into());
    }
    let expected_wrapper = proof.package_dir.join("scripts").join("run.js");
    if normalized_path(&proof.package_bin_target.display().to_string())
        != normalized_path(&expected_wrapper.display().to_string())
    {
        return Err("飞书 CLI launcher 路径与已审计版本不一致".into());
    }
    require_file_fingerprint(
        &proof.package_bin_target,
        "飞书 CLI run.js",
        FEISHU_RUN_SCRIPT_SHA256,
    )?;
    let install_script = proof.package_dir.join("scripts").join("install.js");
    require_file_fingerprint(
        &install_script,
        "飞书 CLI install.js",
        FEISHU_INSTALL_SCRIPT_SHA256,
    )?;
    require_file_fingerprint(
        &proof.package_dir.join("checksums.txt"),
        "飞书 CLI checksums.txt",
        FEISHU_CHECKSUMS_SHA256,
    )?;
    Ok(install_script)
}

/// `Ok(false)` 仅表示原生二进制尚未生成，可安全运行固定版 install.js；
/// 已存在但为链接、reparse、空文件、过大或不可执行时一律 fail closed。
fn validate_feishu_native_binary(proof: &FeishuInstallProof) -> Result<bool, String> {
    let bin_dir = proof
        .native_binary
        .parent()
        .ok_or("飞书 CLI 原生程序没有父目录")?;
    match std::fs::symlink_metadata(bin_dir) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink()
                || metadata_is_reparse(&metadata)
                || !metadata.is_dir()
            {
                return Err("飞书 CLI 原生程序目录不是可信真实目录".into());
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            // install.js 只会在已经逐层验证为真实目录的 package_dir
            // 下创建这个固定 bin 子目录。
        }
        Err(error) => return Err(format!("检查飞书 CLI 原生程序目录失败: {error}")),
    }
    let metadata = match std::fs::symlink_metadata(&proof.native_binary) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(format!("检查飞书 CLI 原生程序失败: {error}")),
    };
    if metadata.file_type().is_symlink()
        || metadata_is_reparse(&metadata)
        || !metadata.is_file()
        || metadata.len() == 0
        || metadata.len() > MAX_FEISHU_TREE_FILE_BYTES
    {
        return Err("飞书 CLI 原生程序不是可信有界普通文件".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            return Err("飞书 CLI 原生程序没有执行权限".into());
        }
    }
    Ok(true)
}

/// Pending 记录尚未保存任何 native 指纹，因此绝不能执行已存在的
/// native。只允许先 unlink 固定位置的普通文件，再由已 pin 的
/// install.js + checksums.txt 重新生成；unlink hard-link 也不会改写
/// 其外部 inode。链接/reparse/目录则 fail closed，避免越界写入。
fn prepare_feishu_native_destination(proof: &FeishuInstallProof) -> Result<(), String> {
    let bin_dir = proof
        .native_binary
        .parent()
        .ok_or("飞书 CLI 原生程序没有父目录")?;
    match std::fs::symlink_metadata(bin_dir) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink()
                || metadata_is_reparse(&metadata)
                || !metadata.is_dir()
            {
                return Err("飞书 CLI 原生程序目录不是可信真实目录，拒绝运行下载器".into());
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("检查飞书 CLI 原生程序目录失败: {error}")),
    }

    match std::fs::symlink_metadata(&proof.native_binary) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink()
                || metadata_is_reparse(&metadata)
                || !metadata.is_file()
            {
                return Err("飞书 CLI 预存 native 不是普通文件；拒绝覆盖或执行".into());
            }
            remove_file_durable(&proof.native_binary)
                .map_err(|error| format!("清除未证明的飞书 CLI native 失败: {error}"))?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("检查飞书 CLI 预存 native 失败: {error}")),
    }
    match std::fs::symlink_metadata(&proof.native_binary) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Ok(_) => return Err("飞书 CLI 未证明的 native 清除后仍存在".into()),
        Err(error) => return Err(format!("复查飞书 CLI native 清除结果失败: {error}")),
    }
    Ok(())
}

fn verify_feishu_native_version(proof: &FeishuInstallProof) -> Result<(), String> {
    if !validate_feishu_native_binary(proof)? {
        return Err("飞书 CLI 原生程序尚未安装".into());
    }
    let mut command = Command::new(&proof.native_binary);
    command.arg("--version");
    let output = run_timed(command, 30)?;
    if !output.status_ok {
        return Err(format!(
            "飞书 CLI 原生程序 --version 失败: {}",
            output.stderr.chars().take(320).collect::<String>()
        ));
    }
    let expected = format!("lark-cli version {}", proof.package_version);
    if !output.stdout.lines().any(|line| line.trim() == expected) {
        return Err(format!(
            "飞书 CLI 原生程序版本输出与 npm 包不一致（期望 {expected}）"
        ));
    }
    Ok(())
}

fn run_pinned_feishu_postinstall(proof: &FeishuInstallProof) -> Result<(), String> {
    let install_script = validate_pinned_feishu_support_files(proof)?;
    let node = which_cmd("node").ok_or("PATH 中找不到 node，无法运行固定版飞书安装器")?;
    let mut command = Command::new(node);
    command.arg(install_script).env("LARK_CLI_RUN", "true");
    let output = run_timed(command, 600)?;
    if !output.status_ok {
        return Err(format!(
            "固定版飞书 CLI 原生程序下载/校验失败；Pending ownership 已保留: {}",
            output.stderr.chars().take(320).collect::<String>()
        ));
    }
    Ok(())
}

/// 固定 install.js 返回成功后，不能直接把其新建文件记为 Installed。
/// 把已验证为普通文件的 native 复制到同目录私有临时文件，先 fsync
/// 内容/权限，再用平台原子 durable replace 重新发布，关闭脚本创建文件
/// 与 ownership 提交之间的断电窗口。
fn durably_republish_feishu_native(proof: &FeishuInstallProof) -> Result<(), String> {
    if !validate_feishu_native_binary(proof)? {
        return Err("飞书 CLI 原生程序尚未生成，无法持久发布".into());
    }
    let mut source = open_regular_file_nofollow(&proof.native_binary, "飞书 CLI 原生程序")?;
    let source_metadata = source
        .metadata()
        .map_err(|error| format!("复查飞书 CLI 原生程序失败: {error}"))?;
    if metadata_is_reparse(&source_metadata)
        || !source_metadata.is_file()
        || source_metadata.len() == 0
        || source_metadata.len() > MAX_FEISHU_TREE_FILE_BYTES
    {
        return Err("飞书 CLI 原生程序打开后类型/大小发生变化".into());
    }
    let parent = proof
        .native_binary
        .parent()
        .ok_or("飞书 CLI 原生程序没有父目录")?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temp = parent.join(format!(
        ".codecli-native-durable-{}-{nonce}",
        std::process::id()
    ));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o700).custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let mut output = options
        .open(&temp)
        .map_err(|error| format!("创建飞书 CLI durable native 临时文件失败: {error}"))?;
    let copied = std::io::copy(&mut source, &mut output)
        .map_err(|error| format!("复制飞书 CLI native 失败: {error}"))?;
    if copied != source_metadata.len() {
        let _ = remove_file_durable(&temp);
        return Err("飞书 CLI native 复制长度不一致".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o755))
            .map_err(|error| format!("设置飞书 CLI durable native 权限失败: {error}"))?;
    }
    if let Err(error) = output.sync_all() {
        drop(output);
        let _ = remove_file_durable(&temp);
        return Err(format!("持久化飞书 CLI durable native 失败: {error}"));
    }
    drop(output);
    if let Err(error) = atomic_replace_file(&temp, &proof.native_binary) {
        let cleanup = remove_file_durable(&temp).err();
        return Err(format!(
            "原子发布飞书 CLI durable native 失败: {error}{}",
            cleanup
                .map(|value| format!("；清理临时文件也失败: {value}"))
                .unwrap_or_default()
        ));
    }
    if !validate_feishu_native_binary(proof)? {
        return Err("飞书 CLI durable native 发布后复验失败".into());
    }
    // Unix atomic_replace 已同步 native 父目录；再同步 package_dir 中
    // bin 目录项。Windows replace 使用 MOVEFILE_WRITE_THROUGH。
    sync_parent_dir(parent).map_err(|error| format!("持久化飞书 CLI package 目录失败: {error}"))
}

fn validate_feishu_launcher_on_path(prefix: &Path) -> Result<(), String> {
    let found = which_cmd("lark-cli").ok_or("PATH 中找不到刚安装的 lark-cli launcher")?;
    let bin = feishu_bin_dir(prefix);
    if !path_is_within(&found, &bin) {
        return Err(format!(
            "PATH 中 lark-cli 指向非 CodeCLI 目录 {found}，已拒绝宣告安装成功"
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FeishuTreeManifest {
    fingerprint: String,
    entries: usize,
    total_file_bytes: u64,
}

fn hash_manifest_field(hasher: &mut Sha256, tag: &[u8], value: &[u8]) {
    hasher.update(tag);
    hasher.update((value.len() as u64).to_le_bytes());
    hasher.update(value);
}

fn open_regular_file_nofollow(path: &Path, label: &str) -> Result<std::fs::File, String> {
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
    options
        .open(path)
        .map_err(|error| format!("安全打开 {label} 失败: {error}"))
}

struct FeishuWalkContext<'a> {
    root: &'a Path,
    canonical_root: &'a Path,
    allow_dangling_internal_symlink: bool,
    ignore_pending_marker: bool,
}

fn walk_feishu_tree(
    context: &FeishuWalkContext<'_>,
    directory: &Path,
    depth: usize,
    hasher: &mut Sha256,
    entries: &mut usize,
    total_file_bytes: &mut u64,
) -> Result<(), String> {
    if depth > MAX_FEISHU_TREE_DEPTH {
        return Err("飞书 CLI 文件树深度超限，已拒绝取得删除权".into());
    }
    let mut children = Vec::new();
    for entry in
        std::fs::read_dir(directory).map_err(|error| format!("读取飞书 CLI 文件树失败: {error}"))?
    {
        let entry = entry.map_err(|error| format!("读取飞书 CLI 文件树条目失败: {error}"))?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| "飞书 CLI 文件树含非 UTF-8 文件名，已拒绝取得删除权")?;
        children.push((name, entry.path()));
    }
    children.sort_by(|left, right| left.0.cmp(&right.0));

    for (name, path) in children {
        if depth == 0 && context.ignore_pending_marker && name == FEISHU_PENDING_MARKER {
            continue;
        }
        *entries = entries
            .checked_add(1)
            .ok_or("飞书 CLI 文件树条目计数溢出")?;
        if *entries > MAX_FEISHU_TREE_ENTRIES {
            return Err("飞书 CLI 文件树条目数超限，已拒绝取得删除权".into());
        }
        let relative = path
            .strip_prefix(context.root)
            .map_err(|_| "飞书 CLI 文件树条目越出 prefix")?
            .to_str()
            .ok_or("飞书 CLI 文件树路径不是 UTF-8")?
            .replace('\\', "/");
        let metadata = std::fs::symlink_metadata(&path)
            .map_err(|error| format!("检查飞书 CLI 文件树条目失败: {error}"))?;
        if metadata_is_reparse(&metadata) {
            return Err(format!(
                "飞书 CLI 文件树含 Windows reparse 条目 {relative}，已拒绝取得删除权"
            ));
        }
        if metadata.file_type().is_symlink() {
            let target = std::fs::read_link(&path)
                .map_err(|error| format!("读取飞书 CLI 符号链接失败: {error}"))?;
            let candidate = if target.is_absolute() {
                let normalized_target = normalize_absolute_lexically(&target)?;
                if !path_is_within(&normalized_target.display().to_string(), context.root) {
                    return Err(format!(
                        "飞书 CLI 符号链接 {relative} 词法上越出独占 prefix"
                    ));
                }
                normalized_target
            } else {
                join_relative_within(
                    context.root,
                    path.parent().ok_or("飞书 CLI 符号链接没有父目录")?,
                    &target,
                )?
            };
            match std::fs::canonicalize(&candidate) {
                Ok(canonical_target) => {
                    if !path_is_within(
                        &canonical_target.display().to_string(),
                        context.canonical_root,
                    ) {
                        return Err(format!(
                            "飞书 CLI 符号链接 {relative} 越出独占 prefix，已拒绝取得删除权"
                        ));
                    }
                }
                Err(error)
                    if context.allow_dangling_internal_symlink
                        && error.kind() == std::io::ErrorKind::NotFound =>
                {
                    // 只有显式的兼容扫描才能容忍词法上仍在 prefix 内的
                    // dangling launcher；整树所有权判定与删除始终使用严格模式。
                }
                Err(error) => {
                    return Err(format!("解析飞书 CLI 符号链接失败: {error}"));
                }
            }
            let raw_target = target.to_str().ok_or("飞书 CLI 符号链接目标不是 UTF-8")?;
            hash_manifest_field(hasher, b"L", relative.as_bytes());
            hash_manifest_field(hasher, b"T", raw_target.as_bytes());
        } else if metadata.is_dir() {
            hash_manifest_field(hasher, b"D", relative.as_bytes());
            walk_feishu_tree(context, &path, depth + 1, hasher, entries, total_file_bytes)?;
        } else if metadata.is_file() {
            if metadata.len() > MAX_FEISHU_TREE_FILE_BYTES {
                return Err(format!("飞书 CLI 文件 {relative} 过大，已拒绝取得删除权"));
            }
            *total_file_bytes = total_file_bytes
                .checked_add(metadata.len())
                .ok_or("飞书 CLI 文件树大小溢出")?;
            if *total_file_bytes > MAX_FEISHU_TREE_TOTAL_BYTES {
                return Err("飞书 CLI 文件树总大小超限，已拒绝取得删除权".into());
            }
            hash_manifest_field(hasher, b"F", relative.as_bytes());
            hasher.update(metadata.len().to_le_bytes());
            let mut file = open_regular_file_nofollow(&path, "飞书 CLI 文件树普通文件")?;
            let opened = file
                .metadata()
                .map_err(|error| format!("复查飞书 CLI 文件失败: {error}"))?;
            if !opened.is_file() || opened.len() != metadata.len() {
                return Err("飞书 CLI 文件在生成指纹期间发生变化".into());
            }
            let mut remaining = metadata.len();
            let mut buffer = [0_u8; 64 * 1024];
            while remaining > 0 {
                let wanted = usize::try_from(remaining.min(buffer.len() as u64))
                    .map_err(|_| "飞书 CLI 文件读取长度溢出")?;
                let count = file
                    .read(&mut buffer[..wanted])
                    .map_err(|error| format!("读取飞书 CLI 文件失败: {error}"))?;
                if count == 0 {
                    return Err("飞书 CLI 文件在生成指纹期间被截短".into());
                }
                hasher.update(&buffer[..count]);
                remaining -= count as u64;
            }
            let mut extra = [0_u8; 1];
            if file
                .read(&mut extra)
                .map_err(|error| format!("复查飞书 CLI 文件尾失败: {error}"))?
                != 0
            {
                return Err("飞书 CLI 文件在生成指纹期间变大".into());
            }
        } else {
            return Err(format!(
                "飞书 CLI 文件树含特殊文件 {relative}，已拒绝取得删除权"
            ));
        }
    }
    Ok(())
}

fn feishu_tree_manifest_with_options(
    prefix: &Path,
    allow_dangling_internal_symlink: bool,
    ignore_pending_marker: bool,
) -> Result<FeishuTreeManifest, String> {
    if !trusted_directory_exists(prefix, "飞书 CLI 独占 prefix")? {
        return Err("飞书 CLI 独占 prefix 不存在".into());
    }
    let canonical_root = std::fs::canonicalize(prefix)
        .map_err(|error| format!("解析飞书 CLI 独占 prefix 失败: {error}"))?;
    let mut hasher = Sha256::new();
    hasher.update(b"codecli-feishu-tree-v1\0");
    let mut entries = 0;
    let mut total_file_bytes = 0;
    let context = FeishuWalkContext {
        root: prefix,
        canonical_root: &canonical_root,
        allow_dangling_internal_symlink,
        ignore_pending_marker,
    };
    walk_feishu_tree(
        &context,
        prefix,
        0,
        &mut hasher,
        &mut entries,
        &mut total_file_bytes,
    )?;
    Ok(FeishuTreeManifest {
        fingerprint: hex::encode(hasher.finalize()),
        entries,
        total_file_bytes,
    })
}

fn feishu_tree_manifest(prefix: &Path) -> Result<FeishuTreeManifest, String> {
    feishu_tree_manifest_with_options(prefix, false, false)
}

fn save_owned(store: &OwnershipStore) -> Result<(), String> {
    validate_store(store)?;
    let path = ownership_path().ok_or("无状态目录")?;
    if let Some(parent) = path.parent() {
        ensure_private_directory(parent, "扩展状态目录")?;
    }
    atomic_write_mode(
        &path,
        &serde_json::to_string_pretty(store).map_err(|error| error.to_string())?,
        true,
    )
}

fn reserve_record(
    store: &mut OwnershipStore,
    id: &str,
    record: OwnershipRecord,
) -> Result<(), String> {
    let on_disk = load_owned()?;
    if on_disk != *store {
        return Err("扩展 ownership 在操作期间被外部修改，已拒绝覆盖".into());
    }
    let mut next = store.clone();
    next.items.insert(id.to_string(), record);
    save_owned(&next)?;
    *store = next;
    Ok(())
}

fn remove_record(store: &mut OwnershipStore, id: &str) -> Result<(), String> {
    let on_disk = load_owned()?;
    if on_disk != *store {
        return Err("扩展 ownership 在删除期间被外部修改，已保留记录".into());
    }
    let mut next = store.clone();
    next.items.remove(id);
    save_owned(&next)?;
    *store = next;
    Ok(())
}

fn normalized_path(value: &str) -> String {
    let normalized = value.trim().trim_matches('"').replace('\\', "/");
    if cfg!(windows) {
        normalized.trim_end_matches('/').to_lowercase()
    } else {
        normalized.trim_end_matches('/').to_string()
    }
}

fn path_is_within(candidate: &str, root: &Path) -> bool {
    let candidate = normalized_path(candidate);
    let root = normalized_path(&root.display().to_string());
    candidate == root || candidate.starts_with(&(root + "/"))
}

fn external_feishu_on_path() -> Result<Option<String>, String> {
    let tool_bin = feishu_bin_dir(&feishu_prefix()?);
    for binary in ["lark-cli", "lark"] {
        if let Some(found) = which_cmd(binary) {
            if !path_is_within(&found, &tool_bin) {
                return Ok(Some(found));
            }
        }
    }
    Ok(None)
}

fn external_feishu_install() -> Result<Option<String>, String> {
    let prefix = feishu_prefix()?;
    if let Some(found) = external_feishu_on_path()? {
        return Ok(Some(found));
    }
    if which_cmd("npm").is_none() {
        return Ok(None);
    }

    let mut command = npm_command()?;
    command.args(["root", "-g"]);
    let output = run_timed(command, 30)?;
    if !output.status_ok {
        return Err(format!(
            "无法确认用户全局 npm 包目录，已拒绝抢占飞书 CLI 归属: {}",
            output.stderr.chars().take(240).collect::<String>()
        ));
    }
    let roots = output
        .stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    if roots.len() != 1 {
        return Err("npm root -g 返回异常，无法安全判断飞书 CLI 归属".into());
    }
    let root = PathBuf::from(roots[0]);
    if !root.is_absolute() {
        return Err("npm root -g 未返回绝对路径，已拒绝继续".into());
    }
    let package = root.join("@larksuite").join("cli");
    if normalized_path(&package.display().to_string())
        == normalized_path(&feishu_package_dir(&prefix).display().to_string())
    {
        return Ok(None);
    }
    match std::fs::symlink_metadata(&package) {
        Ok(_) => Ok(Some(package.display().to_string())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("检查用户全局飞书 CLI 失败: {error}")),
    }
}

fn directory_is_empty(path: &Path) -> Result<bool, String> {
    let mut entries = std::fs::read_dir(path)
        .map_err(|error| format!("读取飞书 CLI 独占 prefix 失败: {error}"))?;
    match entries.next() {
        None => Ok(true),
        Some(Ok(_)) => Ok(false),
        Some(Err(error)) => Err(format!("读取飞书 CLI prefix 条目失败: {error}")),
    }
}

fn require_unowned_prefix_empty(prefix: &Path) -> Result<(), String> {
    if trusted_directory_exists(prefix, "飞书 CLI 独占 prefix")? && !directory_is_empty(prefix)?
    {
        return Err("CodeCLI 独占 prefix 已有无归属内容；为避免抢占或覆盖，已拒绝安装".into());
    }
    Ok(())
}

fn pending_marker_path(prefix: &Path) -> PathBuf {
    prefix.join(FEISHU_PENDING_MARKER)
}

fn verify_pending_marker(prefix: &Path) -> Result<bool, String> {
    let marker = pending_marker_path(prefix);
    match file_fingerprint(&marker, "飞书 CLI reservation marker")? {
        None => Ok(false),
        Some(actual) if actual == pending_marker_fingerprint() => Ok(true),
        Some(_) => Err("飞书 CLI reservation marker 内容已变化，已拒绝续传或删除".into()),
    }
}

/// Pending ownership 先于任何 bundle 发布落盘。若恰好在创建 marker
/// 前崩溃，只允许在不存在/空 prefix 中重建；非空且无
/// marker 必须 fail-closed，避免把外部内容追认为本工具事务。
fn ensure_pending_marker(prefix: &Path) -> Result<(), String> {
    ensure_private_directory(prefix, "飞书 CLI 独占 prefix")?;
    if verify_pending_marker(prefix)? {
        return Ok(());
    }
    if !directory_is_empty(prefix)? {
        return Err(
            "飞书 CLI Pending prefix 非空且缺少 reservation marker；已保留并拒绝接管".into(),
        );
    }
    let marker = pending_marker_path(prefix);
    atomic_write_mode(&marker, FEISHU_PENDING_MARKER_BODY, true)?;
    if !verify_pending_marker(prefix)? {
        return Err("飞书 CLI reservation marker 写入后验证失败".into());
    }
    Ok(())
}

fn prefix_has_non_marker_entries(prefix: &Path) -> Result<bool, String> {
    if !trusted_directory_exists(prefix, "飞书 CLI 独占 prefix")? {
        return Ok(false);
    }
    for entry in
        std::fs::read_dir(prefix).map_err(|error| format!("读取飞书 CLI prefix 失败: {error}"))?
    {
        let entry = entry.map_err(|error| format!("读取飞书 CLI prefix 条目失败: {error}"))?;
        if entry.file_name() != FEISHU_PENDING_MARKER {
            return Ok(true);
        }
    }
    Ok(false)
}

fn require_marker_only_pending_tree(prefix: &Path) -> Result<(), String> {
    if !verify_pending_marker(prefix)? {
        return Err("飞书 CLI 事务 prefix 非空且缺少 reservation marker；ownership 已保留".into());
    }
    if prefix_has_non_marker_entries(prefix)? {
        return Err(
            "飞书 CLI Pending/CleanupPending 含非 marker 内容；未知文件均已保留，拒绝递归删除"
                .into(),
        );
    }
    Ok(())
}

fn remove_pending_marker(prefix: &Path) -> Result<bool, String> {
    if !trusted_directory_exists(prefix, "飞书 CLI 独占 prefix")? {
        return Ok(false);
    }
    if !verify_pending_marker(prefix)? {
        return Ok(false);
    }
    let marker = pending_marker_path(prefix);
    remove_file_durable(&marker)
        .map_err(|error| format!("持久删除飞书 CLI reservation marker 失败: {error}"))?;
    Ok(true)
}

/// 只尝试删除一个由固定 npm 布局明确命名的真实空目录。
///
/// 不读取、不递归未知子树；任一祖先变成 symlink/reparse/special entry 时都原样保留，
/// 避免通过用户新增的 sibling 跳出专属 prefix。
fn remove_known_empty_feishu_directory(prefix: &Path, relative: &Path) -> Result<bool, String> {
    if !trusted_directory_exists(prefix, "飞书 CLI 独占 prefix")? {
        return Ok(false);
    }
    let mut current = prefix.to_path_buf();
    for component in relative.components() {
        let Component::Normal(component) = component else {
            return Err("飞书 CLI 已知空骨架路径异常".into());
        };
        current.push(component);
        let metadata = match std::fs::symlink_metadata(&current) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => {
                return Err(format!(
                    "检查飞书 CLI 已知空骨架 {} 失败: {error}",
                    current.display()
                ))
            }
        };
        if metadata.file_type().is_symlink() || metadata_is_reparse(&metadata) || !metadata.is_dir()
        {
            return Ok(false);
        }
    }
    match std::fs::remove_dir(&current) {
        Ok(()) => {
            sync_parent_dir(&current).map_err(|error| {
                format!(
                    "持久删除飞书 CLI 已知空骨架 {} 失败: {error}",
                    current.display()
                )
            })?;
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) if error.kind() == std::io::ErrorKind::DirectoryNotEmpty => Ok(false),
        Err(error) => Err(format!(
            "删除飞书 CLI 已知空骨架 {} 失败: {error}",
            current.display()
        )),
    }
}

/// Bottom-up 只剪固定 npm/launcher 骨架，最后仅在 prefix 自身为空时删除 prefix。
/// 未列出的目录（即使为空）以及列出目录中的任何未知内容都绝不遍历或删除。
fn prune_known_empty_feishu_scaffold(prefix: &Path) -> Result<bool, String> {
    if !trusted_directory_exists(prefix, "飞书 CLI 独占 prefix")? {
        return Ok(false);
    }
    let known: &[&str] = if cfg!(windows) {
        &["node_modules/@larksuite", "node_modules"]
    } else {
        &[
            "lib/node_modules/@larksuite",
            "lib/node_modules",
            "lib",
            "bin",
        ]
    };
    for relative in known {
        let _ = remove_known_empty_feishu_directory(prefix, Path::new(relative))?;
    }
    remove_known_empty_feishu_directory(prefix, Path::new(""))
}

fn prepend_current_path(path: &Path) -> Result<(), String> {
    let existing = std::env::var_os("PATH").unwrap_or_default();
    let mut paths = vec![path.to_path_buf()];
    paths.extend(std::env::split_paths(&existing).filter(|item| {
        normalized_path(&item.display().to_string()) != normalized_path(&path.display().to_string())
    }));
    let joined =
        std::env::join_paths(paths).map_err(|error| format!("更新当前 PATH 失败: {error}"))?;
    unsafe { std::env::set_var("PATH", joined) };
    Ok(())
}

fn ensure_feishu_path(prefix: &Path) -> Result<(), String> {
    let bin = feishu_bin_dir(prefix);
    ensure_real_directory(&bin, "飞书 CLI bin 目录")?;
    prepend_current_path(&bin)?;
    if cfg!(windows) {
        add_user_path_segment_windows(&bin.display().to_string())
    } else {
        ensure_tool_path_block(FEISHU_PATH_TAG, &bin)
    }
}

fn remove_feishu_path(prefix: &Path) -> Result<Vec<String>, String> {
    let bin = feishu_bin_dir(prefix);
    if cfg!(windows) {
        remove_user_path_segment_windows(&bin.display().to_string())?;
        Ok(vec![format!("user-path:{}", bin.display())])
    } else {
        remove_tool_path_block(FEISHU_PATH_TAG)
    }
}

/// 只有 durable Installed 记录能证明同路径文件此前已由本工具成功提交。
/// Legacy 没有内容收据；Pending 也无法区分“create_new 后崩溃”与
/// “外部进程抢先写入完全相同内容”，二者都不得认领既有文件。
fn file_record_may_claim_existing(record: &OwnershipRecord) -> bool {
    record.state == OwnershipState::Installed
}

fn file_install(
    id: &str,
    body: &str,
    store: &mut OwnershipStore,
) -> Result<ExtensionActionResult, String> {
    let path = expected_target(id)?;
    recover_file_quarantine(&path, id)?;
    let desired = sha256_hex(body.as_bytes());
    let current = file_fingerprint(&path, "扩展目标文件")?;
    let existing = store.items.get(id).cloned();

    if let Some(record) = &existing {
        if current.is_some() && !file_record_may_claim_existing(record) {
            match record.state {
                OwnershipState::Legacy | OwnershipState::Pending => {
                    remove_record(store, id)?;
                    return Err(format!(
                        "{} 已存在；{:?} ownership 无法证明该文件由本工具原子创建，即使内容相同也已保留文件并放弃归属",
                        path.display(),
                        record.state
                    ));
                }
                OwnershipState::Uninstalling | OwnershipState::CleanupPending => {
                    return Err("文件扩展出现不可能的卸载事务状态".into());
                }
                OwnershipState::Installed => unreachable!("Installed may claim existing"),
            }
        }
        match record.state {
            OwnershipState::Legacy | OwnershipState::Pending => {}
            OwnershipState::Installed => {
                if let Some(current) = &current {
                    let recorded = record.fingerprint.as_deref().unwrap_or_default();
                    if current != recorded {
                        return Err(format!(
                            "{} 内容已变化，无法证明仍属本工具；已拒绝覆盖",
                            path.display()
                        ));
                    }
                    if current == &desired {
                        return Ok(ExtensionActionResult {
                            ok: true,
                            message: format!("扩展已安装：{}", path.display()),
                            written: vec![path.display().to_string()],
                        });
                    }
                    return Err(format!(
                        "{} 是本工具旧版内容；为避免并发覆盖，请先安全卸载后再安装新版",
                        path.display()
                    ));
                }
            }
            OwnershipState::Uninstalling | OwnershipState::CleanupPending => {
                return Err("文件扩展出现不可能的卸载事务状态".into());
            }
        }
    } else if current.is_some() {
        return Err(format!(
            "已存在非本工具创建的扩展文件 {}，拒绝覆盖",
            path.display()
        ));
    }

    // 副作用前写 Pending + 精确路径 + 将写入内容指纹。
    reserve_record(
        store,
        id,
        new_file_record(id, OwnershipState::Pending, desired.clone())?,
    )?;
    let parent = path.parent().ok_or("扩展目标没有父目录")?;
    if id.starts_with("skill-") {
        let skills = skills_dir().ok_or("找不到 ~/.claude")?;
        ensure_real_directory(&skills, "Skills 目录")?;
        ensure_real_directory(parent, "Skill 目标目录")?;
    } else {
        ensure_private_directory(parent, "扩展状态目录")?;
    }
    if let Err(error) = create_extension_file_no_replace(&path, body) {
        match std::fs::symlink_metadata(&path) {
            Ok(_) => {
                // create_new 的竞争赢家（或创建后写入/fsync 失败留下的对象）
                // 没有 durable inode/file-id 收据。Pending 不能取得删除权限；
                // 立即放弃记录并原样保留该目录项。
                remove_record(store, id)?;
                return Err(format!(
                    "{error}；目标路径现已存在，已放弃 Pending ownership，现有对象不会被自动删除"
                ));
            }
            Err(metadata_error) if metadata_error.kind() == std::io::ErrorKind::NotFound => {
                return Err(error);
            }
            Err(metadata_error) => {
                return Err(format!(
                    "{error}；复查目标路径失败，Pending ownership 已保留: {metadata_error}"
                ));
            }
        }
    }
    match file_fingerprint(&path, "扩展目标文件") {
        Ok(actual) if actual.as_deref() == Some(desired.as_str()) => {}
        Ok(_) => {
            remove_record(store, id)?;
            return Err("扩展写入后指纹不匹配；已保留对象并放弃 Pending ownership".into());
        }
        Err(error) => {
            remove_record(store, id)?;
            return Err(format!(
                "扩展写入后对象类型/内容无法验证；已保留对象并放弃 Pending ownership: {error}"
            ));
        }
    }
    reserve_record(
        store,
        id,
        new_file_record(id, OwnershipState::Installed, desired)?,
    )?;
    Ok(ExtensionActionResult {
        ok: true,
        message: format!("已安装扩展 → {}", path.display()),
        written: vec![path.display().to_string()],
    })
}

/// 扩展首次落盘只允许原子新建，绝不 replace。
/// Pending 与创建之间如果用户/编辑器先建立了同名文件，
/// create_new 会 fail-closed，用户内容保持原样。
fn create_extension_file_no_replace(path: &Path, body: &str) -> Result<(), String> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let mut file = options.open(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::AlreadyExists {
            format!(
                "扩展目标 {} 在安装期间已被创建，已拒绝覆盖；Pending ownership 已保留供重试/安全卸载",
                path.display()
            )
        } else {
            format!("原子新建扩展文件 {} 失败: {error}", path.display())
        }
    })?;
    file.write_all(body.as_bytes())
        .map_err(|error| format!("写入扩展文件失败: {error}"))?;
    file.sync_all()
        .map_err(|error| format!("持久化扩展文件失败: {error}"))?;
    drop(file);
    sync_parent_dir(path).map_err(|error| format!("持久化扩展目录失败: {error}"))
}

fn install_feishu(store: &mut OwnershipStore) -> Result<ExtensionActionResult, String> {
    check_cancelled()?;
    // @clack/prompts 当前闭包要求 Node >=20.12；统一使用产品已固定
    // 校验的 Node 22。npm 仅保留给外部安装探测，不参与本工具的
    // 安装解析/下载/lifecycle 或卸载。
    super::runtime::ensure_node_sync(Some(22))?;
    let prefix = feishu_prefix()?;
    let existing = store.items.get("cli-feishu").cloned();

    if let Some(record) = &existing {
        match record.state {
            OwnershipState::Installed => {
                let receipt = record
                    .receipt
                    .as_ref()
                    .ok_or("飞书 CLI Installed ownership 缺少发布前收据")?;
                super::pinned_npm::verify_pinned_bundle_receipt(
                    &prefix,
                    FEISHU_PACKAGE,
                    FEISHU_COMMAND,
                    receipt,
                )?;
                let proof = validate_feishu_install(&prefix)?
                    .ok_or("飞书 CLI Installed ownership 对应的包已缺失，拒绝自动重装")?;
                let manifest = feishu_tree_manifest(&prefix)?;
                if record.fingerprint.as_deref() != Some(manifest.fingerprint.as_str()) {
                    return Err(
                        "飞书 CLI 文件树已变化；Installed ownership 已保留，拒绝覆盖或接管".into(),
                    );
                }
                // 必须先以 durable receipt + 整树指纹证明 native
                // 未被替换，之后才能执行 --version。
                validate_pinned_feishu_support_files(&proof)?;
                verify_feishu_native_version(&proof)?;
                ensure_feishu_path(&prefix)?;
                validate_feishu_launcher_on_path(&prefix)?;
                return Ok(ExtensionActionResult {
                    ok: true,
                    message: "飞书 CLI 已安装，整树指纹与 launcher 均验证通过".into(),
                    written: vec![
                        prefix.display().to_string(),
                        proof.package_bin_target.display().to_string(),
                        proof.global_launcher.display().to_string(),
                    ],
                });
            }
            OwnershipState::Uninstalling | OwnershipState::CleanupPending => {
                return Err("飞书 CLI 上次卸载未收尾；请先重试卸载，再重新安装".into());
            }
            OwnershipState::Legacy => {
                // v1 只有一个 ID，无法证明 prefix 或其内容由本工具创建；即使
                // package.json 名称吻合也绝不升级为可删除的 Installed 归属。
                if trusted_directory_exists(&prefix, "飞书 CLI 独占 prefix")?
                    && !directory_is_empty(&prefix)?
                {
                    remove_record(store, "cli-feishu")?;
                    return Err(
                        "旧版飞书记录无法证明现有 prefix 归属；已放弃旧归属并拒绝接管".into(),
                    );
                }
            }
            OwnershipState::Pending => {
                ensure_pending_marker(&prefix)?;
                if let Some(receipt) = &record.receipt {
                    // receipt 在首个 final rename 前已落盘；因而
                    // package-only / launcher-only / 完整发布都可精确恢复。
                    super::pinned_npm::remove_pinned_bundle_exact(
                        &prefix,
                        FEISHU_PACKAGE,
                        FEISHU_COMMAND,
                        receipt,
                    )?;
                }
                let _ = prune_known_empty_feishu_scaffold(&prefix)?;
                if !verify_pending_marker(&prefix)? {
                    return Err("飞书 CLI Pending marker 缺失或已变化，已拒绝重试".into());
                }
                if prefix_has_non_marker_entries(&prefix)? {
                    return Err(
                        "飞书 CLI Pending 精确恢复后仍有无收据内容；已保留并拒绝猜测删除".into(),
                    );
                }
            }
        }
    } else {
        require_unowned_prefix_empty(&prefix)?;
    }

    if let Some(path) = external_feishu_install()? {
        if existing
            .as_ref()
            .map(|record| record.state == OwnershipState::Legacy)
            .unwrap_or(false)
        {
            remove_record(store, "cli-feishu")?;
        }
        return Err(format!(
            "检测到用户已安装飞书 CLI（{path}）；为避免抢占卸载归属，已拒绝覆盖"
        ));
    }

    if existing
        .as_ref()
        .map(|record| record.state != OwnershipState::Pending)
        .unwrap_or(true)
    {
        // 先 durable 保留精确 marker 指纹，再创建 marker/启动固定
        // bundle 下载。若此间崩溃，下次只会在空 prefix 中恢复 marker。
        reserve_record(
            store,
            "cli-feishu",
            new_feishu_record(
                OwnershipState::Pending,
                Some(pending_marker_fingerprint()),
                None,
            )?,
        )?;
    }
    ensure_pending_marker(&prefix)?;
    // 启动固定 bundle 前再验证事务 marker。发布 API 使用原子
    // no-replace；Pending 中若已有未知/部分内容会 fail-closed，绝不覆盖。
    if !verify_pending_marker(&prefix)? {
        return Err("飞书 CLI Pending marker 缺失，已拒绝启动固定 bundle 下载".into());
    }
    let mut prepared = super::pinned_npm::prepare_feishu_bundle(&prefix).map_err(|error| {
        format!("飞书 CLI 固定官方 tarball/SRI bundle 准备失败；最终 prefix 尚未发布: {error}")
    })?;
    let staged = prepared.staged_install();
    let staging_prefix = feishu_bundle_prefix(&staged)?;
    let staging_proof = validate_feishu_install(&staging_prefix)?
        .ok_or("飞书 CLI staging 包/bin/launcher 验证失败")?;
    let staging_install_script = validate_pinned_feishu_support_files(&staging_proof)?;
    if staged.package_dir != staging_proof.package_dir
        || staged.bin_script != staging_proof.package_bin_target
        || staged.install_script.as_deref() != Some(staging_install_script.as_path())
        || !staged
            .launcher_paths
            .contains(&staging_proof.global_launcher)
        || staged.native_alias_dir.is_some()
    {
        return Err("飞书 CLI 固定 bundle staging 证明路径不一致，已拒绝执行安装脚本".into());
    }
    // lifecycle 只在私有 staging 内执行；最终 prefix 此时仍只有 marker。
    prepare_feishu_native_destination(&staging_proof)?;
    run_pinned_feishu_postinstall(&staging_proof)?;
    durably_republish_feishu_native(&staging_proof)?;
    verify_feishu_native_version(&staging_proof)?;
    prepared.refresh_receipt_after_staging_changes()?;
    let receipt = prepared.receipt().clone();
    if !store.items.contains_key("cli-feishu") {
        return Err("飞书 CLI Pending ownership 意外丢失".into());
    }
    reserve_record(
        store,
        "cli-feishu",
        new_feishu_record(
            OwnershipState::Pending,
            Some(pending_marker_fingerprint()),
            Some(receipt.clone()),
        )?,
    )?;
    // 硬边界：receipt 已 durable 落盘后，才允许首个 final rename。
    let bundle = prepared.publish().map_err(|error| {
        format!("飞书 CLI 固定 bundle 发布失败；Pending receipt 已保留可精确恢复: {error}")
    })?;
    super::pinned_npm::verify_pinned_bundle_receipt(
        &prefix,
        FEISHU_PACKAGE,
        FEISHU_COMMAND,
        &receipt,
    )?;
    let proof = validate_feishu_install(&prefix)?
        .ok_or("固定 bundle 发布成功但飞书 CLI 包/bin/launcher 验证失败；Pending receipt 已保留")?;
    let final_install_script = validate_pinned_feishu_support_files(&proof)?;
    if bundle.package_dir != proof.package_dir
        || bundle.bin_script != proof.package_bin_target
        || bundle.install_script.as_deref() != Some(final_install_script.as_path())
        || !bundle.launcher_paths.contains(&proof.global_launcher)
        || bundle.native_alias_dir.is_some()
    {
        return Err(
            "飞书 CLI 最终 bundle 路径与 staging 收据不一致；Pending ownership 已保留".into(),
        );
    }
    // final prefix 绝不再执行 install.js；只执行已通过 receipt 的 native 版本检查。
    verify_feishu_native_version(&proof)?;
    if !verify_pending_marker(&prefix)? {
        return Err("固定 bundle 完成后 reservation marker 缺失；Pending ownership 已保留".into());
    }
    let manifest = feishu_tree_manifest(&prefix)?;
    reserve_record(
        store,
        "cli-feishu",
        new_feishu_record(
            OwnershipState::Installed,
            Some(manifest.fingerprint),
            Some(receipt),
        )?,
    )?;
    ensure_feishu_path(&prefix)?;
    validate_feishu_launcher_on_path(&prefix)?;
    let bin = feishu_bin_dir(&prefix);
    Ok(ExtensionActionResult {
        ok: true,
        message: format!(
            "已从官方固定 tarball 安装飞书 CLI 到 CodeCLI 独占目录，并完成 SHA-512 SRI/依赖闭包/原生校验。请新开终端运行 lark-cli 登录。\nPATH: export PATH={}:$PATH",
            shell_single_quote(&bin.display().to_string())
        ),
        written: vec![
            prefix.display().to_string(),
            proof.package_bin_target.display().to_string(),
            proof.native_binary.display().to_string(),
            proof.global_launcher.display().to_string(),
        ],
    })
}

pub fn list_extensions_sync() -> Result<ExtensionListResult, String> {
    let store = load_owned()?;
    let mut items = catalog();
    let prefix = feishu_prefix()?;
    let tool_feishu = validate_package_chain(&prefix)?;
    let external_feishu = if tool_feishu {
        None
    } else {
        // 列表页不起 npm 子进程；真正安装前会再用带超时的
        // `npm root -g` 严格检查未进 PATH 的用户自装包。
        external_feishu_on_path()?
    };

    for item in &mut items {
        let record = store.items.get(&item.id);
        item.owned_by_tool = record.is_some();
        item.can_uninstall = record.is_some();
        match item.id.as_str() {
            "skill-explain" | "skill-bugfix" | "skill-pr" | "mcp-filesystem-note" => {
                let path = expected_target(&item.id)?;
                item.installed = file_fingerprint(&path, "扩展目标文件")?.is_some();
                item.detail = Some(path.display().to_string());
            }
            "cli-feishu" => {
                item.installed = tool_feishu || external_feishu.is_some();
                item.detail = if tool_feishu {
                    Some(prefix.display().to_string())
                } else {
                    external_feishu.clone()
                };
            }
            _ => {}
        }
        if record.map(|value| value.state) == Some(OwnershipState::Pending) {
            item.detail = Some(format!(
                "{}（上次安装未完成，可重试或安全卸载）",
                item.detail.clone().unwrap_or_default()
            ));
        }
        if record.map(|value| value.state) == Some(OwnershipState::Uninstalling) {
            item.detail = Some(format!(
                "{}（卸载中断；仅在文件树未变化时可安全重试）",
                item.detail.clone().unwrap_or_default()
            ));
        }
        if record.map(|value| value.state) == Some(OwnershipState::CleanupPending) {
            item.detail = Some(format!(
                "{}（旧版卸载已清除包内容，仅待清理 marker、空目录与 PATH）",
                item.detail.clone().unwrap_or_default()
            ));
        }
        if record.map(|value| value.state) == Some(OwnershipState::Legacy) {
            item.detail = Some(format!(
                "{}（旧版归属待验证）",
                item.detail.clone().unwrap_or_default()
            ));
        }
    }
    Ok(ExtensionListResult {
        ok: true,
        items,
        message: "仅白名单扩展；pending/installed 均记录精确路径，用户自装内容永不删除".into(),
    })
}

pub fn install_extension_sync(id: String) -> Result<ExtensionActionResult, String> {
    let id = id.trim();
    if !known_extension_id(id) {
        return Err(format!("不在白名单: {id}"));
    }
    let mut store = load_owned()?;
    match id {
        "skill-explain" | "skill-bugfix" | "skill-pr" => {
            let (_, body) = skill_body(id).ok_or("未知 Skill")?;
            file_install(id, body, &mut store)
        }
        "mcp-filesystem-note" => {
            let body = mcp_note_body()?;
            file_install(id, &body, &mut store)
        }
        "cli-feishu" => install_feishu(&mut store),
        _ => Err(format!("不在白名单: {id}")),
    }
}

fn quarantine_directory_for(path: &Path, id: &str) -> Result<PathBuf, String> {
    if !known_extension_id(id)
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err("扩展隔离目录 ID 非法".into());
    }
    let parent = path.parent().ok_or("扩展文件没有父目录")?;
    let candidate = parent.join(format!(".codecli-delete-{id}"));
    std::fs::create_dir(&candidate).map_err(|error| {
        if error.kind() == std::io::ErrorKind::AlreadyExists {
            "扩展隔离目录已存在，必须先完成中断恢复".to_string()
        } else {
            format!("创建扩展隔离目录失败: {error}")
        }
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(error) =
            std::fs::set_permissions(&candidate, std::fs::Permissions::from_mode(0o700))
        {
            let _ = std::fs::remove_dir(&candidate);
            return Err(format!("收紧扩展隔离目录权限失败: {error}"));
        }
    }
    sync_parent_dir(&candidate)
        .map_err(|error| format!("fsync 扩展隔离目录父目录失败: {error}"))?;
    Ok(candidate)
}

fn remove_empty_quarantine_directory(directory: &Path) -> Result<(), String> {
    match std::fs::remove_dir(directory) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("清理扩展隔离目录失败: {error}")),
    }
    sync_parent_dir(directory).map_err(|error| format!("fsync 扩展隔离目录父目录失败: {error}"))
}

/// 隔离目录名与扩展 ID 绑定，使崩溃后的下一次 install/uninstall
/// 能找到 payload。只有“原路径缺失 + 隔离中唯一 payload 目录项”才
/// no-follow 原子恢复；payload 可能是竞态换入的 symlink/目录，恢复时
/// 作为不透明对象移动而不遍历。两份同时存在或有额外条目时 fail-closed。
fn recover_file_quarantine(path: &Path, id: &str) -> Result<(), String> {
    let parent = path.parent().ok_or("扩展文件没有父目录")?;
    let directory = parent.join(format!(".codecli-delete-{id}"));
    let metadata = match std::fs::symlink_metadata(&directory) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("检查扩展隔离目录失败: {error}")),
    };
    if metadata.file_type().is_symlink() || metadata_is_reparse(&metadata) || !metadata.is_dir() {
        return Err("扩展隔离目录不是可信真实目录，已拒绝恢复".into());
    }
    let mut entries =
        std::fs::read_dir(&directory).map_err(|error| format!("读取扩展隔离目录失败: {error}"))?;
    let first = match entries.next() {
        None => {
            remove_empty_quarantine_directory(&directory)?;
            return Ok(());
        }
        Some(entry) => entry.map_err(|error| format!("读取扩展隔离条目失败: {error}"))?,
    };
    if first.file_name() != "payload" || entries.next().is_some() {
        return Err("扩展隔离目录含未知条目，已保留并拒绝恢复".into());
    }
    let payload = first.path();
    // preflight 普通文件校验与 rename 之间，外部进程仍可能把原路径
    // 换成 symlink/目录。硬崩溃后这里必须把单一 payload 当作“不透明
    // 目录项”按对象原子恢复；只做 symlink_metadata，不读取或遍历它，
    // 更不能因其不是普通文件而把用户对象永久搬离原路径。
    std::fs::symlink_metadata(&payload)
        .map_err(|error| format!("检查扩展隔离 payload 失败: {error}"))?;
    match std::fs::symlink_metadata(path) {
        Ok(_) => {
            return Err(format!(
                "扩展原路径与隔离 payload 同时存在；两者均已保留，请人工核对: {}",
                path.display()
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("检查扩展原路径失败: {error}")),
    }
    atomic_restore_no_replace(&payload, path)?;
    sync_parent_dir(&payload).map_err(|error| format!("fsync 扩展隔离目录失败: {error}"))?;
    sync_parent_dir(path).map_err(|error| format!("fsync 扩展原目录失败: {error}"))?;
    remove_empty_quarantine_directory(&directory)
}

#[cfg(target_os = "macos")]
fn atomic_restore_no_replace(source: &Path, destination: &Path) -> Result<(), String> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let source = CString::new(source.as_os_str().as_bytes()).map_err(|_| "扩展隔离源路径含 NUL")?;
    let destination =
        CString::new(destination.as_os_str().as_bytes()).map_err(|_| "扩展恢复目标路径含 NUL")?;
    let result =
        unsafe { libc::renamex_np(source.as_ptr(), destination.as_ptr(), libc::RENAME_EXCL) };
    if result == 0 {
        Ok(())
    } else {
        Err(format!(
            "原子恢复扩展文件失败: {}",
            std::io::Error::last_os_error()
        ))
    }
}

#[cfg(target_os = "linux")]
fn atomic_restore_no_replace(source: &Path, destination: &Path) -> Result<(), String> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let source = CString::new(source.as_os_str().as_bytes()).map_err(|_| "扩展隔离源路径含 NUL")?;
    let destination =
        CString::new(destination.as_os_str().as_bytes()).map_err(|_| "扩展恢复目标路径含 NUL")?;
    let result = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            source.as_ptr(),
            libc::AT_FDCWD,
            destination.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(format!(
            "原子恢复扩展文件失败: {}",
            std::io::Error::last_os_error()
        ))
    }
}

#[cfg(windows)]
fn atomic_restore_no_replace(source: &Path, destination: &Path) -> Result<(), String> {
    // MoveFile/rename 在 Windows 默认不覆盖既有目标，保持 no-replace 语义。
    std::fs::rename(source, destination).map_err(|error| format!("原子恢复扩展文件失败: {error}"))
}

#[cfg(all(unix, not(any(target_os = "macos", target_os = "linux"))))]
fn atomic_restore_no_replace(source: &Path, destination: &Path) -> Result<(), String> {
    // 其它 Unix 目标不是当前发行平台；用 hard_link 获得原子 no-replace，
    // 该回退只适用于已验证的普通文件。
    std::fs::hard_link(source, destination)
        .map_err(|error| format!("原子恢复扩展文件失败: {error}"))?;
    std::fs::remove_file(source).map_err(|error| format!("清理扩展隔离文件失败: {error}"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QuarantineDeleteResult {
    Missing,
    Deleted,
    FingerprintMismatchRestored,
}

fn quarantine_then_delete_if_matching(
    id: &str,
    path: &Path,
    expected_fingerprint: &str,
) -> Result<QuarantineDeleteResult, String> {
    recover_file_quarantine(path, id)?;

    // 先在原路径上以 no-follow 语义确认对象仍是可信小文件，
    // 再创建隔离事务。否则用户将文件替换为符号链接或目录时，
    // 先 rename 再校验会在崩溃窗口中把用户对象留在隔离目录。
    // 隔离后仍会再算指纹，用来抵御预检与 rename 之间的普通文件竞态。
    let preflight = match file_fingerprint(path, "待隔离的扩展文件")? {
        Some(value) => value,
        None => return Ok(QuarantineDeleteResult::Missing),
    };
    if preflight != expected_fingerprint {
        return Ok(QuarantineDeleteResult::FingerprintMismatchRestored);
    }

    let quarantine_dir = quarantine_directory_for(path, id)?;
    let quarantine_path = quarantine_dir.join("payload");
    match std::fs::rename(path, &quarantine_path) {
        Ok(()) => {
            sync_parent_dir(path).map_err(|error| format!("fsync 扩展原目录失败: {error}"))?;
            sync_parent_dir(&quarantine_path)
                .map_err(|error| format!("fsync 扩展隔离目录失败: {error}"))?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            remove_empty_quarantine_directory(&quarantine_dir)?;
            return Ok(QuarantineDeleteResult::Missing);
        }
        Err(error) => {
            let _ = remove_empty_quarantine_directory(&quarantine_dir);
            return Err(format!("原子隔离扩展文件 {} 失败: {error}", path.display()));
        }
    }

    let actual = match file_fingerprint(&quarantine_path, "隔离后的扩展文件") {
        Ok(value) => value,
        Err(error) => {
            let restore = atomic_restore_no_replace(&quarantine_path, path);
            if restore.is_ok() {
                let _ = sync_parent_dir(&quarantine_path);
                let _ = sync_parent_dir(path);
                let _ = remove_empty_quarantine_directory(&quarantine_dir);
            }
            return match restore {
                Ok(()) => Err(format!("隔离扩展校验失败，已原子恢复: {error}")),
                Err(restore_error) => Err(format!(
                    "隔离扩展校验失败且无法恢复；文件仍在 {}: {error}; {restore_error}",
                    quarantine_path.display()
                )),
            };
        }
    };
    if actual.as_deref() != Some(expected_fingerprint) {
        if let Err(error) = atomic_restore_no_replace(&quarantine_path, path) {
            return Err(format!(
                "隔离后指纹不符且无法原子恢复；原对象保留在 {}: {error}",
                quarantine_path.display()
            ));
        }
        sync_parent_dir(&quarantine_path)
            .map_err(|error| format!("fsync 扩展隔离目录失败: {error}"))?;
        sync_parent_dir(path).map_err(|error| format!("fsync 扩展原目录失败: {error}"))?;
        remove_empty_quarantine_directory(&quarantine_dir)?;
        return Ok(QuarantineDeleteResult::FingerprintMismatchRestored);
    }

    remove_file_durable(&quarantine_path)
        .map_err(|error| format!("删除隔离后的本工具扩展文件失败: {error}"))?;
    remove_empty_quarantine_directory(&quarantine_dir)?;
    Ok(QuarantineDeleteResult::Deleted)
}

fn quarantine_file_for_record(
    id: &str,
    path: &Path,
    record: &OwnershipRecord,
) -> Result<QuarantineDeleteResult, String> {
    let expected = match record.state {
        // Legacy/Pending 都没有“本工具成功提交了这个 inode/file-id”的
        // durable 证明。即使字节恰好等于内置内容，也可能是外部进程
        // 创建的同内容文件，因此只放弃记录、绝不隔离或删除。
        OwnershipState::Legacy | OwnershipState::Pending => None,
        OwnershipState::Installed => record.fingerprint.clone(),
        OwnershipState::Uninstalling | OwnershipState::CleanupPending => {
            return Err("文件扩展出现不可能的卸载事务状态".into());
        }
    };
    if let Some(expected) = expected {
        quarantine_then_delete_if_matching(id, path, &expected)
    } else {
        Ok(QuarantineDeleteResult::FingerprintMismatchRestored)
    }
}

fn uninstall_file(id: &str, store: &mut OwnershipStore) -> Result<ExtensionActionResult, String> {
    let record = store
        .items
        .get(id)
        .cloned()
        .ok_or("该扩展非本工具安装记录，拒绝删除（防误删用户自建内容）")?;
    let path = expected_target(id)?;
    recover_file_quarantine(&path, id)?;
    let result = quarantine_file_for_record(id, &path, &record)?;
    // ownership 只记录文件，没有记录父目录是否由本工具创建。
    // 因此即使 Skill 目录已空也必须保留，避免删除用户
    // 原先就存在的空目录，或 Pending 期间用户创建的目录。
    remove_record(store, id)?;
    Ok(ExtensionActionResult {
        ok: true,
        message: if result == QuarantineDeleteResult::FingerprintMismatchRestored {
            format!(
                "{} 内容与 ownership 指纹不一致，已保留文件并放弃自动删除归属",
                path.display()
            )
        } else {
            format!("已卸载本工具扩展：{}", path.display())
        },
        written: vec![path.display().to_string()],
    })
}

fn finish_feishu_uninstall(
    store: &mut OwnershipStore,
    prefix: &Path,
    message: &str,
) -> Result<ExtensionActionResult, String> {
    let mut preserved_unknown = false;
    if trusted_directory_exists(prefix, "飞书 CLI 独占 prefix")? {
        // receipt 只授权 package + 固定 launcher；用户在其他路径
        // 新增的 sibling（包括空目录）永不删。只剪明确列出的 npm
        // 空骨架和精确 marker，绝不递归扫描 prefix。
        let _ = prune_known_empty_feishu_scaffold(prefix)?;
        let _ = remove_pending_marker(prefix)?;
        let _ = prune_known_empty_feishu_scaffold(prefix)?;
        if trusted_directory_exists(prefix, "飞书 CLI 独占 prefix")? {
            preserved_unknown = !directory_is_empty(prefix)?;
        }
    }
    let mut written = remove_feishu_path(prefix)?;
    written.push(prefix.display().to_string());
    remove_record(store, "cli-feishu")?;
    Ok(ExtensionActionResult {
        ok: true,
        message: if preserved_unknown {
            format!("{message}；prefix 中的非本工具 sibling 内容已原样保留")
        } else {
            message.into()
        },
        written,
    })
}

fn uninstall_feishu(store: &mut OwnershipStore) -> Result<ExtensionActionResult, String> {
    let mut record = store
        .items
        .get("cli-feishu")
        .cloned()
        .ok_or("飞书 CLI 非本工具安装记录，拒绝删除用户自装包")?;
    if record.state == OwnershipState::Legacy {
        remove_record(store, "cli-feishu")?;
        return Ok(ExtensionActionResult {
            ok: true,
            message: "旧版记录没有 prefix 证明；已保留现有飞书 CLI 并放弃自动删除归属".into(),
            written: Vec::new(),
        });
    }

    let prefix = feishu_prefix()?;
    let prefix_exists = trusted_directory_exists(&prefix, "飞书 CLI 独占 prefix")?;
    if record.state == OwnershipState::CleanupPending
        || (record.state == OwnershipState::Pending && record.receipt.is_none())
    {
        // 无 receipt 事务永远只有 marker/空骨架权限。
        if !prefix_exists {
            return finish_feishu_uninstall(
                store,
                &prefix,
                "飞书 CLI 中断事务未产生副作用，已完成 PATH/ownership 收尾",
            );
        }
        if prune_known_empty_feishu_scaffold(&prefix)? {
            return finish_feishu_uninstall(
                store,
                &prefix,
                "飞书 CLI 中断事务只剩空目录，已完成 PATH/ownership 收尾",
            );
        }
        require_marker_only_pending_tree(&prefix)?;
        return finish_feishu_uninstall(
            store,
            &prefix,
            "已清理飞书 CLI 中断事务的精确 marker、空骨架与 PATH",
        );
    }

    if record.state == OwnershipState::Installed {
        // 在任何精确隔离前 durable 记录卸载意图，并保留
        // 首次 publish 前收据；整树指纹仅作审计，不授权删 sibling。
        reserve_record(
            store,
            "cli-feishu",
            new_feishu_record(
                OwnershipState::Uninstalling,
                record.fingerprint.clone(),
                record.receipt.clone(),
            )?,
        )?;
        record = store
            .items
            .get("cli-feishu")
            .cloned()
            .ok_or("Uninstalling ownership 意外丢失")?;
    }

    if !matches!(
        record.state,
        OwnershipState::Pending | OwnershipState::Uninstalling
    ) {
        return Err("飞书 CLI 卸载状态机未进入安全事务状态".into());
    }
    let receipt = record
        .receipt
        .as_ref()
        .ok_or("飞书 CLI 精确卸载缺少发布前收据")?;
    check_cancelled()?;
    super::pinned_npm::remove_pinned_bundle_exact(
        &prefix,
        FEISHU_PACKAGE,
        FEISHU_COMMAND,
        receipt,
    )?;
    finish_feishu_uninstall(
        store,
        &prefix,
        "已精确卸载 receipt 绑定的飞书 CLI package/launcher，并完成 PATH/ownership 收尾",
    )
}

fn uninstall_owned_extension(
    id: &str,
    store: &mut OwnershipStore,
) -> Result<ExtensionActionResult, String> {
    if !store.items.contains_key(id) {
        return Err("该扩展非本工具安装记录，拒绝删除（防误删用户自建内容）".into());
    }
    match id {
        "skill-explain" | "skill-bugfix" | "skill-pr" | "mcp-filesystem-note" => {
            uninstall_file(id, store)
        }
        "cli-feishu" => uninstall_feishu(store),
        _ => Err(format!("不在白名单: {id}")),
    }
}

pub fn uninstall_extension_sync(id: String) -> Result<ExtensionActionResult, String> {
    // 安全退出能力不能依赖外部服务状态；但仍严格只删 durable ownership。
    let id = id.trim();
    if !known_extension_id(id) {
        return Err(format!("不在白名单: {id}"));
    }
    let mut store = load_owned()?;
    uninstall_owned_extension(id, &mut store)
}

/// 完整卸载在删除状态目录前调用。每成功清一项就立即 durable 更新 ownership；
/// 任一项失败即 fail-closed，状态目录和剩余归属记录都会保留供重试。
pub(crate) fn uninstall_owned_extensions_for_purge() -> Result<Vec<String>, String> {
    let mut store = load_owned()?;
    let ids: Vec<String> = store.items.keys().cloned().collect();
    let mut written = Vec::new();
    for id in ids {
        let result = uninstall_owned_extension(&id, &mut store)?;
        written.extend(result.written);
    }
    Ok(written)
}

#[tauri::command]
pub async fn list_extensions() -> Result<ExtensionListResult, String> {
    super::util::spawn_blocking_result(list_extensions_sync).await
}

#[tauri::command]
pub async fn install_extension(id: String) -> Result<ExtensionActionResult, String> {
    super::util::spawn_blocking_result(move || with_op_lock(|| install_extension_sync(id))).await
}

#[tauri::command]
pub async fn uninstall_extension(id: String) -> Result<ExtensionActionResult, String> {
    super::util::spawn_blocking_result(move || with_op_lock(|| uninstall_extension_sync(id))).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    fn test_directory(label: &str) -> PathBuf {
        let sequence = TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        // macOS 的 /var 是 /private/var 符号链接；生产路径校验会正确拒绝
        // 未解析的链接目录链，因此测试根也必须先 canonicalize。
        let temp = std::fs::canonicalize(std::env::temp_dir()).expect("canonical temp directory");
        let path = temp.join(format!(
            "codecli-extensions-{label}-{}-{sequence}",
            std::process::id()
        ));
        std::fs::create_dir(&path).expect("create test directory");
        path
    }

    fn cleanup_test_directory(path: &Path) {
        // 仅清理由本测试使用 create_dir 创建、同时带 PID 与单调序号的临时目录。
        let _ = std::fs::remove_dir_all(path);
    }

    fn test_feishu_receipt() -> super::super::pinned_npm::PinnedBundleReceipt {
        let prefix = feishu_prefix().unwrap();
        let mut launcher_sha256 = BTreeMap::new();
        for launcher in super::super::runtime::cli_launcher_paths(&prefix, FEISHU_COMMAND).unwrap()
        {
            let key = launcher
                .strip_prefix(&prefix)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            launcher_sha256.insert(key, "1".repeat(64));
        }
        super::super::pinned_npm::PinnedBundleReceipt {
            schema_version: 1,
            package_sha256: "0".repeat(64),
            launcher_sha256,
        }
    }

    #[test]
    fn ownership_parser_rejects_unknown_ids() {
        let error = parse_owned_body(r#"["skill-explain","unknown-extension"]"#)
            .expect_err("unknown ownership must fail closed");
        assert!(error.contains("未知条目"));
    }

    #[test]
    fn legacy_ownership_is_migrated_without_delete_authority() {
        let parsed = parse_owned_body(
            r#"["skill-explain","skill-bugfix","skill-pr","mcp-filesystem-note","cli-feishu"]"#,
        )
        .expect("catalog ownership");
        assert_eq!(parsed.items.len(), 5);
        assert!(parsed
            .items
            .values()
            .all(|record| record.state == OwnershipState::Legacy && record.target.is_none()));
    }

    #[test]
    fn old_v2_feishu_record_without_tree_fingerprint_loses_delete_authority() {
        let target = feishu_prefix().unwrap().display().to_string();
        let raw = serde_json::json!({
            "version": 2,
            "items": {
                "cli-feishu": {
                    "state": "installed",
                    "kind": "cli",
                    "target": target,
                    "package": FEISHU_PACKAGE
                }
            }
        })
        .to_string();
        let parsed = parse_owned_body(&raw).expect("safe legacy downgrade");
        let record = parsed.items.get("cli-feishu").unwrap();
        assert_eq!(record.state, OwnershipState::Legacy);
        assert!(record.target.is_none());
        assert!(record.fingerprint.is_none());
        assert!(record.package.is_none());
    }

    #[test]
    fn old_v2_feishu_installed_with_tree_fingerprint_still_loses_delete_authority() {
        let target = feishu_prefix().unwrap().display().to_string();
        let raw = serde_json::json!({
            "version": 2,
            "items": {
                "cli-feishu": {
                    "state": "installed",
                    "kind": "cli",
                    "target": target,
                    "fingerprint": "0".repeat(64),
                    "package": FEISHU_PACKAGE
                }
            }
        })
        .to_string();
        let parsed = parse_owned_body(&raw).expect("v2 tree hash is not a receipt");
        assert_eq!(
            parsed.items.get("cli-feishu").unwrap().state,
            OwnershipState::Legacy
        );
    }

    #[test]
    fn old_v2_feishu_pending_without_marker_loses_delete_authority() {
        let target = feishu_prefix().unwrap().display().to_string();
        let raw = serde_json::json!({
            "version": 2,
            "items": {
                "cli-feishu": {
                    "state": "pending",
                    "kind": "cli",
                    "target": target,
                    "package": FEISHU_PACKAGE
                }
            }
        })
        .to_string();
        let parsed = parse_owned_body(&raw).expect("safe pending legacy downgrade");
        assert_eq!(
            parsed.items.get("cli-feishu").unwrap().state,
            OwnershipState::Legacy
        );
    }

    #[test]
    fn old_v2_pending_cannot_inject_a_v3_receipt() {
        let target = feishu_prefix().unwrap().display().to_string();
        let raw = serde_json::json!({
            "version": 2,
            "items": {
                "cli-feishu": {
                    "state": "pending",
                    "kind": "cli",
                    "target": target,
                    "fingerprint": pending_marker_fingerprint(),
                    "package": FEISHU_PACKAGE,
                    "receipt": test_feishu_receipt()
                }
            }
        })
        .to_string();
        let parsed = parse_owned_body(&raw).expect("v2 cannot grant receipt delete authority");
        assert_eq!(
            parsed.items.get("cli-feishu").unwrap().state,
            OwnershipState::Legacy
        );
    }

    #[test]
    fn current_installed_record_without_receipt_is_rejected() {
        let target = feishu_prefix().unwrap().display().to_string();
        let raw = serde_json::json!({
            "version": OWNERSHIP_VERSION,
            "items": {
                "cli-feishu": {
                    "state": "installed",
                    "kind": "cli",
                    "target": target,
                    "fingerprint": "0".repeat(64),
                    "package": FEISHU_PACKAGE
                }
            }
        })
        .to_string();
        assert!(parse_owned_body(&raw).is_err());
    }

    #[test]
    fn pending_receipt_survives_partial_publish_crash_boundary() {
        let receipt = test_feishu_receipt();
        let mut store = OwnershipStore::default();
        store.items.insert(
            "cli-feishu".into(),
            new_feishu_record(
                OwnershipState::Pending,
                Some(pending_marker_fingerprint()),
                Some(receipt.clone()),
            )
            .unwrap(),
        );

        let raw = serde_json::to_string(&store).unwrap();
        let parsed = parse_owned_body(&raw).expect("durable pending receipt must be replayable");
        let replay = parsed.items.get("cli-feishu").unwrap();
        assert_eq!(replay.state, OwnershipState::Pending);
        assert_eq!(replay.receipt.as_ref(), Some(&receipt));

        let uninstalling = new_feishu_record(
            OwnershipState::Uninstalling,
            Some("2".repeat(64)),
            replay.receipt.clone(),
        )
        .unwrap();
        assert_eq!(uninstalling.receipt.as_ref(), Some(&receipt));
    }

    #[test]
    fn tampered_target_or_fingerprint_is_rejected() {
        let mut store = OwnershipStore::default();
        let mut record =
            new_file_record("skill-explain", OwnershipState::Pending, "0".repeat(64)).unwrap();
        record.target = Some("/tmp/not-owned".into());
        store.items.insert("skill-explain".into(), record);
        assert!(validate_store(&store)
            .unwrap_err()
            .contains("目标路径被篡改"));

        let mut store = OwnershipStore::default();
        store.items.insert(
            "skill-explain".into(),
            new_file_record("skill-explain", OwnershipState::Pending, "NOT-A-SHA".into()).unwrap(),
        );
        assert!(validate_store(&store).unwrap_err().contains("内容指纹异常"));
    }

    #[test]
    fn pending_record_authorizes_only_exact_file_fingerprint() {
        let expected = sha256_hex(b"tool content");
        let record =
            new_file_record("skill-explain", OwnershipState::Pending, expected.clone()).unwrap();
        assert_eq!(record.fingerprint.as_deref(), Some(expected.as_str()));
        assert_ne!(
            record.fingerprint.as_deref(),
            Some(sha256_hex(b"user content").as_str())
        );
    }

    #[test]
    fn feishu_record_is_bound_to_fixed_tool_prefix_and_package() {
        let marker_fingerprint = pending_marker_fingerprint();
        let record = new_feishu_record(
            OwnershipState::Pending,
            Some(marker_fingerprint.clone()),
            None,
        )
        .unwrap();
        assert_eq!(record.package.as_deref(), Some(FEISHU_PACKAGE));
        assert_eq!(
            record.target.as_deref(),
            Some(feishu_prefix().unwrap().display().to_string().as_str())
        );
        assert_eq!(
            record.fingerprint.as_deref(),
            Some(marker_fingerprint.as_str())
        );

        let receipt = test_feishu_receipt();
        assert!(new_feishu_record(OwnershipState::Pending, None, None).is_err());
        assert!(new_feishu_record(OwnershipState::Pending, Some("0".repeat(64)), None).is_err());
        assert!(new_feishu_record(OwnershipState::Installed, None, Some(receipt.clone())).is_err());
        assert!(new_feishu_record(OwnershipState::Installed, Some("0".repeat(64)), None).is_err());
        assert!(new_feishu_record(
            OwnershipState::Installed,
            Some("0".repeat(64)),
            Some(receipt.clone())
        )
        .is_ok());
        assert!(new_feishu_record(
            OwnershipState::Uninstalling,
            Some("0".repeat(64)),
            Some(receipt.clone())
        )
        .is_ok());
        assert!(new_feishu_record(
            OwnershipState::CleanupPending,
            Some(marker_fingerprint.clone()),
            None
        )
        .is_ok());
        assert!(
            new_feishu_record(OwnershipState::CleanupPending, Some("0".repeat(64)), None).is_err()
        );
        assert!(new_feishu_record(
            OwnershipState::CleanupPending,
            Some(marker_fingerprint),
            Some(receipt)
        )
        .is_err());
    }

    #[test]
    fn pending_marker_is_created_only_in_empty_prefix() {
        let empty = test_directory("pending-marker-empty");
        ensure_pending_marker(&empty).expect("create marker in empty reserved prefix");
        assert!(verify_pending_marker(&empty).unwrap());
        assert!(!prefix_has_non_marker_entries(&empty).unwrap());
        cleanup_test_directory(&empty);

        let nonempty = test_directory("pending-marker-nonempty");
        std::fs::write(nonempty.join("user.txt"), b"user").unwrap();
        let error = ensure_pending_marker(&nonempty).unwrap_err();
        assert!(error.contains("非空且缺少"));
        assert_eq!(std::fs::read(nonempty.join("user.txt")).unwrap(), b"user");
        cleanup_test_directory(&nonempty);
    }

    #[test]
    fn unowned_nonempty_prefix_is_rejected_even_without_package() {
        let prefix = test_directory("unowned-prefix");
        std::fs::write(prefix.join("user-file"), b"not ours").unwrap();
        let error = require_unowned_prefix_empty(&prefix).unwrap_err();
        assert!(error.contains("无归属内容"));
        assert_eq!(
            std::fs::read(prefix.join("user-file")).unwrap(),
            b"not ours"
        );
        cleanup_test_directory(&prefix);
    }

    #[test]
    fn tree_manifest_detects_drift() {
        let prefix = test_directory("tree-drift");
        std::fs::write(prefix.join("a.txt"), b"before").unwrap();
        let before = feishu_tree_manifest(&prefix).unwrap();
        std::fs::write(prefix.join("a.txt"), b"after").unwrap();
        let after = feishu_tree_manifest(&prefix).unwrap();
        assert_ne!(before.fingerprint, after.fingerprint);
        assert_eq!(before.entries, 1);
        assert_eq!(after.total_file_bytes, 5);
        cleanup_test_directory(&prefix);
    }

    #[test]
    fn pending_cleanup_rejects_non_marker_payload_and_preserves_it() {
        let prefix = test_directory("pending-cleanup-payload");
        atomic_write_mode(
            &pending_marker_path(&prefix),
            FEISHU_PENDING_MARKER_BODY,
            true,
        )
        .unwrap();
        let user_file = prefix.join("user-file.txt");
        std::fs::write(&user_file, b"keep me").unwrap();

        let error = require_marker_only_pending_tree(&prefix).unwrap_err();
        assert!(error.contains("含非 marker 内容"));
        assert_eq!(std::fs::read(&user_file).unwrap(), b"keep me");
        assert!(verify_pending_marker(&prefix).unwrap());
        cleanup_test_directory(&prefix);
    }

    #[test]
    fn pinned_feishu_metadata_and_hash_constants_are_strict() {
        // 这里故意断言精确审计值，而不只检查 64 位 hex 形状；否则升级
        // npm tarball 时遗留旧 checksums.txt 指纹会直到真实安装才失败。
        assert_eq!(FEISHU_PACKAGE_VERSION, "1.0.70");
        assert_eq!(
            FEISHU_INSTALL_SCRIPT_SHA256,
            "c057a117af60f1bf908507ee799dd2d17acc582f315153e996de1bfedd7618de"
        );
        assert_eq!(
            FEISHU_RUN_SCRIPT_SHA256,
            "b6b575a31d62ea45f55155f1090a49d31e79a1b0e5c70af15f9431ab850ca577"
        );
        assert_eq!(
            FEISHU_CHECKSUMS_SHA256,
            "106ac4329692a2d339145d4e08d905f50310733c02ef2783f29dfdc690c13ea7"
        );
        let prefix = test_directory("pinned-version-reject");
        let proof = FeishuInstallProof {
            package_dir: prefix.clone(),
            package_version: "999.0.0".into(),
            package_bin_target: prefix.join("scripts/run.js"),
            native_binary: prefix.join("bin/lark-cli"),
            global_launcher: prefix.join("global/lark-cli"),
            postinstall: Some("node scripts/install.js".into()),
        };
        assert!(validate_pinned_feishu_support_files(&proof)
            .unwrap_err()
            .contains("不在本安装器白名单"));
        cleanup_test_directory(&prefix);
    }

    #[test]
    #[ignore = "explicit live registry smoke test; ordinary tests remain fully offline"]
    fn live_pinned_feishu_support_files_match_execution_allowlist() {
        let prefix = test_directory("live-pinned-support-files");
        let prepared = super::super::pinned_npm::prepare_feishu_bundle(&prefix)
            .expect("download and verify pinned Feishu bundle");
        prepared
            .publish()
            .expect("publish pinned Feishu bundle for support-file validation");
        let proof = validate_feishu_install(&prefix)
            .expect("validate published Feishu package")
            .expect("published Feishu package must exist");
        validate_pinned_feishu_support_files(&proof)
            .expect("actual pinned support files must match the execution allowlist");
        cleanup_test_directory(&prefix);
    }

    #[test]
    fn pending_native_is_unlinked_before_pinned_installer_runs() {
        let prefix = test_directory("pending-native-replace");
        let native = prefix.join("bin/lark-cli");
        std::fs::create_dir_all(native.parent().unwrap()).unwrap();
        std::fs::write(&native, b"untrusted executable bytes").unwrap();
        let proof = FeishuInstallProof {
            package_dir: prefix.clone(),
            package_version: FEISHU_PACKAGE_VERSION.into(),
            package_bin_target: prefix.join("scripts/run.js"),
            native_binary: native.clone(),
            global_launcher: prefix.join("global/lark-cli"),
            postinstall: Some("node scripts/install.js".into()),
        };
        prepare_feishu_native_destination(&proof).unwrap();
        assert!(!native.exists());
        cleanup_test_directory(&prefix);
    }

    #[cfg(unix)]
    #[test]
    fn pending_native_symlink_is_never_followed_or_overwritten() {
        use std::os::unix::fs::symlink;

        let prefix = test_directory("pending-native-symlink");
        let outside = test_directory("pending-native-outside");
        let outside_file = outside.join("keep");
        std::fs::write(&outside_file, b"keep me").unwrap();
        let native = prefix.join("bin/lark-cli");
        std::fs::create_dir_all(native.parent().unwrap()).unwrap();
        symlink(&outside_file, &native).unwrap();
        let proof = FeishuInstallProof {
            package_dir: prefix.clone(),
            package_version: FEISHU_PACKAGE_VERSION.into(),
            package_bin_target: prefix.join("scripts/run.js"),
            native_binary: native,
            global_launcher: prefix.join("global/lark-cli"),
            postinstall: Some("node scripts/install.js".into()),
        };
        assert!(prepare_feishu_native_destination(&proof)
            .unwrap_err()
            .contains("不是普通文件"));
        assert_eq!(std::fs::read(&outside_file).unwrap(), b"keep me");
        cleanup_test_directory(&prefix);
        cleanup_test_directory(&outside);
    }

    #[cfg(unix)]
    #[test]
    fn tree_manifest_rejects_outbound_symlink() {
        use std::os::unix::fs::symlink;

        let prefix = test_directory("tree-outbound-link");
        symlink("/etc/passwd", prefix.join("escape")).unwrap();
        let error = feishu_tree_manifest(&prefix).unwrap_err();
        assert!(error.contains("越出独占 prefix"));
        cleanup_test_directory(&prefix);
    }

    #[cfg(unix)]
    #[test]
    fn owned_tree_manifest_rejects_all_dangling_symlinks() {
        use std::os::unix::fs::symlink;

        let prefix = test_directory("tree-dangling-internal-link");
        std::fs::create_dir_all(prefix.join("bin")).unwrap();
        symlink(
            "../lib/node_modules/@larksuite/cli/missing.js",
            prefix.join("bin/lark-cli"),
        )
        .unwrap();
        assert!(feishu_tree_manifest(&prefix).is_err());
        cleanup_test_directory(&prefix);

        let prefix = test_directory("tree-dangling-outbound-link");
        std::fs::create_dir_all(prefix.join("bin")).unwrap();
        symlink("../../../outside/missing.js", prefix.join("bin/lark-cli")).unwrap();
        assert!(feishu_tree_manifest(&prefix).is_err());
        cleanup_test_directory(&prefix);

        let prefix = test_directory("tree-dangling-absolute-dotdot-link");
        std::fs::create_dir_all(prefix.join("bin")).unwrap();
        let outside = prefix.join("..").join("outside").join("missing.js");
        symlink(outside, prefix.join("bin/lark-cli")).unwrap();
        assert!(
            feishu_tree_manifest(&prefix).is_err(),
            "absolute dangling target containing .. must be normalized before containment"
        );
        cleanup_test_directory(&prefix);
    }

    #[test]
    fn package_bin_and_global_launcher_are_verified() {
        let prefix = test_directory("launcher-proof");
        let package = feishu_package_dir(&prefix);
        std::fs::create_dir_all(package.join("scripts")).unwrap();
        std::fs::write(
            package.join("package.json"),
            br#"{"name":"@larksuite/cli","version":"1.0.70","bin":{"lark-cli":"scripts/run.js"}}"#,
        )
        .unwrap();
        std::fs::write(
            package.join("scripts").join("run.js"),
            b"#!/usr/bin/env node\n",
        )
        .unwrap();
        let bin = feishu_bin_dir(&prefix);
        std::fs::create_dir_all(&bin).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let relative = PathBuf::from("../lib/node_modules/@larksuite/cli/scripts/run.js");
            symlink(relative, bin.join("lark-cli")).unwrap();
        }
        #[cfg(windows)]
        {
            // 产品校验要求 extensionless/.cmd/.ps1 三份 launcher 与已审计
            // cmd-shim 模板整文件一致；夹具用产品生成器创建，而不是
            // 手写一份近似 .cmd。
            crate::installer::runtime::create_and_validate_cli_launchers(
                &prefix,
                "lark-cli",
                &package.join("scripts").join("run.js"),
            )
            .unwrap();
        }

        let proof = validate_feishu_install(&prefix)
            .unwrap()
            .expect("valid package and launcher");
        assert_eq!(proof.package_bin_target, package.join("scripts/run.js"));
        assert!(proof.global_launcher.starts_with(&bin));

        std::fs::write(package.join("package.json"), br#"{"name":"user-package"}"#).unwrap();
        assert!(validate_feishu_install(&prefix)
            .unwrap_err()
            .contains("包名不匹配"));
        cleanup_test_directory(&prefix);
    }

    #[test]
    fn file_delete_is_quarantined_and_mismatch_is_atomically_restored() {
        let directory = test_directory("file-quarantine");
        let path = directory.join("SKILL.md");
        std::fs::write(&path, b"user content").unwrap();

        let mismatch = quarantine_then_delete_if_matching(
            "skill-explain",
            &path,
            &sha256_hex(b"tool content"),
        )
        .expect("mismatch restore");
        assert_eq!(
            mismatch,
            QuarantineDeleteResult::FingerprintMismatchRestored
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"user content");

        let deleted = quarantine_then_delete_if_matching(
            "skill-explain",
            &path,
            &sha256_hex(b"user content"),
        )
        .expect("matching delete");
        assert_eq!(deleted, QuarantineDeleteResult::Deleted);
        assert!(!path.exists());
        cleanup_test_directory(&directory);
    }

    #[test]
    fn extension_create_never_replaces_concurrently_created_file() {
        let directory = test_directory("file-create-no-replace");
        let path = directory.join("SKILL.md");
        std::fs::write(&path, b"user editor content").unwrap();

        let error = create_extension_file_no_replace(&path, "tool content")
            .expect_err("create_new must reject an existing race winner");
        assert!(error.contains("拒绝覆盖"), "{error}");
        assert_eq!(std::fs::read(&path).unwrap(), b"user editor content");
        cleanup_test_directory(&directory);
    }

    #[test]
    fn pending_identical_external_file_is_neither_claimed_nor_deleted() {
        let directory = test_directory("pending-identical-external");
        let path = directory.join("SKILL.md");
        let desired = sha256_hex(b"identical bytes");
        std::fs::write(&path, b"identical bytes").unwrap();
        let record = new_file_record("skill-explain", OwnershipState::Pending, desired).unwrap();

        assert!(!file_record_may_claim_existing(&record));
        assert_eq!(
            quarantine_file_for_record("skill-explain", &path, &record).unwrap(),
            QuarantineDeleteResult::FingerprintMismatchRestored
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"identical bytes");
        cleanup_test_directory(&directory);
    }

    #[test]
    fn legacy_identical_external_file_is_neither_claimed_nor_deleted() {
        let directory = test_directory("legacy-identical-external");
        let path = directory.join("SKILL.md");
        std::fs::write(&path, b"identical bytes").unwrap();
        let record = legacy_record("skill-explain").unwrap();

        assert!(!file_record_may_claim_existing(&record));
        assert_eq!(
            quarantine_file_for_record("skill-explain", &path, &record).unwrap(),
            QuarantineDeleteResult::FingerprintMismatchRestored
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"identical bytes");
        cleanup_test_directory(&directory);
    }

    #[test]
    fn file_quarantine_rejects_directory_without_moving_it() {
        let directory = test_directory("file-quarantine-directory");
        let path = directory.join("SKILL.md");
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("user.txt"), b"keep").unwrap();

        let error = quarantine_then_delete_if_matching(
            "skill-explain",
            &path,
            &sha256_hex(b"tool content"),
        )
        .expect_err("directory must fail closed before rename");
        assert!(error.contains("不是可信普通文件") || error.contains("安全打开"));
        assert_eq!(std::fs::read(path.join("user.txt")).unwrap(), b"keep");
        assert!(!directory.join(".codecli-delete-skill-explain").exists());
        cleanup_test_directory(&directory);
    }

    #[cfg(unix)]
    #[test]
    fn file_quarantine_rejects_symlink_without_moving_it() {
        use std::os::unix::fs::symlink;

        let directory = test_directory("file-quarantine-symlink");
        let outside = directory.join("outside.txt");
        let path = directory.join("SKILL.md");
        std::fs::write(&outside, b"user content").unwrap();
        symlink(&outside, &path).unwrap();

        let error = quarantine_then_delete_if_matching(
            "skill-explain",
            &path,
            &sha256_hex(b"user content"),
        )
        .expect_err("symlink must fail closed before rename");
        assert!(error.contains("安全打开") || error.contains("不是可信普通文件"));
        assert!(std::fs::symlink_metadata(&path)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(std::fs::read(&outside).unwrap(), b"user content");
        assert!(!directory.join(".codecli-delete-skill-explain").exists());
        cleanup_test_directory(&directory);
    }

    #[test]
    fn interrupted_file_quarantine_is_deterministically_restored() {
        let directory = test_directory("file-quarantine-recovery");
        let path = directory.join("SKILL.md");
        std::fs::write(&path, b"owned content").unwrap();
        let quarantine = quarantine_directory_for(&path, "skill-explain").unwrap();
        std::fs::rename(&path, quarantine.join("payload")).unwrap();

        recover_file_quarantine(&path, "skill-explain").expect("recover interrupted rename");
        assert_eq!(std::fs::read(&path).unwrap(), b"owned content");
        assert!(!quarantine.exists());
        cleanup_test_directory(&directory);
    }

    #[cfg(unix)]
    #[test]
    fn interrupted_quarantine_restores_swapped_symlink_as_opaque_entry() {
        use std::os::unix::fs::symlink;

        let directory = test_directory("file-quarantine-symlink-recovery");
        let outside = directory.join("outside.txt");
        let path = directory.join("SKILL.md");
        std::fs::write(&outside, b"keep outside").unwrap();
        symlink(&outside, &path).unwrap();
        let quarantine = quarantine_directory_for(&path, "skill-explain").unwrap();
        std::fs::rename(&path, quarantine.join("payload")).unwrap();

        recover_file_quarantine(&path, "skill-explain").expect("restore opaque symlink payload");
        assert!(std::fs::symlink_metadata(&path)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(std::fs::read_link(&path).unwrap(), outside);
        assert_eq!(std::fs::read(&outside).unwrap(), b"keep outside");
        assert!(!quarantine.exists());
        cleanup_test_directory(&directory);
    }

    #[test]
    fn interrupted_quarantine_restores_swapped_directory_as_opaque_entry() {
        let directory = test_directory("file-quarantine-directory-recovery");
        let path = directory.join("SKILL.md");
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("user.txt"), b"keep directory").unwrap();
        let quarantine = quarantine_directory_for(&path, "skill-explain").unwrap();
        std::fs::rename(&path, quarantine.join("payload")).unwrap();

        recover_file_quarantine(&path, "skill-explain").expect("restore opaque directory payload");
        assert!(path.is_dir());
        assert_eq!(
            std::fs::read(path.join("user.txt")).unwrap(),
            b"keep directory"
        );
        assert!(!quarantine.exists());
        cleanup_test_directory(&directory);
    }

    #[test]
    fn quarantine_recovery_preserves_both_when_original_reappears() {
        let directory = test_directory("file-quarantine-conflict");
        let path = directory.join("SKILL.md");
        std::fs::write(&path, b"old owned content").unwrap();
        let quarantine = quarantine_directory_for(&path, "skill-explain").unwrap();
        std::fs::rename(&path, quarantine.join("payload")).unwrap();
        std::fs::write(&path, b"new user content").unwrap();

        let error = recover_file_quarantine(&path, "skill-explain").unwrap_err();
        assert!(error.contains("同时存在"));
        assert_eq!(std::fs::read(&path).unwrap(), b"new user content");
        assert_eq!(
            std::fs::read(quarantine.join("payload")).unwrap(),
            b"old owned content"
        );
        cleanup_test_directory(&directory);
    }

    #[test]
    fn known_scaffold_pruning_preserves_unknown_empty_sibling() {
        let prefix = test_directory("prune-known-scaffold");
        let unknown = prefix.join("user-empty-sibling");
        std::fs::create_dir(&unknown).unwrap();
        if cfg!(windows) {
            std::fs::create_dir_all(prefix.join("node_modules/@larksuite")).unwrap();
        } else {
            std::fs::create_dir_all(prefix.join("lib/node_modules/@larksuite")).unwrap();
            std::fs::create_dir(prefix.join("bin")).unwrap();
        }

        assert!(!prune_known_empty_feishu_scaffold(&prefix).unwrap());
        assert!(unknown.is_dir());
        if cfg!(windows) {
            assert!(!prefix.join("node_modules").exists());
        } else {
            assert!(!prefix.join("lib").exists());
            assert!(!prefix.join("bin").exists());
        }
        cleanup_test_directory(&prefix);
    }

    #[test]
    fn known_scaffold_pruning_never_deletes_unknown_content_inside_known_directory() {
        let prefix = test_directory("prune-known-content");
        let known = if cfg!(windows) {
            prefix.join("node_modules/@larksuite")
        } else {
            prefix.join("lib/node_modules/@larksuite")
        };
        std::fs::create_dir_all(&known).unwrap();
        let user_file = known.join("user.txt");
        std::fs::write(&user_file, b"keep").unwrap();

        assert!(!prune_known_empty_feishu_scaffold(&prefix).unwrap());
        assert_eq!(std::fs::read(&user_file).unwrap(), b"keep");
        cleanup_test_directory(&prefix);
    }
}
