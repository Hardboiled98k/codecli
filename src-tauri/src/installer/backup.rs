// SPDX-License-Identifier: MPL-2.0
//! 配置备份 / 恢复（仅本工具相关文件；不碰用户 hooks 整盘覆盖）

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use super::op_lock::with_op_lock;
use super::platform::{claude_config_dir, codecli_state_dir, codex_config_toml};
use super::util::{atomic_write_mode, chrono_like_now};

const BACKUP_MANIFEST_VERSION: u32 = 1;
const BACKUP_MANIFEST_FILE: &str = "manifest.json";
const BACKUP_META_FILE: &str = "meta.json";
const MAX_BACKUP_FILE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_BACKUP_META_BYTES: u64 = 256 * 1024;
const MAX_BACKUP_NOTE_BYTES: usize = 4 * 1024;
const BACKUP_CANDIDATE_NAMES: &[&str] = &[
    "ownership.json",
    "schemes.json",
    "secrets.env",
    "last-claude.json",
    "settings.json",
    "config.toml",
];

static STAGING_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BackupItem {
    pub id: String,
    pub created_at: String,
    pub note: String,
    pub files: Vec<String>,
    pub path: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupListResult {
    pub ok: bool,
    pub items: Vec<BackupItem>,
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupActionResult {
    pub ok: bool,
    pub message: String,
    pub id: Option<String>,
    pub written: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BackupManifestEntry {
    name: String,
    size: u64,
    sha256: String,
    private: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BackupManifest {
    version: u32,
    complete: bool,
    id: String,
    files: Vec<BackupManifestEntry>,
    /// 显式记录创建时不存在的候选文件。它与 `files` 的并集必须
    /// 精确等于当前 manifest 版本的候选集，避免“文件和目录项一起
    /// 被删除”被误当成一份完整的空快照。
    absent: Vec<String>,
}

#[derive(Debug, Clone)]
struct ValidatedBackup {
    item: BackupItem,
    contents: BTreeMap<String, String>,
}

fn is_known_candidate(name: &str) -> bool {
    BACKUP_CANDIDATE_NAMES.contains(&name)
}

fn manifest_private(name: &str) -> bool {
    // 所有候选文件都可能含绝对路径、Key 或 provider 认证信息；
    // 在备份边界内统一按私密数据处理。meta 只是列表展示信息。
    is_known_candidate(name)
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .as_bytes()
            .iter()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn is_symlink_or_reparse(meta: &std::fs::Metadata) -> bool {
    if meta.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        // FILE_ATTRIBUTE_REPARSE_POINT
        return meta.file_attributes() & 0x0000_0400 != 0;
    }
    #[cfg(not(windows))]
    false
}

fn create_private_dir(path: &Path, recursive: bool) -> Result<(), String> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(recursive);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(path)
        .map_err(|e| format!("创建私密目录 {} 失败: {}", path.display(), e))
}

fn open_directory_nofollow(path: &Path, label: &str) -> Result<std::fs::File, String> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT
        options.custom_flags(0x0200_0000 | 0x0020_0000);
    }
    let file = options
        .open(path)
        .map_err(|e| format!("打开 {} 失败: {}", label, e))?;
    let meta = file
        .metadata()
        .map_err(|e| format!("读取 {} 元数据失败: {}", label, e))?;
    if is_symlink_or_reparse(&meta) || !meta.is_dir() {
        return Err(format!("拒绝非真实目录或符号链接: {}", label));
    }
    Ok(file)
}

fn ensure_private_directory(path: &Path, label: &str) -> Result<(), String> {
    let meta =
        std::fs::symlink_metadata(path).map_err(|e| format!("读取 {} 元数据失败: {}", label, e))?;
    if is_symlink_or_reparse(&meta) || !meta.is_dir() {
        return Err(format!("拒绝非真实目录或符号链接: {}", label));
    }
    let file = open_directory_nofollow(path, label)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o700))
            .map_err(|e| format!("设置 {} 权限失败: {}", label, e))?;
    }
    #[cfg(not(unix))]
    drop(file);
    Ok(())
}

fn validate_private_directory(path: &Path, label: &str) -> Result<(), String> {
    let file = open_directory_nofollow(path, label)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = file
            .metadata()
            .map_err(|e| format!("读取 {} 权限失败: {}", label, e))?
            .permissions()
            .mode()
            & 0o777;
        if mode != 0o700 {
            return Err(format!("{} 权限必须为 0700，当前为 {:04o}", label, mode));
        }
    }
    #[cfg(not(unix))]
    drop(file);
    Ok(())
}

fn sync_directory(path: &Path, label: &str) -> Result<(), String> {
    open_directory_nofollow(path, label)?
        .sync_all()
        .map_err(|e| format!("fsync {} 失败: {}", label, e))
}

fn backups_root() -> Result<PathBuf, String> {
    let dir = codecli_state_dir().ok_or("找不到状态目录")?;
    let p = dir.join("backups");
    match std::fs::symlink_metadata(&p) {
        Ok(meta) => {
            if is_symlink_or_reparse(&meta) || !meta.is_dir() {
                return Err("备份根目录不是可信真实目录".into());
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => create_private_dir(&p, true)?,
        Err(e) => return Err(format!("检查备份根目录失败: {}", e)),
    }
    ensure_private_directory(&p, "备份根目录")?;
    Ok(p)
}

fn stamp_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let d = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    format!("bak_{}_{:09}", d.as_secs(), d.subsec_nanos())
}

/// 仅允许 bak_数字_9位数字
fn validate_backup_id(id: &str) -> Result<(), String> {
    let id = id.trim();
    if id.is_empty() || id == "." || id == ".." {
        return Err("非法备份 id".into());
    }
    if id.contains('/') || id.contains('\\') || id.contains("..") {
        return Err("非法备份 id".into());
    }
    let re_ok = id
        .strip_prefix("bak_")
        .and_then(|rest| rest.split_once('_'))
        .is_some_and(|(seconds, nanos)| {
            !seconds.is_empty()
                && seconds.len() <= 20
                && seconds.chars().all(|c| c.is_ascii_digit())
                && nanos.len() == 9
                && nanos.chars().all(|c| c.is_ascii_digit())
        })
        && id.matches('_').count() == 2
        && id.len() < 64;
    if !re_ok {
        return Err("备份 id 格式无效".into());
    }
    Ok(())
}

fn candidates() -> Vec<(String, PathBuf)> {
    let mut list = Vec::new();
    if let Some(state) = codecli_state_dir() {
        list.push(("ownership.json".into(), state.join("ownership.json")));
        list.push(("schemes.json".into(), state.join("schemes.json")));
        list.push(("secrets.env".into(), state.join("secrets.env")));
        list.push(("last-claude.json".into(), state.join("last-claude.json")));
    }
    if let Some(claude) = claude_config_dir() {
        list.push(("settings.json".into(), claude.join("settings.json")));
    }
    if let Some(toml) = codex_config_toml() {
        list.push(("config.toml".into(), toml));
    }
    list
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
        // 读取 reparse point 本身，而不是跟随到目标。
        options.custom_flags(0x0020_0000); // FILE_FLAG_OPEN_REPARSE_POINT
    }
    let file = options
        .open(path)
        .map_err(|e| format!("打开 {} 失败: {}", label, e))?;
    let meta = file
        .metadata()
        .map_err(|e| format!("读取 {} 元数据失败: {}", label, e))?;
    if is_symlink_or_reparse(&meta) || !meta.is_file() {
        return Err(format!("拒绝符号链接或非普通文件: {}", label));
    }
    Ok(file)
}

fn read_optional_regular_bytes_nofollow(
    path: &Path,
    label: &str,
    max_bytes: u64,
) -> Result<Option<Vec<u8>>, String> {
    let before = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("读取 {} 元数据失败: {}", label, e)),
    };
    if is_symlink_or_reparse(&before) {
        return Err(format!("拒绝符号链接: {}", label));
    }
    if !before.is_file() {
        return Err(format!("{} 不是普通文件", label));
    }
    if before.len() > max_bytes {
        return Err(format!("{} 超过大小上限 {} 字节", label, max_bytes));
    }

    let mut file = open_regular_file_nofollow(path, label)?;
    let opened = file
        .metadata()
        .map_err(|e| format!("读取 {} 元数据失败: {}", label, e))?;
    if opened.len() > max_bytes {
        return Err(format!("{} 超过大小上限 {} 字节", label, max_bytes));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if before.dev() != opened.dev() || before.ino() != opened.ino() {
            return Err(format!("{} 在打开期间被替换，已中止", label));
        }
    }

    let mut body = Vec::with_capacity(opened.len().min(max_bytes) as usize);
    std::io::Read::by_ref(&mut file)
        .take(max_bytes + 1)
        .read_to_end(&mut body)
        .map_err(|e| format!("读取 {} 失败: {}", label, e))?;
    if body.len() as u64 > max_bytes {
        return Err(format!("{} 超过大小上限 {} 字节", label, max_bytes));
    }
    let after = file
        .metadata()
        .map_err(|e| format!("复核 {} 元数据失败: {}", label, e))?;
    if after.len() != opened.len() || body.len() as u64 != after.len() {
        return Err(format!("{} 在读取期间发生变化，已中止", label));
    }
    Ok(Some(body))
}

fn read_required_regular_bytes_nofollow(
    path: &Path,
    label: &str,
    max_bytes: u64,
) -> Result<Vec<u8>, String> {
    read_optional_regular_bytes_nofollow(path, label, max_bytes)?
        .ok_or_else(|| format!("{} 缺失", label))
}

fn write_new_private_file(path: &Path, bytes: &[u8], label: &str) -> Result<(), String> {
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
        options.custom_flags(0x0020_0000); // FILE_FLAG_OPEN_REPARSE_POINT
    }
    let mut file = options
        .open(path)
        .map_err(|e| format!("创建 {} 失败: {}", label, e))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("设置 {} 权限失败: {}", label, e))?;
    }
    file.write_all(bytes)
        .map_err(|e| format!("写入 {} 失败: {}", label, e))?;
    file.sync_all()
        .map_err(|e| format!("fsync {} 失败: {}", label, e))
}

fn verify_private_file_mode(path: &Path, label: &str) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let file = open_regular_file_nofollow(path, label)?;
        let mode = file
            .metadata()
            .map_err(|e| format!("读取 {} 权限失败: {}", label, e))?
            .permissions()
            .mode()
            & 0o777;
        if mode != 0o600 {
            return Err(format!("{} 权限必须为 0600，当前为 {:04o}", label, mode));
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (path, label);
    }
    Ok(())
}

fn manifest_entry(name: &str, bytes: &[u8]) -> BackupManifestEntry {
    BackupManifestEntry {
        name: name.to_string(),
        size: bytes.len() as u64,
        sha256: sha256_hex(bytes),
        private: manifest_private(name),
    }
}

fn staging_name(id: &str) -> String {
    let serial = STAGING_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!(".staging-{}-{}-{}", id, std::process::id(), serial)
}

fn is_staging_name(name: &str) -> bool {
    name.starts_with(".staging-")
}

fn is_backup_payload_name(name: &str) -> bool {
    name == BACKUP_MANIFEST_FILE || name == BACKUP_META_FILE || is_known_candidate(name)
}

fn remove_backup_dir_shallow(root: &Path, dir: &Path, label: &str) -> Result<(), String> {
    let meta = std::fs::symlink_metadata(dir).map_err(|e| format!("检查 {} 失败: {}", label, e))?;
    if is_symlink_or_reparse(&meta) {
        return Err(format!("拒绝删除符号链接或 reparse point: {}", label));
    }
    if !meta.is_dir() || dir.parent() != Some(root) {
        return Err(format!("拒绝删除非备份根目录直接子目录: {}", label));
    }

    let mut entries = Vec::new();
    for entry in std::fs::read_dir(dir).map_err(|e| format!("读取 {} 失败: {}", label, e))? {
        entries.push(entry.map_err(|e| format!("遍历 {} 失败: {}", label, e))?);
    }
    let mut files = Vec::new();
    for entry in entries {
        let path = entry.path();
        let entry_label = format!("{} 中的 {}", label, path.display());
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| format!("拒绝删除含非 UTF-8 条目的 {}", label))?;
        if !is_backup_payload_name(&name) {
            return Err(format!("拒绝删除含未知条目 {} 的 {}", name, label));
        }
        let meta = std::fs::symlink_metadata(&path)
            .map_err(|e| format!("检查 {} 失败: {}", entry_label, e))?;
        if is_symlink_or_reparse(&meta) {
            return Err(format!("拒绝删除含符号链接的 {}", label));
        }
        if !meta.is_file() {
            return Err(format!("拒绝递归删除含子目录的 {}", label));
        }
        files.push(path);
    }
    // 完整 preflight 通过后才开始 unlink，避免发现后续 symlink/子目录时
    // 已经部分删除前面的文件。
    for path in files {
        std::fs::remove_file(&path).map_err(|e| format!("删除 {} 失败: {}", path.display(), e))?;
    }
    std::fs::remove_dir(dir).map_err(|e| format!("删除 {} 失败: {}", label, e))?;
    sync_directory(root, "备份根目录")
}

fn validate_manifest_shape(manifest: &BackupManifest, expected_id: &str) -> Result<(), String> {
    if manifest.version != BACKUP_MANIFEST_VERSION {
        return Err(format!("不支持的备份 manifest 版本: {}", manifest.version));
    }
    if !manifest.complete {
        return Err("备份 manifest 未完成".into());
    }
    if manifest.id != expected_id {
        return Err("备份 manifest id 与目录不匹配".into());
    }

    let mut represented = BTreeSet::new();
    let mut has_meta = false;
    for entry in &manifest.files {
        let allowed = entry.name == BACKUP_META_FILE || is_known_candidate(&entry.name);
        if !allowed {
            return Err(format!("manifest 包含未知备份项: {}", entry.name));
        }
        if !represented.insert(entry.name.clone()) {
            return Err(format!("manifest 备份项重复: {}", entry.name));
        }
        if entry.name == BACKUP_META_FILE {
            has_meta = true;
        }
        if !valid_sha256(&entry.sha256) {
            return Err(format!("manifest {} sha256 无效", entry.name));
        }
        let limit = if entry.name == BACKUP_META_FILE {
            MAX_BACKUP_META_BYTES
        } else {
            MAX_BACKUP_FILE_BYTES
        };
        if entry.size > limit {
            return Err(format!("manifest {} size 超限", entry.name));
        }
        let expected_private = manifest_private(&entry.name);
        if entry.private != expected_private {
            return Err(format!("manifest {} private 标志无效", entry.name));
        }
    }
    if !has_meta {
        return Err("manifest 缺少 meta.json 项".into());
    }
    for name in &manifest.absent {
        if !is_known_candidate(name) {
            return Err(format!("manifest 包含未知缺失项: {}", name));
        }
        if !represented.insert(name.clone()) {
            return Err(format!("manifest 备份项重复或同时标记缺失: {}", name));
        }
    }
    for expected in BACKUP_CANDIDATE_NAMES {
        if !represented.contains(*expected) {
            return Err(format!("manifest 不完整，缺少候选项: {}", expected));
        }
    }
    Ok(())
}

fn validate_backup_directory(root: &Path, id: &str) -> Result<ValidatedBackup, String> {
    validate_backup_id(id)?;
    validate_private_directory(root, "备份根目录")?;
    let dir = root.join(id);
    if dir.parent() != Some(root) {
        return Err("备份目录越界".into());
    }
    validate_private_directory(&dir, &format!("备份 {}", id))?;

    let mut actual = BTreeSet::new();
    for entry in std::fs::read_dir(&dir).map_err(|e| format!("读取备份 {} 失败: {}", id, e))?
    {
        let entry = entry.map_err(|e| format!("遍历备份 {} 失败: {}", id, e))?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| format!("备份 {} 包含非 UTF-8 文件名", id))?;
        let meta = std::fs::symlink_metadata(entry.path())
            .map_err(|e| format!("检查备份项 {} 失败: {}", name, e))?;
        if is_symlink_or_reparse(&meta) {
            return Err(format!("备份 {} 包含符号链接: {}", id, name));
        }
        if !meta.is_file() {
            return Err(format!("备份 {} 包含非普通文件: {}", id, name));
        }
        actual.insert(name);
    }

    if !actual.contains(BACKUP_MANIFEST_FILE) {
        return Err(format!("备份 {} 缺少 manifest.json", id));
    }
    let manifest_path = dir.join(BACKUP_MANIFEST_FILE);
    verify_private_file_mode(&manifest_path, "manifest.json")?;
    let manifest_bytes = read_required_regular_bytes_nofollow(
        &manifest_path,
        "manifest.json",
        MAX_BACKUP_META_BYTES,
    )?;
    let manifest: BackupManifest = serde_json::from_slice(&manifest_bytes)
        .map_err(|e| format!("备份 manifest 解析失败: {}", e))?;
    validate_manifest_shape(&manifest, id)?;

    let mut expected_top = BTreeSet::from([BACKUP_MANIFEST_FILE.to_string()]);
    for entry in &manifest.files {
        expected_top.insert(entry.name.clone());
    }
    if actual != expected_top {
        let unknown: Vec<_> = actual.difference(&expected_top).cloned().collect();
        let missing: Vec<_> = expected_top.difference(&actual).cloned().collect();
        return Err(format!(
            "备份顶层条目与 manifest 不一致（未知: {:?}，缺失: {:?}）",
            unknown, missing
        ));
    }

    let mut contents = BTreeMap::new();
    for entry in &manifest.files {
        let path = dir.join(&entry.name);
        verify_private_file_mode(&path, &format!("备份项 {}", entry.name))?;
        let limit = if entry.name == BACKUP_META_FILE {
            MAX_BACKUP_META_BYTES
        } else {
            MAX_BACKUP_FILE_BYTES
        };
        let bytes =
            read_required_regular_bytes_nofollow(&path, &format!("备份项 {}", entry.name), limit)?;
        if bytes.len() as u64 != entry.size {
            return Err(format!("备份项 {} size 与 manifest 不符", entry.name));
        }
        if sha256_hex(&bytes) != entry.sha256 {
            return Err(format!("备份项 {} sha256 与 manifest 不符", entry.name));
        }
        let text = String::from_utf8(bytes)
            .map_err(|_| format!("备份项 {} 不是 UTF-8 文本", entry.name))?;
        contents.insert(entry.name.clone(), text);
    }

    let meta_text = contents
        .get(BACKUP_META_FILE)
        .ok_or("备份缺少 meta.json 内容")?;
    let mut item: BackupItem =
        serde_json::from_str(meta_text).map_err(|e| format!("备份 meta.json 解析失败: {}", e))?;
    if item.id != id {
        return Err("备份 meta.json id 与目录不匹配".into());
    }
    if item.created_at.is_empty() || item.note.len() > MAX_BACKUP_NOTE_BYTES {
        return Err("备份 meta.json 时间或备注无效".into());
    }
    let expected_reports: Vec<String> = BACKUP_CANDIDATE_NAMES
        .iter()
        .filter(|name| contents.contains_key(**name))
        .map(|name| format!("{} → {}", name, dir.join(name).display()))
        .collect();
    if item.files != expected_reports {
        return Err("备份 meta.json files 与 manifest 不匹配".into());
    }
    if item.path != dir.display().to_string() {
        return Err("备份 meta.json path 与实际目录不匹配".into());
    }
    // 列表层始终使用已验证的实际路径，不把盘上字符串当作路径权威。
    item.path = dir.display().to_string();
    contents.remove(BACKUP_META_FILE);
    Ok(ValidatedBackup { item, contents })
}

fn publish_backup_at_root(
    root: &Path,
    id: &str,
    note: String,
    source_candidates: &[(String, PathBuf)],
) -> Result<BackupActionResult, String> {
    validate_backup_id(id)?;
    if note.len() > MAX_BACKUP_NOTE_BYTES {
        return Err(format!("备注过长（最多 {} 字节）", MAX_BACKUP_NOTE_BYTES));
    }
    match std::fs::symlink_metadata(root) {
        Ok(_) => ensure_private_directory(root, "备份根目录")?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            create_private_dir(root, true)?;
            ensure_private_directory(root, "备份根目录")?;
        }
        Err(e) => return Err(format!("检查备份根目录失败: {}", e)),
    }

    let final_dir = root.join(id);
    match std::fs::symlink_metadata(&final_dir) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Ok(_) => return Err(format!("备份 id 已存在: {}", id)),
        Err(e) => return Err(format!("检查备份 id 失败: {}", e)),
    }

    let mut by_name = BTreeMap::new();
    for (name, path) in source_candidates {
        if !is_known_candidate(name) {
            return Err(format!("未知备份候选: {}", name));
        }
        if by_name.insert(name.clone(), path.clone()).is_some() {
            return Err(format!("重复备份候选: {}", name));
        }
    }

    let staging = loop {
        let candidate = root.join(staging_name(id));
        match create_private_dir(&candidate, false) {
            Ok(()) => break candidate,
            Err(error) => match std::fs::symlink_metadata(&candidate) {
                Ok(_) => continue,
                Err(meta_error) if meta_error.kind() == std::io::ErrorKind::NotFound => {
                    return Err(error)
                }
                Err(meta_error) => {
                    return Err(format!("{}；复核 staging 失败: {}", error, meta_error))
                }
            },
        }
    };
    let mut published = false;
    let build = (|| -> Result<BackupActionResult, String> {
        ensure_private_directory(&staging, "备份 staging 目录")?;
        let mut manifest_files = Vec::new();
        let mut absent = Vec::new();
        let mut reports = Vec::new();

        for name in BACKUP_CANDIDATE_NAMES {
            let Some(source) = by_name.get(*name) else {
                absent.push((*name).to_string());
                continue;
            };
            let Some(bytes) = read_optional_regular_bytes_nofollow(
                source,
                &format!("备份源 {}", source.display()),
                MAX_BACKUP_FILE_BYTES,
            )?
            else {
                absent.push((*name).to_string());
                continue;
            };
            // 备份的全部对象都是文本配置；创建时就拒绝非 UTF-8，
            // 不把无法恢复的数据发布为“完整备份”。
            std::str::from_utf8(&bytes)
                .map_err(|_| format!("备份源 {} 不是 UTF-8 文本", source.display()))?;
            let destination = staging.join(name);
            write_new_private_file(&destination, &bytes, &format!("备份项 {}", name))?;
            manifest_files.push(manifest_entry(name, &bytes));
            reports.push(format!("{} → {}", name, final_dir.join(name).display()));
        }

        let meta = BackupItem {
            id: id.to_string(),
            created_at: chrono_like_now(),
            note,
            files: reports.clone(),
            path: final_dir.display().to_string(),
        };
        let meta_bytes =
            serde_json::to_vec_pretty(&meta).map_err(|e| format!("序列化备份 meta 失败: {}", e))?;
        if meta_bytes.len() as u64 > MAX_BACKUP_META_BYTES {
            return Err("备份 meta.json 超过大小上限".into());
        }
        write_new_private_file(
            &staging.join(BACKUP_META_FILE),
            &meta_bytes,
            "备份 meta.json",
        )?;
        manifest_files.push(manifest_entry(BACKUP_META_FILE, &meta_bytes));

        let manifest = BackupManifest {
            version: BACKUP_MANIFEST_VERSION,
            complete: true,
            id: id.to_string(),
            files: manifest_files,
            absent,
        };
        validate_manifest_shape(&manifest, id)?;
        let manifest_bytes = serde_json::to_vec_pretty(&manifest)
            .map_err(|e| format!("序列化备份 manifest 失败: {}", e))?;
        if manifest_bytes.len() as u64 > MAX_BACKUP_META_BYTES {
            return Err("备份 manifest.json 超过大小上限".into());
        }
        // manifest 最后写入；目录在 rename 之前始终不可被列表/恢复。
        write_new_private_file(
            &staging.join(BACKUP_MANIFEST_FILE),
            &manifest_bytes,
            "备份 manifest.json",
        )?;
        sync_directory(&staging, "备份 staging 目录")?;

        // 同一根目录内的 rename 将已 fsync 的完整快照一次性发布。
        match std::fs::symlink_metadata(&final_dir) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Ok(_) => return Err(format!("发布前发现备份 id 已被占用: {}", id)),
            Err(e) => return Err(format!("发布前复核备份 id 失败: {}", e)),
        }
        std::fs::rename(&staging, &final_dir)
            .map_err(|e| format!("原子发布备份 {} 失败: {}", id, e))?;
        published = true;
        sync_directory(root, "备份根目录")?;

        // 发布后再用与 list/restore 相同的信任边界复核一次。
        validate_backup_directory(root, id)?;
        Ok(BackupActionResult {
            ok: true,
            message: format!("已备份 {} 个文件 · {}", reports.len(), id),
            id: Some(id.to_string()),
            written: reports,
        })
    })();

    match build {
        Ok(result) => Ok(result),
        Err(error) => {
            // rename 发布后再出错，可能是并发篡改或磁盘校验失败。
            // 此时不再自动删已发布目录，避免把外部刚放入的
            // 同名普通文件当成本次 staging 副作用删掉。该目录
            // 会被 list/restore 完整性校验自动隐藏，供人工核对。
            if published {
                return Err(format!(
                    "{error}；备份已原子发布但复核失败，已保留 {} 且不会出现在可恢复列表",
                    final_dir.display()
                ));
            }
            let cleanup_target = &staging;
            let cleanup = match std::fs::symlink_metadata(cleanup_target) {
                Ok(_) => remove_backup_dir_shallow(root, cleanup_target, "未完成备份"),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(format!("检查未完成备份失败: {}", e)),
            };
            match cleanup {
                Ok(()) => Err(error),
                Err(cleanup_error) => Err(format!(
                    "{}；清理未完成备份也失败: {}",
                    error, cleanup_error
                )),
            }
        }
    }
}

pub fn create_backup_sync(note: Option<String>) -> Result<BackupActionResult, String> {
    create_backup_inner(note, true, None)
}

fn create_backup_inner(
    note: Option<String>,
    do_prune: bool,
    protect_id: Option<&str>,
) -> Result<BackupActionResult, String> {
    let root = backups_root()?;
    let id = stamp_id();
    let result = publish_backup_at_root(
        &root,
        &id,
        note.unwrap_or_else(|| "手动备份".into()),
        &candidates(),
    )?;

    if do_prune {
        prune_old_backups(20, protect_id)
            .map_err(|e| format!("备份 {} 已安全发布，但清理旧备份失败: {}", id, e))?;
    }
    Ok(result)
}

fn prune_old_backups(keep: usize, protect_id: Option<&str>) -> Result<(), String> {
    let root = backups_root()?;
    prune_old_backups_at_root(&root, keep, protect_id)
}

fn prune_old_backups_at_root(
    root: &Path,
    keep: usize,
    protect_id: Option<&str>,
) -> Result<(), String> {
    ensure_private_directory(root, "备份根目录")?;
    let mut dirs = Vec::new();
    for entry in std::fs::read_dir(root).map_err(|e| format!("读取备份根目录失败: {}", e))?
    {
        let entry = entry.map_err(|e| format!("遍历备份根目录失败: {}", e))?;
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if is_staging_name(&name) || validate_backup_id(&name).is_err() {
            continue;
        }
        let meta = std::fs::symlink_metadata(entry.path())
            .map_err(|e| format!("检查备份 {} 失败: {}", name, e))?;
        if is_symlink_or_reparse(&meta) {
            return Err(format!("拒绝 prune 符号链接备份: {}", name));
        }
        if !meta.is_dir() {
            return Err(format!("备份 id {} 不是目录", name));
        }
        // 旧版/损坏备份不纳入自动删除候选，避免对无法完整验证的
        // 目录执行删除。真正的遍历/删除错误仍会向上传播。
        if validate_backup_directory(root, &name).is_ok() {
            dirs.push(entry);
        }
    }
    // 新→旧
    dirs.sort_by_key(|e| std::cmp::Reverse(e.file_name()));
    let mut kept = 0usize;
    for d in dirs {
        let name = d.file_name().to_string_lossy().to_string();
        if protect_id == Some(name.as_str()) {
            // 受保护备份始终保留，不占 keep 配额之外的删除优先
            continue;
        }
        if kept < keep {
            kept += 1;
            continue;
        }
        remove_backup_dir_shallow(root, &d.path(), &format!("旧备份 {}", name))?;
    }
    Ok(())
}

pub fn list_backups_sync() -> Result<BackupListResult, String> {
    let root = backups_root()?;
    let (items, rejected) = list_backups_at_root(&root)?;
    let n = items.len();
    Ok(BackupListResult {
        ok: true,
        items,
        message: if rejected == 0 {
            format!("{} 份备份", n)
        } else {
            format!(
                "{} 份可验证备份（已隐藏 {} 份不完整/损坏备份）",
                n, rejected
            )
        },
    })
}

fn list_backups_at_root(root: &Path) -> Result<(Vec<BackupItem>, usize), String> {
    ensure_private_directory(root, "备份根目录")?;
    let mut ids = Vec::new();
    let mut rejected = 0usize;
    for entry in std::fs::read_dir(root).map_err(|e| format!("读取备份根目录失败: {}", e))?
    {
        let entry = entry.map_err(|e| format!("遍历备份根目录失败: {}", e))?;
        let Ok(name) = entry.file_name().into_string() else {
            rejected += 1;
            continue;
        };
        if is_staging_name(&name) {
            continue;
        }
        if validate_backup_id(&name).is_err() {
            continue;
        }
        ids.push(name);
    }
    ids.sort_by(|a, b| b.cmp(a));
    let mut items = Vec::new();
    for id in ids {
        match validate_backup_directory(root, &id) {
            Ok(validated) => items.push(validated.item),
            Err(_) => rejected += 1,
        }
    }
    Ok((items, rejected))
}

pub fn restore_backup_sync(backup_id: String) -> Result<BackupActionResult, String> {
    let id = backup_id.trim();
    validate_backup_id(id)?;
    let root = backups_root()?;
    let validated = validate_backup_directory(&root, id)
        .map_err(|e| format!("备份不可恢复（完整性校验失败）: {}", e))?;

    // 严格分两阶段：先完整读取备份/当前文件，解析 JSON/TOML，并生成
    // 所有目标内容。任一 preflight 失败时，尚未触碰任何目标文件。
    let restore_candidates = candidates();
    let plan = prepare_restore_plan_from_contents(&validated.contents, &restore_candidates)?;

    // 恢复前必须成功自动备份；失败则中止；且 prune 不得删除正在恢复的源
    create_backup_inner(Some(format!("restore-前自动 · {}", id)), true, Some(id))
        .map_err(|e| format!("安全备份失败，已中止恢复: {}", e))?;

    // 第二阶段只执行已生成的 plan。每个写入都是原子替换；任一步失败
    // 都会逆序原子恢复所有已触碰目标。
    let written = execute_restore_plan(&plan, None)?;

    Ok(BackupActionResult {
        ok: true,
        message: format!(
            "已从 {} 事务性恢复 {} 项。settings 精确恢复受管 env；codex 精确恢复 model/provider，并保留 hooks/MCP/其他 provider。新开终端后生效。",
            id,
            written.len()
        ),
        id: Some(id.to_string()),
        written,
    })
}

const CLAUDE_ENV_KEYS: &[&str] = &[
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_MODEL",
];
const CODEX_PROVIDER_ID: &str = "codecli_installer";

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileSnapshot {
    content: String,
    /// Unix 权限位；其他平台为 None。回滚时尽量还原元数据。
    mode: Option<u32>,
}

#[derive(Debug, Clone)]
enum DesiredTarget {
    Present { content: String, private: bool },
    Absent,
}

#[derive(Debug, Clone)]
struct RestoreOperation {
    name: String,
    path: PathBuf,
    original: Option<FileSnapshot>,
    desired: DesiredTarget,
    report: String,
}

#[derive(Debug, Clone, Default)]
struct RestorePlan {
    operations: Vec<RestoreOperation>,
}

fn metadata_mode(meta: &std::fs::Metadata) -> Option<u32> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        Some(meta.permissions().mode())
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        None
    }
}

/// 读取可选的普通文本文件；符号链接/目录/非 UTF-8 都在 preflight 阶段拒绝。
#[cfg(test)]
fn read_optional_regular_text(path: &Path, label: &str) -> Result<Option<String>, String> {
    read_optional_regular_bytes_nofollow(path, label, MAX_BACKUP_FILE_BYTES)?
        .map(|bytes| {
            String::from_utf8(bytes).map_err(|_| format!("完整读取 {} 失败: 非 UTF-8 文本", label))
        })
        .transpose()
}

fn snapshot_target(path: &Path, label: &str) -> Result<Option<FileSnapshot>, String> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("读取当前 {} 元数据失败: {}", label, e)),
    };
    if meta.file_type().is_symlink() {
        return Err(format!("拒绝覆盖符号链接目标: {}", label));
    }
    if !meta.is_file() {
        return Err(format!("当前 {} 不是普通文件", label));
    }
    let content = read_optional_regular_bytes_nofollow(path, label, MAX_BACKUP_FILE_BYTES)?
        .ok_or_else(|| format!("当前 {} 在读取期间消失", label))
        .and_then(|bytes| {
            String::from_utf8(bytes).map_err(|_| format!("完整读取当前 {} 失败: 非 UTF-8", label))
        })?;
    Ok(Some(FileSnapshot {
        content,
        mode: metadata_mode(&meta),
    }))
}

fn parse_json_object(raw: &str, label: &str) -> Result<serde_json::Value, String> {
    let value: serde_json::Value =
        serde_json::from_str(raw).map_err(|e| format!("{} 解析失败: {}", label, e))?;
    if !value.is_object() {
        return Err(format!("{} 根节点不是对象", label));
    }
    Ok(value)
}

/// 根据备份中受管 env 的“存在/缺失”精确生成目标 settings。
/// 备份缺少某个受管 key 即从当前文件删除；hooks 和非受管 env 不动。
fn generate_claude_settings_content(
    backup: Option<&str>,
    current: Option<&str>,
) -> Result<Option<String>, String> {
    let mut backup_values = std::collections::BTreeMap::new();
    if let Some(raw) = backup {
        let root = parse_json_object(raw, "备份 settings.json")?;
        if let Some(env_value) = root.get("env") {
            let env = env_value
                .as_object()
                .ok_or("备份 settings.json 的 env 不是对象")?;
            for key in CLAUDE_ENV_KEYS {
                if let Some(value) = env.get(*key) {
                    backup_values.insert((*key).to_string(), value.clone());
                }
            }
        }
    }

    let had_current = current.is_some();
    let mut root = match current {
        Some(raw) => parse_json_object(raw, "当前 settings.json")?,
        None => serde_json::json!({}),
    };
    let obj = root.as_object_mut().ok_or("settings.json 根不是对象")?;

    if obj.contains_key("env") || !backup_values.is_empty() {
        if !obj.contains_key("env") {
            obj.insert("env".into(), serde_json::json!({}));
        }
        let env = obj
            .get_mut("env")
            .and_then(|v| v.as_object_mut())
            .ok_or("当前 settings.json 的 env 不是对象")?;
        for key in CLAUDE_ENV_KEYS {
            match backup_values.get(*key) {
                Some(value) => {
                    env.insert((*key).to_string(), value.clone());
                }
                None => {
                    env.remove(*key);
                }
            }
        }
        if env.is_empty() {
            obj.remove("env");
        }
    }

    // 当前文件本就不存在，且备份中也没有任何受管字段时，
    // 不为了一个空对象创建 settings.json。
    if !had_current && obj.is_empty() {
        return Ok(None);
    }
    serde_json::to_string_pretty(&root)
        .map(Some)
        .map_err(|e| format!("生成 settings.json 失败: {}", e))
}

fn parse_codex_doc(raw: &str, label: &str) -> Result<toml_edit::DocumentMut, String> {
    if raw.trim().is_empty() {
        return Ok(toml_edit::DocumentMut::new());
    }
    raw.parse()
        .map_err(|e| format!("{} 解析失败: {}", label, e))
}

fn codex_provider_item(doc: &toml_edit::DocumentMut) -> Option<toml_edit::Item> {
    doc.get("model_providers")
        .and_then(|item| item.as_table_like())
        .and_then(|providers| providers.get(CODEX_PROVIDER_ID))
        .cloned()
}

/// 精确恢复 model/model_provider/整个 codecli_installer provider Item。
/// 备份中缺失就从当前文件删除，其他 provider 和 MCP 表原样保留。
fn generate_codex_toml_content(
    backup: Option<&str>,
    current: Option<&str>,
) -> Result<Option<String>, String> {
    use toml_edit::{DocumentMut, Item, Table};

    let backup_doc = match backup {
        Some(raw) => parse_codex_doc(raw, "备份 config.toml")?,
        None => DocumentMut::new(),
    };
    let backup_provider = codex_provider_item(&backup_doc);
    let has_backup_managed = backup_doc.get("model").is_some()
        || backup_doc.get("model_provider").is_some()
        || backup_provider.is_some();

    let had_current = current.is_some();
    let mut doc = match current {
        Some(raw) => parse_codex_doc(raw, "当前 config.toml")?,
        None => DocumentMut::new(),
    };

    for key in ["model", "model_provider"] {
        match backup_doc.get(key).cloned() {
            Some(item) => {
                doc.as_table_mut().insert(key, item);
            }
            None => {
                doc.as_table_mut().remove(key);
            }
        }
    }

    match backup_provider {
        Some(provider) => {
            if doc.get("model_providers").is_none() {
                doc.as_table_mut()
                    .insert("model_providers", Item::Table(Table::new()));
            } else if let Some(inline) = doc
                .get("model_providers")
                .and_then(|item| item.as_inline_table())
                .cloned()
            {
                // InlineTable 只能容纳 Value；备份 provider 可能是普通 Table。
                // 先无损转成普通 Table，保留其他 inline provider，避免
                // TableLike::insert 因表示形式不兼容而 panic。
                let mut table = Table::new();
                for (key, value) in inline.iter() {
                    table.insert(key, Item::Value(value.clone()));
                }
                doc.as_table_mut()
                    .insert("model_providers", Item::Table(table));
            }
            let providers = doc
                .get_mut("model_providers")
                .and_then(|item| item.as_table_like_mut())
                .ok_or("当前 config.toml 的 model_providers 不是表")?;
            providers.insert(CODEX_PROVIDER_ID, provider);
        }
        None => {
            if let Some(providers) = doc
                .get_mut("model_providers")
                .and_then(|item| item.as_table_like_mut())
            {
                providers.remove(CODEX_PROVIDER_ID);
            }
        }
    }

    if !had_current && !has_backup_managed {
        return Ok(None);
    }
    Ok(Some(doc.to_string()))
}

fn validate_private_backup(name: &str, content: &str) -> Result<(), String> {
    if matches!(name, "ownership.json" | "schemes.json" | "last-claude.json") {
        serde_json::from_str::<serde_json::Value>(content)
            .map_err(|e| format!("备份 {} 解析失败: {}", name, e))?;
    }
    Ok(())
}

fn private_mode_is_600(mode: Option<u32>) -> bool {
    #[cfg(unix)]
    {
        mode.map(|m| m & 0o777 == 0o600).unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        let _ = mode;
        true
    }
}

fn desired_differs(desired: &DesiredTarget, original: &Option<FileSnapshot>) -> bool {
    match (desired, original) {
        (DesiredTarget::Absent, None) => false,
        (DesiredTarget::Absent, Some(_)) => true,
        (DesiredTarget::Present { .. }, None) => true,
        (
            DesiredTarget::Present { content, private },
            Some(FileSnapshot { content: old, mode }),
        ) => old != content || (*private && !private_mode_is_600(*mode)),
    }
}

/// 生成完整 restore plan：此函数只读/解析/生成，绝不写目标。
#[cfg(test)]
fn prepare_restore_plan(
    backup_dir: &Path,
    restore_candidates: &[(String, PathBuf)],
) -> Result<RestorePlan, String> {
    let mut contents = BTreeMap::new();
    for (name, _) in restore_candidates {
        if let Some(text) =
            read_optional_regular_text(&backup_dir.join(name), &format!("备份 {}", name))?
        {
            contents.insert(name.clone(), text);
        }
    }
    prepare_restore_plan_from_contents(&contents, restore_candidates)
}

fn prepare_restore_plan_from_contents(
    backup_contents: &BTreeMap<String, String>,
    restore_candidates: &[(String, PathBuf)],
) -> Result<RestorePlan, String> {
    let mut plan = RestorePlan::default();
    for (name, dest) in restore_candidates {
        let backup_text = backup_contents.get(name).cloned();
        let original = snapshot_target(dest, name)?;
        let current = original.as_ref().map(|snapshot| snapshot.content.as_str());

        let (desired, report) = match name.as_str() {
            "settings.json" => {
                let generated = generate_claude_settings_content(backup_text.as_deref(), current)?;
                match generated {
                    Some(content) => (
                        DesiredTarget::Present {
                            content,
                            private: false,
                        },
                        "restored:settings.json managed env exact".to_string(),
                    ),
                    None => (
                        DesiredTarget::Absent,
                        "removed:settings.json(no managed/current content)".to_string(),
                    ),
                }
            }
            "config.toml" => {
                let generated = generate_codex_toml_content(backup_text.as_deref(), current)?;
                match generated {
                    Some(content) => (
                        DesiredTarget::Present {
                            content,
                            private: false,
                        },
                        "restored:config.toml managed fields exact".to_string(),
                    ),
                    None => (
                        DesiredTarget::Absent,
                        "removed:config.toml(no managed/current content)".to_string(),
                    ),
                }
            }
            _ => match backup_text {
                Some(content) => {
                    validate_private_backup(name, &content)?;
                    (
                        DesiredTarget::Present {
                            content,
                            private: name == "secrets.env",
                        },
                        format!("restored:{}", name),
                    )
                }
                None => (DesiredTarget::Absent, format!("removed:{}", name)),
            },
        };

        if desired_differs(&desired, &original) {
            plan.operations.push(RestoreOperation {
                name: name.clone(),
                path: dest.clone(),
                original,
                desired,
                report,
            });
        }
    }
    Ok(plan)
}

fn remove_regular_target_if_exists(path: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_file() || meta.file_type().is_symlink() => {
            std::fs::remove_file(path).map_err(|e| format!("删除 {} 失败: {}", path.display(), e))
        }
        Ok(_) => Err(format!("拒绝删除非普通文件: {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("检查 {} 失败: {}", path.display(), e)),
    }
}

fn apply_restore_operation(op: &RestoreOperation) -> Result<(), String> {
    match &op.desired {
        DesiredTarget::Present { content, private } => {
            atomic_write_mode(&op.path, content, *private)
                .map_err(|e| format!("原子恢复 {} 失败: {}", op.name, e))
        }
        DesiredTarget::Absent => remove_regular_target_if_exists(&op.path),
    }
}

fn restore_original(op: &RestoreOperation) -> Result<(), String> {
    match &op.original {
        Some(snapshot) => {
            let private = op.name == "secrets.env";
            atomic_write_mode(&op.path, &snapshot.content, private)
                .map_err(|e| format!("原子回滚 {} 失败: {}", op.name, e))?;
            #[cfg(unix)]
            if !private {
                if let Some(mode) = snapshot.mode {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(&op.path, std::fs::Permissions::from_mode(mode))
                        .map_err(|e| format!("回滚 {} 权限失败: {}", op.name, e))?;
                }
            }
            Ok(())
        }
        None => remove_regular_target_if_exists(&op.path),
    }
}

fn rollback_touched(touched: &[&RestoreOperation]) -> Vec<String> {
    let mut errors = Vec::new();
    for op in touched.iter().rev() {
        if let Err(e) = restore_original(op) {
            errors.push(e);
        }
    }
    errors
}

/// 执行事务 plan。`fail_after_writes` 仅用于回归测试注入中途失败。
fn execute_restore_plan(
    plan: &RestorePlan,
    fail_after_writes: Option<usize>,
) -> Result<Vec<String>, String> {
    let mut touched: Vec<&RestoreOperation> = Vec::new();
    let mut written = Vec::new();

    let execution = (|| -> Result<(), String> {
        if fail_after_writes == Some(0) {
            return Err("测试注入：在写入前失败".into());
        }
        for op in &plan.operations {
            // preflight 与实际写之间若被外部进程改动，立即中止并回滚
            // 前面已写目标，避免覆盖用户的并发修改。
            let now = snapshot_target(&op.path, &op.name)?;
            if now != op.original {
                return Err(format!(
                    "{} 在恢复 preflight 后被其他进程修改，已中止",
                    op.name
                ));
            }
            // 先记为 touched：atomic_write_mode 在 rename 后设权限仍可能失败，
            // 此时返回 Err 也必须把当前目标回滚。
            touched.push(op);
            apply_restore_operation(op)?;
            written.push(op.report.clone());
            if fail_after_writes == Some(touched.len()) {
                return Err(format!("测试注入：在已写 {} 项后失败", touched.len()));
            }
        }
        Ok(())
    })();

    if let Err(cause) = execution {
        let rollback_errors = rollback_touched(&touched);
        if rollback_errors.is_empty() {
            return Err(format!(
                "{}；已回滚全部 {} 个已触碰目标",
                cause,
                touched.len()
            ));
        }
        return Err(format!(
            "{}；已尝试回滚全部目标，但以下回滚失败: {}",
            cause,
            rollback_errors.join(" | ")
        ));
    }
    Ok(written)
}

pub fn delete_backup_sync(backup_id: String) -> Result<BackupActionResult, String> {
    let id = backup_id.trim();
    validate_backup_id(id)?;
    let root = backups_root()?;
    let dir = root.join(id);
    // API 不能仅凭一个格式正确的目录名就删除其中普通文件。
    // 先用与 list/restore 一致的 manifest/权限/哈希边界证明它是
    // 完整的本工具备份；删除 preflight 还会再拒绝新增未知条目。
    validate_backup_directory(&root, id)
        .map_err(|error| format!("备份不可删除（完整性校验失败）: {error}"))?;
    remove_backup_dir_shallow(&root, &dir, &format!("备份 {}", id))?;
    Ok(BackupActionResult {
        ok: true,
        message: format!("已删除备份 {}", id),
        id: Some(id.to_string()),
        written: vec![],
    })
}

#[tauri::command]
pub async fn create_backup(note: Option<String>) -> Result<BackupActionResult, String> {
    super::util::spawn_blocking_result(move || with_op_lock(|| create_backup_sync(note))).await
}

#[tauri::command]
pub async fn list_backups() -> Result<BackupListResult, String> {
    super::util::spawn_blocking_result(list_backups_sync).await
}

#[tauri::command]
pub async fn restore_backup(backup_id: String) -> Result<BackupActionResult, String> {
    super::util::spawn_blocking_result(move || with_op_lock(|| restore_backup_sync(backup_id)))
        .await
}

#[tauri::command]
pub async fn delete_backup(backup_id: String) -> Result<BackupActionResult, String> {
    super::util::spawn_blocking_result(move || with_op_lock(|| delete_backup_sync(backup_id))).await
}

#[tauri::command]
pub async fn open_backups_folder() -> Result<String, String> {
    super::util::spawn_blocking_result(|| {
        let root = backups_root()?;
        let s = root.display().to_string();
        let status = if cfg!(target_os = "macos") {
            std::process::Command::new("open").arg(&s).status()
        } else if cfg!(target_os = "windows") {
            std::process::Command::new("explorer").arg(&s).status()
        } else {
            std::process::Command::new("xdg-open").arg(&s).status()
        }
        .map_err(|e| format!("打开备份目录失败: {}", e))?;
        if !status.success() {
            return Err(format!("打开备份目录失败，退出码: {:?}", status.code()));
        }
        Ok(s)
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "codecli-backup-test-{}-{}-{}",
                label,
                std::process::id(),
                stamp_id()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self { path }
        }

        fn join(&self, path: &str) -> PathBuf {
            self.path.join(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn generators_restore_managed_presence_exactly_and_preserve_unmanaged() {
        let current_settings = r#"{
  "hooks": {"keep": true},
  "env": {
    "MY_CUSTOM": "keep-current",
    "ANTHROPIC_BASE_URL": "https://old.example",
    "ANTHROPIC_API_KEY": "old-key",
    "ANTHROPIC_AUTH_TOKEN": "old-token",
    "ANTHROPIC_MODEL": "old-model"
  }
}"#;
        let backup_settings = r#"{
  "hooks": {"do_not_restore": true},
  "env": {
    "MY_BACKUP_ONLY": "ignore",
    "ANTHROPIC_API_KEY": "backup-key"
  }
}"#;
        let generated =
            generate_claude_settings_content(Some(backup_settings), Some(current_settings))
                .unwrap()
                .unwrap();
        let settings: serde_json::Value = serde_json::from_str(&generated).unwrap();
        assert_eq!(settings["hooks"]["keep"].as_bool(), Some(true));
        assert!(settings["hooks"].get("do_not_restore").is_none());
        assert_eq!(settings["env"]["MY_CUSTOM"].as_str(), Some("keep-current"));
        assert!(settings["env"].get("MY_BACKUP_ONLY").is_none());
        assert_eq!(
            settings["env"]["ANTHROPIC_API_KEY"].as_str(),
            Some("backup-key")
        );
        for missing in [
            "ANTHROPIC_BASE_URL",
            "ANTHROPIC_AUTH_TOKEN",
            "ANTHROPIC_MODEL",
        ] {
            assert!(
                settings["env"].get(missing).is_none(),
                "backup 缺失的受管 key 必须删除: {missing}"
            );
        }

        let current_toml = r#"model = "old-model"
model_provider = "codecli_installer"

[model_providers.codecli_installer]
name = "Old Installer"
stale = "remove-me"

[model_providers.user_keep]
name = "User Keep"
base_url = "https://user.example/v1"

[mcp_servers.keep]
command = "keep-mcp"
"#;
        let backup_without_provider = r#"model = "backup-model"

[mcp_servers.backup_only]
command = "do-not-copy"
"#;
        let generated =
            generate_codex_toml_content(Some(backup_without_provider), Some(current_toml))
                .unwrap()
                .unwrap();
        let doc: toml_edit::DocumentMut = generated.parse().unwrap();
        assert_eq!(
            doc.get("model").and_then(|i| i.as_str()),
            Some("backup-model")
        );
        assert!(doc.get("model_provider").is_none());
        assert!(codex_provider_item(&doc).is_none());
        assert!(doc["model_providers"]["user_keep"].is_table());
        assert_eq!(
            doc["mcp_servers"]["keep"]["command"].as_str(),
            Some("keep-mcp")
        );
        assert!(doc["mcp_servers"].get("backup_only").is_none());

        // 备份 provider 存在时应整个 Item 替换，包括非预设的自定义字段。
        let backup_with_provider = r#"model_provider = "codecli_installer"

[model_providers.codecli_installer]
name = "Backup Installer"
base_url = "https://backup.example/v1"
custom_flag = "preserve-exactly"
"#;
        let generated = generate_codex_toml_content(Some(backup_with_provider), Some(current_toml))
            .unwrap()
            .unwrap();
        let doc: toml_edit::DocumentMut = generated.parse().unwrap();
        let provider = codex_provider_item(&doc).unwrap();
        let provider = provider.as_table().unwrap();
        assert_eq!(provider["name"].as_str(), Some("Backup Installer"));
        assert_eq!(provider["custom_flag"].as_str(), Some("preserve-exactly"));
        assert!(provider.get("stale").is_none());
        assert!(doc["model_providers"]["user_keep"].is_table());
        assert!(doc.get("model").is_none());

        // 当前 model_providers 是 inline table 时也不得丢掉其他 provider。
        let inline_current = r#"model_providers = { user_keep = { name = "Inline User" }, codecli_installer = { stale = "remove" } }
"#;
        let generated =
            generate_codex_toml_content(Some(backup_with_provider), Some(inline_current))
                .unwrap()
                .unwrap();
        let doc: toml_edit::DocumentMut = generated.parse().unwrap();
        assert_eq!(
            doc["model_providers"]["user_keep"]["name"].as_str(),
            Some("Inline User")
        );
        let provider = codex_provider_item(&doc).unwrap();
        assert_eq!(
            provider.as_table().unwrap()["custom_flag"].as_str(),
            Some("preserve-exactly")
        );
    }

    #[test]
    fn preflight_generates_every_target_before_any_write() {
        let temp = TempDir::new("preflight");
        let backup = temp.join("backup");
        let current = temp.join("current");
        std::fs::create_dir_all(&backup).unwrap();
        std::fs::create_dir_all(&current).unwrap();

        std::fs::write(backup.join("ownership.json"), r#"{"new":true}"#).unwrap();
        std::fs::write(backup.join("config.toml"), "not = [valid toml").unwrap();
        let ownership = current.join("ownership.json");
        let config = current.join("config.toml");
        std::fs::write(&ownership, r#"{"old":true}"#).unwrap();
        std::fs::write(&config, "model = \"old\"\n").unwrap();

        let candidates = vec![
            ("ownership.json".into(), ownership.clone()),
            ("config.toml".into(), config.clone()),
        ];
        let err = prepare_restore_plan(&backup, &candidates).unwrap_err();
        assert!(err.contains("备份 config.toml"), "err={err}");
        assert_eq!(
            std::fs::read_to_string(&ownership).unwrap(),
            r#"{"old":true}"#,
            "后续目标生成失败前不得先写前面的目标"
        );
        assert_eq!(
            std::fs::read_to_string(&config).unwrap(),
            "model = \"old\"\n"
        );
    }

    #[test]
    fn execution_failure_rolls_back_every_touched_target() {
        let temp = TempDir::new("rollback");
        let backup = temp.join("backup");
        let current = temp.join("current");
        std::fs::create_dir_all(&backup).unwrap();
        std::fs::create_dir_all(&current).unwrap();

        std::fs::write(backup.join("ownership.json"), r#"{"state":"new-1"}"#).unwrap();
        std::fs::write(backup.join("schemes.json"), r#"{"state":"new-2"}"#).unwrap();
        let first = current.join("ownership.json");
        let second = current.join("schemes.json");
        std::fs::write(&first, r#"{"state":"old-1"}"#).unwrap();
        std::fs::write(&second, r#"{"state":"old-2"}"#).unwrap();

        let candidates = vec![
            ("ownership.json".into(), first.clone()),
            ("schemes.json".into(), second.clone()),
        ];
        let plan = prepare_restore_plan(&backup, &candidates).unwrap();
        assert_eq!(plan.operations.len(), 2);
        let err = execute_restore_plan(&plan, Some(1)).unwrap_err();
        assert!(err.contains("已回滚全部"), "err={err}");
        assert_eq!(
            std::fs::read_to_string(&first).unwrap(),
            r#"{"state":"old-1"}"#
        );
        assert_eq!(
            std::fs::read_to_string(&second).unwrap(),
            r#"{"state":"old-2"}"#
        );
    }

    #[test]
    fn missing_private_file_deletes_current_and_secrets_stay_private() {
        let temp = TempDir::new("missing-private");
        let backup = temp.join("backup");
        let current = temp.join("current");
        std::fs::create_dir_all(&backup).unwrap();
        std::fs::create_dir_all(&current).unwrap();

        // ownership 在备份中缺失；secrets 存在。
        std::fs::write(backup.join("secrets.env"), "API_KEY='new-secret'\n").unwrap();
        let ownership = current.join("ownership.json");
        let secrets = current.join("secrets.env");
        std::fs::write(&ownership, r#"{"current":true}"#).unwrap();
        std::fs::write(&secrets, "API_KEY='old-secret'\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&secrets, std::fs::Permissions::from_mode(0o644)).unwrap();
        }

        let candidates = vec![
            ("ownership.json".into(), ownership.clone()),
            ("secrets.env".into(), secrets.clone()),
        ];
        let plan = prepare_restore_plan(&backup, &candidates).unwrap();
        let written = execute_restore_plan(&plan, None).unwrap();
        assert!(written.iter().any(|s| s == "removed:ownership.json"));
        assert!(!ownership.exists(), "备份缺失的私有文件必须删除当前文件");
        assert_eq!(
            std::fs::read_to_string(&secrets).unwrap(),
            "API_KEY='new-secret'\n"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&secrets).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "secrets.env 恢复后必须是 0600");
        }
    }

    #[cfg(unix)]
    #[test]
    fn backup_source_symlink_is_rejected_during_preflight() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new("symlink");
        let backup = temp.join("backup");
        std::fs::create_dir_all(&backup).unwrap();
        let real = temp.join("real.json");
        std::fs::write(&real, r#"{"safe":false}"#).unwrap();
        symlink(&real, backup.join("ownership.json")).unwrap();
        let target = temp.join("ownership.json");
        let candidates = vec![("ownership.json".into(), target)];

        let err = prepare_restore_plan(&backup, &candidates).unwrap_err();
        assert!(err.contains("符号链接"), "err={err}");
    }

    #[test]
    fn atomic_publish_writes_complete_manifest_and_empty_snapshot_is_valid() {
        let temp = TempDir::new("atomic-manifest");
        let root = temp.join("backups");
        let source = temp.join("ownership.json");
        std::fs::write(&source, r#"{"owned":true}"#).unwrap();

        let id = "bak_1700000000_000000001";
        publish_backup_at_root(
            &root,
            id,
            "test".into(),
            &[("ownership.json".into(), source)],
        )
        .unwrap();
        let validated = validate_backup_directory(&root, id).unwrap();
        assert_eq!(
            validated.contents.get("ownership.json").map(String::as_str),
            Some(r#"{"owned":true}"#)
        );
        let manifest: BackupManifest = serde_json::from_slice(
            &read_required_regular_bytes_nofollow(
                &root.join(id).join(BACKUP_MANIFEST_FILE),
                "manifest",
                MAX_BACKUP_META_BYTES,
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(manifest.version, BACKUP_MANIFEST_VERSION);
        assert!(manifest.complete);
        assert!(manifest.absent.contains(&"schemes.json".to_string()));

        let empty_id = "bak_1700000000_000000002";
        publish_backup_at_root(&root, empty_id, "empty".into(), &[]).unwrap();
        let empty = validate_backup_directory(&root, empty_id).unwrap();
        assert!(empty.contents.is_empty());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&root).unwrap().permissions().mode() & 0o777,
                0o700
            );
            for dir in [root.join(id), root.join(empty_id)] {
                assert_eq!(
                    std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
                    0o700
                );
                for entry in std::fs::read_dir(dir).unwrap() {
                    let entry = entry.unwrap();
                    assert_eq!(
                        entry.metadata().unwrap().permissions().mode() & 0o777,
                        0o600,
                        "{}",
                        entry.path().display()
                    );
                }
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn create_rejects_symlink_source_and_cleans_staging() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new("create-symlink");
        let root = temp.join("backups");
        let real = temp.join("real.json");
        let source = temp.join("ownership.json");
        std::fs::write(&real, r#"{"outside":true}"#).unwrap();
        symlink(&real, &source).unwrap();
        let id = "bak_1700000000_000000003";
        let err = publish_backup_at_root(
            &root,
            id,
            "test".into(),
            &[("ownership.json".into(), source)],
        )
        .unwrap_err();
        assert!(err.contains("符号链接"), "err={err}");
        assert!(!root.join(id).exists());
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);
    }

    #[test]
    fn list_and_validator_reject_partial_staging_unknown_and_tampered_backup() {
        let temp = TempDir::new("invalid-backups");
        let root = temp.join("backups");
        let source = temp.join("ownership.json");
        std::fs::write(&source, r#"{"ok":true}"#).unwrap();
        let good_id = "bak_1700000000_000000004";
        publish_backup_at_root(
            &root,
            good_id,
            "good".into(),
            &[("ownership.json".into(), source)],
        )
        .unwrap();

        let staging = root.join(".staging-bak_1700000000_000000099-test");
        create_private_dir(&staging, false).unwrap();
        ensure_private_directory(&staging, "staging").unwrap();
        let partial_id = "bak_1700000000_000000005";
        let partial = root.join(partial_id);
        create_private_dir(&partial, false).unwrap();
        ensure_private_directory(&partial, "partial").unwrap();
        let (items, rejected) = list_backups_at_root(&root).unwrap();
        assert_eq!(items.len(), 1, "staging 必须隐藏");
        assert_eq!(rejected, 1, "partial 必须拒绝");
        assert!(validate_backup_directory(&root, partial_id)
            .unwrap_err()
            .contains("manifest"));

        write_new_private_file(
            &root.join(good_id).join("unknown.txt"),
            b"unexpected",
            "unknown",
        )
        .unwrap();
        assert!(validate_backup_directory(&root, good_id)
            .unwrap_err()
            .contains("manifest"));
        let delete_error =
            remove_backup_dir_shallow(&root, &root.join(good_id), "含外部条目的备份").unwrap_err();
        assert!(delete_error.contains("未知条目"), "err={delete_error}");
        assert!(root.join(good_id).join("unknown.txt").exists());
        assert!(root.join(good_id).join("ownership.json").exists());
        std::fs::remove_file(root.join(good_id).join("unknown.txt")).unwrap();
        std::fs::write(root.join(good_id).join("ownership.json"), r#"{"ok":null}"#).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                root.join(good_id).join("ownership.json"),
                std::fs::Permissions::from_mode(0o600),
            )
            .unwrap();
        }
        assert!(validate_backup_directory(&root, good_id)
            .unwrap_err()
            .contains("sha256"));
    }

    #[cfg(unix)]
    #[test]
    fn prune_and_delete_reject_symlink_backup_and_propagate_error() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new("prune-symlink");
        let root = temp.join("backups");
        create_private_dir(&root, true).unwrap();
        ensure_private_directory(&root, "root").unwrap();
        let outside = temp.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        let id = "bak_1700000000_000000006";
        symlink(&outside, root.join(id)).unwrap();

        let prune_err = prune_old_backups_at_root(&root, 0, None).unwrap_err();
        assert!(prune_err.contains("符号链接"), "err={prune_err}");
        let delete_err =
            remove_backup_dir_shallow(&root, &root.join(id), "symlink backup").unwrap_err();
        assert!(delete_err.contains("符号链接"), "err={delete_err}");
        assert!(outside.exists());
    }
}
