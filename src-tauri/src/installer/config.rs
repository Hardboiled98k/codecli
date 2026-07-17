// SPDX-License-Identifier: MPL-2.0
use std::io::Read;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use toml_edit::{DocumentMut, Item, Table, Value as TomlValue};

use super::op_lock::with_new_operation;
use super::platform::{
    claude_config_dir, codex_config_dir, codex_config_toml, get_persistent_env_strict,
    set_user_env, unset_user_env,
};
use super::providers::find_provider;
use super::util::{
    atomic_write, atomic_write_mode, chrono_like_now, mask_key, validate_base_url,
    validate_env_value,
};

const CLAUDE_ENV_KEYS: &[&str] = &[
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_MODEL",
];
const CODEX_ENV_KEYS: &[&str] = &["OPENAI_API_KEY", "OPENAI_BASE_URL"];
/// 固定 provider id，避免覆盖用户自建表
const CODEX_PROVIDER_ID: &str = "codecli_installer";

fn unique_stamp() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let d = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}_{:09}", d.as_secs(), d.subsec_nanos())
}

const BASELINE_ABSENT: &str = "__ABSENT__";
const MAX_CONFIG_BACKUP_BYTES: u64 = 16 * 1024 * 1024;

#[cfg(test)]
std::thread_local! {
    static TEST_FAIL_OWNERSHIP_SAVE_AFTER: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
    static TEST_FAIL_REMOVE_PATH: std::cell::RefCell<Option<std::path::PathBuf>> = const { std::cell::RefCell::new(None) };
}

fn remove_path_if_exists(path: &std::path::Path) -> Result<(), String> {
    #[cfg(test)]
    if TEST_FAIL_REMOVE_PATH.with(|slot| slot.borrow().as_deref() == Some(path)) {
        return Err(format!("注入的删除失败: {}", path.display()));
    }
    super::util::remove_file_durable(path)
        .map_err(|error| format!("持久删除 {} 失败: {error}", path.display()))
}

fn read_trusted_backup(path: &std::path::Path, label: &str) -> Result<String, String> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("检查 {label} 失败 {}: {error}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!("{label} 不是可信普通文件: {}", path.display()));
    }
    if metadata.len() > MAX_CONFIG_BACKUP_BYTES {
        return Err(format!("{label} 超过 16 MiB，已拒绝读取"));
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
    let mut file = options
        .open(path)
        .map_err(|error| format!("安全打开 {label} 失败: {error}"))?;
    let opened = file
        .metadata()
        .map_err(|error| format!("读取 {label} 元数据失败: {error}"))?;
    if !opened.is_file() || opened.len() > MAX_CONFIG_BACKUP_BYTES {
        return Err(format!("{label} 打开后不是可信普通小文件"));
    }
    let mut bytes = Vec::with_capacity(opened.len() as usize);
    std::io::Read::by_ref(&mut file)
        .take(MAX_CONFIG_BACKUP_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("读取 {label} 失败: {error}"))?;
    if bytes.len() as u64 > MAX_CONFIG_BACKUP_BYTES {
        return Err(format!("{label} 读取期间变大，已拒绝"));
    }
    String::from_utf8(bytes).map_err(|error| format!("{label} 不是 UTF-8: {error}"))
}

fn restore_text_backup_atomic(
    backup: &std::path::Path,
    destination: &std::path::Path,
    label: &str,
) -> Result<(), String> {
    let content = read_trusted_backup(backup, label)?;
    atomic_write(destination, &content).map_err(|error| format!("原子恢复 {label} 失败: {error}"))
}

pub(crate) fn reject_top_level_config_link(
    path: &std::path::Path,
    label: &str,
) -> Result<(), String> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(format!(
            "{label} 是顶层符号链接；为避免断开 dotfiles 链接，当前版本已拒绝自动修改"
        )),
        Ok(metadata) if !metadata.is_file() => Err(format!("{label} 不是可信普通文件，已拒绝修改")),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("检查 {label} 失败: {error}")),
    }
}

/// 配置备份可能含旧 API Key，创建瞬间就必须是私有文件；不能先
/// `fs::copy` 成继承源文件的 0644，再事后 chmod。
fn copy_private_text_backup(
    source: &std::path::Path,
    destination: &std::path::Path,
    label: &str,
) -> Result<(), String> {
    let raw = std::fs::read_to_string(source)
        .map_err(|error| format!("读取 {label} 备份源失败: {error}"))?;
    atomic_write_mode(destination, &raw, true)
        .map_err(|error| format!("创建私有 {label} 备份失败: {error}"))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigApplyRequest {
    pub provider_id: String,
    pub api_key: String,
    pub base_url: Option<String>,
    pub model: Option<String>,
    /// claude | codex（v1 不支持 both）
    pub target: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigApplyResult {
    pub ok: bool,
    pub message: String,
    pub written: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct OwnershipRecord {
    /// key -> 原值；None 表示原先不存在
    env_prev: std::collections::BTreeMap<String, Option<String>>,
    /// Claude settings 里我们写入前，键各自的原值
    settings_env_prev: std::collections::BTreeMap<String, Option<String>>,
    /// Codex 根级 model / model_provider 原值
    codex_root_prev: std::collections::BTreeMap<String, Option<String>>,
    /// 我们是否创建了 provider 表（原先不存在）
    codex_provider_created: bool,
    /// 若原先已有同名 provider，整表 TOML 快照（用于恢复）
    codex_provider_prev_toml: Option<String>,
    /// 首次应用前的基线备份（clear 恢复到用户原状）
    settings_baseline_bak: Option<String>,
    codex_baseline_bak: Option<String>,
    /// 最近一次事务回滚备份（失败回滚用，可被覆盖）
    settings_tx_bak: Option<String>,
    codex_tx_bak: Option<String>,
    /// 已由 ownership 接管、但不再是当前 baseline/tx 的备份。
    ///
    /// 新备份会先在这里持久化登记，再创建文件；旧 tx 会先转入
    /// 这里，再删除。因此即使进程崩溃或删除失败，含旧 Key 的备份
    /// 也始终有 durable ownership 记录，下次操作可继续清理。
    #[serde(default)]
    pending_backup_cleanup: Vec<String>,
    updated_at: String,
}

fn home_state_dir() -> Option<std::path::PathBuf> {
    claude_config_dir().map(|d| d.join("codecli-installer"))
}

fn validated_state_dir() -> Result<Option<std::path::PathBuf>, String> {
    let Some(dir) = home_state_dir() else {
        return Ok(None);
    };
    if dir.file_name().and_then(|v| v.to_str()) != Some("codecli-installer") {
        return Err("状态目录校验失败".into());
    }
    match std::fs::symlink_metadata(&dir) {
        Ok(meta) if meta.file_type().is_symlink() => {
            Err("本工具状态目录是符号链接，已拒绝操作".into())
        }
        Ok(meta) if !meta.is_dir() => Err("本工具状态路径不是目录".into()),
        Ok(_) => Ok(Some(dir)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("检查状态目录失败: {e}")),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum StateDirIdentity {
    #[cfg(unix)]
    Unix { device: u64, inode: u64 },
    #[cfg(windows)]
    Windows { volume_serial: u32, file_index: u64 },
    #[cfg(not(any(unix, windows)))]
    Portable(std::path::PathBuf),
}

const STATE_PURGE_MARKER_VERSION: u8 = 1;
const MAX_STATE_PURGE_MARKER_BYTES: u64 = 16 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum StateDirPurgePhase {
    Prepared,
    Quarantined,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StateDirPurgeMarker {
    version: u8,
    phase: StateDirPurgePhase,
    quarantine_name: String,
    expected_identity: StateDirIdentity,
}

fn state_dir_identity(path: &std::path::Path) -> Result<StateDirIdentity, String> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("读取状态目录身份失败: {error}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err("状态目录身份检查发现路径不再是可信目录".into());
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(StateDirIdentity::Unix {
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
    #[cfg(windows)]
    {
        state_dir_identity_windows(path)
    }
    #[cfg(not(any(unix, windows)))]
    {
        std::fs::canonicalize(path)
            .map(StateDirIdentity::Portable)
            .map_err(|error| format!("解析状态目录身份失败: {error}"))
    }
}

#[cfg(windows)]
fn state_dir_identity_windows(path: &std::path::Path) -> Result<StateDirIdentity, String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, OPEN_EXISTING,
    };

    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    if wide[..wide.len() - 1].contains(&0) {
        return Err("状态目录路径包含 NUL".into());
    }
    // SAFETY: wide 在调用期间存活且以 NUL 结尾；security/template handle
    // 为空。句柄无论后续成功与否都会在本函数关闭。
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(format!(
            "打开状态目录身份句柄失败: {}",
            std::io::Error::last_os_error()
        ));
    }
    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: handle 有效，information 是可写且大小正确的输出结构。
    let ok = unsafe { GetFileInformationByHandle(handle, &mut information) };
    let information_error = (ok == 0).then(std::io::Error::last_os_error);
    // SAFETY: handle 来自 CreateFileW，且只关闭一次。
    let close_ok = unsafe { CloseHandle(handle) };
    if let Some(error) = information_error {
        return Err(format!("读取状态目录 file-id 失败: {error}"));
    }
    if close_ok == 0 {
        return Err(format!(
            "关闭状态目录身份句柄失败: {}",
            std::io::Error::last_os_error()
        ));
    }
    if information.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0
        || information.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
    {
        return Err("Windows 状态路径不是可信实体目录".into());
    }
    Ok(StateDirIdentity::Windows {
        volume_serial: information.dwVolumeSerialNumber,
        file_index: ((information.nFileIndexHigh as u64) << 32) | information.nFileIndexLow as u64,
    })
}

fn state_dir_purge_marker_path() -> Result<std::path::PathBuf, String> {
    let state = home_state_dir().ok_or("找不到状态目录路径")?;
    let parent = state.parent().ok_or("状态目录没有父目录")?;
    Ok(parent.join(".codecli-installer.purge.json"))
}

fn validate_state_dir_purge_marker(marker: &StateDirPurgeMarker) -> Result<(), String> {
    if marker.version != STATE_PURGE_MARKER_VERSION {
        return Err(format!(
            "不支持的状态目录 purge marker 版本 {}",
            marker.version
        ));
    }
    let name = marker.quarantine_name.as_str();
    if !name.starts_with(".codecli-installer.purge-")
        || name.len() > 180
        || !name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_'))
    {
        return Err("状态目录 purge marker 含越界隔离路径".into());
    }
    Ok(())
}

fn load_state_dir_purge_marker() -> Result<Option<StateDirPurgeMarker>, String> {
    let path = state_dir_purge_marker_path()?;
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("检查状态目录 purge marker 失败: {error}")),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err("状态目录 purge marker 不是可信普通文件".into());
    }
    if metadata.len() > MAX_STATE_PURGE_MARKER_BYTES {
        return Err("状态目录 purge marker 过大".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o777 != 0o600 {
            return Err("状态目录 purge marker 权限不是 0600".into());
        }
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
    let mut file = options
        .open(&path)
        .map_err(|error| format!("安全打开状态目录 purge marker 失败: {error}"))?;
    let opened = file
        .metadata()
        .map_err(|error| format!("读取状态目录 purge marker 元数据失败: {error}"))?;
    if !opened.is_file() || opened.len() > MAX_STATE_PURGE_MARKER_BYTES {
        return Err("状态目录 purge marker 打开后不是可信普通小文件".into());
    }
    let mut bytes = Vec::with_capacity(opened.len() as usize);
    std::io::Read::by_ref(&mut file)
        .take(MAX_STATE_PURGE_MARKER_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("读取状态目录 purge marker 失败: {error}"))?;
    if bytes.len() as u64 > MAX_STATE_PURGE_MARKER_BYTES {
        return Err("状态目录 purge marker 读取期间变大".into());
    }
    let marker: StateDirPurgeMarker = serde_json::from_slice(&bytes)
        .map_err(|error| format!("状态目录 purge marker 损坏: {error}"))?;
    validate_state_dir_purge_marker(&marker)?;
    Ok(Some(marker))
}

fn save_state_dir_purge_marker(marker: &StateDirPurgeMarker) -> Result<(), String> {
    validate_state_dir_purge_marker(marker)?;
    let path = state_dir_purge_marker_path()?;
    let body = serde_json::to_string_pretty(marker).map_err(|error| error.to_string())?;
    if body.len() as u64 > MAX_STATE_PURGE_MARKER_BYTES {
        return Err("状态目录 purge marker 序列化后过大".into());
    }
    atomic_write_mode(&path, &body, true)
}

fn remove_state_dir_purge_marker() -> Result<(), String> {
    let path = state_dir_purge_marker_path()?;
    super::util::remove_file_durable(&path)
        .map_err(|error| format!("持久删除状态目录 purge marker 失败: {error}"))
}

fn state_dir_quarantine_path(marker: &StateDirPurgeMarker) -> Result<std::path::PathBuf, String> {
    validate_state_dir_purge_marker(marker)?;
    let state = home_state_dir().ok_or("找不到状态目录路径")?;
    let parent = state.parent().ok_or("状态目录没有父目录")?;
    Ok(parent.join(&marker.quarantine_name))
}

fn finish_state_dir_purge(mut marker: StateDirPurgeMarker) -> Result<(), String> {
    let original = home_state_dir().ok_or("找不到状态目录路径")?;
    let quarantine = state_dir_quarantine_path(&marker)?;

    if marker.phase == StateDirPurgePhase::Prepared {
        match std::fs::symlink_metadata(&quarantine) {
            Ok(_) => {
                if state_dir_identity(&quarantine)? != marker.expected_identity {
                    return Err("卸载隔离目录 file-id 与 marker 不匹配，已拒绝删除".into());
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if state_dir_identity(&original)? != marker.expected_identity {
                    return Err("状态目录在 purge 恢复期间被同路径替换，已拒绝删除".into());
                }
                std::fs::rename(&original, &quarantine)
                    .map_err(|rename_error| format!("原子隔离状态目录失败: {rename_error}"))?;
                super::util::sync_parent_dir(&quarantine)
                    .map_err(|sync_error| format!("同步状态目录隔离 rename 失败: {sync_error}"))?;
                if state_dir_identity(&quarantine)? != marker.expected_identity {
                    return Err("隔离后状态目录 file-id 改变，已拒绝删除".into());
                }
            }
            Err(error) => return Err(format!("检查卸载隔离目录失败: {error}")),
        }
        marker.phase = StateDirPurgePhase::Quarantined;
        save_state_dir_purge_marker(&marker)?;
    }

    match std::fs::symlink_metadata(&quarantine) {
        Ok(_) => {
            if state_dir_identity(&quarantine)? != marker.expected_identity {
                return Err("卸载隔离目录在递归删除前被替换，已拒绝删除".into());
            }
            std::fs::remove_dir_all(&quarantine)
                .map_err(|error| format!("删除本工具隔离状态目录失败: {error}"))?;
            super::util::sync_parent_dir(&quarantine)
                .map_err(|error| format!("状态目录已删除，但同步父目录失败: {error}"))?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("复查卸载隔离目录失败: {error}")),
    }
    remove_state_dir_purge_marker()
}

pub(crate) fn recover_pending_state_dir_purge() -> Result<(), String> {
    let Some(marker) = load_state_dir_purge_marker()? else {
        return Ok(());
    };
    finish_state_dir_purge(marker)
}

fn remove_state_dir_with_identity(
    dir: &std::path::Path,
    expected: &StateDirIdentity,
) -> Result<(), String> {
    if home_state_dir().as_deref() != Some(dir) {
        return Err("待删除状态目录不在固定工具路径".into());
    }
    if &state_dir_identity(dir)? != expected {
        return Err("状态目录在清理期间被同路径替换，已拒绝递归删除".into());
    }
    if load_state_dir_purge_marker()?.is_some() {
        return Err("已有未完成状态目录 purge marker，请先恢复".into());
    }
    let quarantine_name = format!(".codecli-installer.purge-{}", unique_stamp());
    let marker = StateDirPurgeMarker {
        version: STATE_PURGE_MARKER_VERSION,
        phase: StateDirPurgePhase::Prepared,
        quarantine_name,
        expected_identity: expected.clone(),
    };
    let quarantine = state_dir_quarantine_path(&marker)?;
    if std::fs::symlink_metadata(&quarantine).is_ok() {
        return Err("卸载隔离路径已存在，已拒绝覆盖".into());
    }
    save_state_dir_purge_marker(&marker)?;
    finish_state_dir_purge(marker)
}

fn load_ownership() -> Result<OwnershipRecord, String> {
    let Some(dir) = validated_state_dir()? else {
        return Ok(OwnershipRecord::default());
    };
    let path = dir.join("ownership.json");
    let meta = match std::fs::symlink_metadata(&path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(OwnershipRecord::default()),
        Err(e) => return Err(format!("读取 ownership 元数据失败: {e}")),
    };
    if meta.file_type().is_symlink() || !meta.is_file() {
        return Err("ownership.json 不是可信普通文件，已拒绝操作".into());
    }
    let raw = std::fs::read_to_string(&path)
        .map_err(|e| format!("读取 ownership.json 失败，已保留原状: {e}"))?;
    serde_json::from_str(&raw).map_err(|e| format!("ownership.json 损坏，已保留配置和备份: {e}"))
}

fn save_ownership(rec: &OwnershipRecord) -> Result<(), String> {
    #[cfg(test)]
    {
        let should_fail = TEST_FAIL_OWNERSHIP_SAVE_AFTER.with(|slot| match slot.get() {
            Some(0) => {
                slot.set(None);
                true
            }
            Some(remaining) => {
                slot.set(Some(remaining - 1));
                false
            }
            None => false,
        });
        if should_fail {
            return Err("注入的 ownership 保存失败".into());
        }
    }
    let dir = home_state_dir().ok_or("找不到状态目录")?;
    // 已存在时必须先拒绝符号链接，create_dir_all 本身会跟随链接。
    let _ = validated_state_dir()?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.join("ownership.json");
    let body = serde_json::to_string_pretty(rec).map_err(|e| e.to_string())?;
    atomic_write(&path, &body)
}

fn remember_env(rec: &mut OwnershipRecord, key: &str) -> Result<(), String> {
    if rec.env_prev.contains_key(key) {
        return Ok(());
    }
    rec.env_prev
        .insert(key.to_string(), get_persistent_env_strict(key)?);
    Ok(())
}

fn snapshot_env(
    keys: &[&str],
) -> Result<std::collections::BTreeMap<String, Option<String>>, String> {
    keys.iter()
        .map(|key| Ok(((*key).to_string(), get_persistent_env_strict(key)?)))
        .collect()
}

/// 恢复“本次操作开始前”的环境变量快照。
///
/// OwnershipRecord.env_prev 是首次安装前的长期基线，只能用于最终 clear；
/// 二次 apply 的事务回滚必须使用本次快照，否则会把持久环境回滚到首次安装前，
/// 与已恢复到上一次 apply 的 settings/TOML 产生分裂。
fn restore_env_snapshot(
    snapshot: &std::collections::BTreeMap<String, Option<String>>,
    keys: &[&str],
) -> Result<(), String> {
    let mut errors = Vec::new();
    for key in keys {
        let result = match snapshot.get(*key) {
            Some(Some(value)) => set_user_env(key, value),
            Some(None) => unset_user_env(key),
            None => continue,
        };
        if let Err(error) = result {
            errors.push(format!("{key}: {error}"));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("；"))
    }
}

fn validate_recorded_backup_path(
    raw: &str,
    expected_parent: &std::path::Path,
    expected_prefix: &str,
) -> Result<(), String> {
    if raw == BASELINE_ABSENT {
        return Ok(());
    }
    let path = std::path::Path::new(raw);
    if !path.is_absolute()
        || path.parent() != Some(expected_parent)
        || !path
            .file_name()
            .and_then(|v| v.to_str())
            .map(|name| name.starts_with(expected_prefix))
            .unwrap_or(false)
    {
        return Err(format!(
            "ownership 中的备份路径越界，已拒绝: {}",
            path.display()
        ));
    }
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() || !meta.is_file() => Err(format!(
            "ownership 中的备份不是可信普通文件: {}",
            path.display()
        )),
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("检查备份失败 {}: {e}", path.display())),
    }
}

fn validate_recorded_backup_paths(rec: &OwnershipRecord) -> Result<(), String> {
    if let Some(parent) = claude_config_dir() {
        for raw in [&rec.settings_baseline_bak, &rec.settings_tx_bak]
            .into_iter()
            .flatten()
        {
            validate_recorded_backup_path(raw, &parent, "settings.json.codecli-")?;
        }
    }
    if let Some(config) = codex_config_toml() {
        if let Some(parent) = config.parent() {
            for raw in [&rec.codex_baseline_bak, &rec.codex_tx_bak]
                .into_iter()
                .flatten()
            {
                validate_recorded_backup_path(raw, parent, "config.toml.codecli-")?;
            }
        }
    }
    for raw in &rec.pending_backup_cleanup {
        let path = std::path::Path::new(raw);
        let is_claude_backup = claude_config_dir().is_some_and(|parent| {
            path.parent() == Some(parent.as_path())
                && path
                    .file_name()
                    .and_then(|value| value.to_str())
                    .is_some_and(|name| name.starts_with("settings.json.codecli-"))
        });
        let is_codex_backup = codex_config_toml()
            .and_then(|config| config.parent().map(std::path::Path::to_path_buf))
            .is_some_and(|parent| {
                path.parent() == Some(parent.as_path())
                    && path
                        .file_name()
                        .and_then(|value| value.to_str())
                        .is_some_and(|name| name.starts_with("config.toml.codecli-"))
            });
        if !is_claude_backup && !is_codex_backup {
            return Err(format!(
                "ownership 中的待清理备份路径越界，已拒绝: {}",
                path.display()
            ));
        }
        let expected_parent = path.parent().ok_or("待清理备份缺少父目录")?;
        let expected_prefix = if is_claude_backup {
            "settings.json.codecli-"
        } else {
            "config.toml.codecli-"
        };
        validate_recorded_backup_path(raw, expected_parent, expected_prefix)?;
    }
    Ok(())
}

fn track_pending_backup(rec: &mut OwnershipRecord, path: &std::path::Path) {
    let raw = path.display().to_string();
    if !rec.pending_backup_cleanup.contains(&raw) {
        rec.pending_backup_cleanup.push(raw);
    }
}

fn untrack_pending_backup(rec: &mut OwnershipRecord, path: &std::path::Path) {
    let raw = path.display().to_string();
    rec.pending_backup_cleanup.retain(|value| value != &raw);
}

/// 清理上次崩溃/失败后仍由 ownership 跟踪的备份。
///
/// 必须先删文件、再从 ownership 移除路径。删除或最后的
/// ownership 保存失败都会向上返回；前者保留完整记录，
/// 后者保留「路径已记录但文件可能已不存在」的安全状态。
fn gc_pending_backups(rec: &mut OwnershipRecord) -> Result<(), String> {
    if rec.pending_backup_cleanup.is_empty() {
        return Ok(());
    }
    validate_recorded_backup_paths(rec)?;
    let pending = rec.pending_backup_cleanup.clone();
    for raw in &pending {
        remove_path_if_exists(std::path::Path::new(raw))
            .map_err(|error| format!("清理已跟踪备份失败 {raw}: {error}"))?;
    }
    rec.pending_backup_cleanup.clear();
    rec.updated_at = chrono_like_now();
    save_ownership(rec)
        .map_err(|error| format!("备份已删除，但 ownership 清理状态保存失败: {error}"))
}

/// 新 tx 已成为 durable active tx 且主配置操作成功后，
/// 才可删除旧 tx。旧 tx 在删除前必须先被 durable ownership
/// 记录到 pending_backup_cleanup，删除错误不能吞掉。
fn gc_old_tx(
    rec: &mut OwnershipRecord,
    prev: &Option<String>,
    new_path: &str,
) -> Result<(), String> {
    let Some(raw) = prev.as_deref() else {
        return Ok(());
    };
    if raw == BASELINE_ABSENT || raw == new_path {
        return Ok(());
    }
    let path = std::path::Path::new(raw);
    if !rec.pending_backup_cleanup.iter().any(|value| value == raw) {
        track_pending_backup(rec, path);
        rec.updated_at = chrono_like_now();
        save_ownership(rec).map_err(|error| {
            format!("删除旧 tx 前持久化跟踪记录失败 {}: {error}", path.display())
        })?;
    }
    remove_path_if_exists(path)
        .map_err(|error| format!("删除旧 tx 备份失败 {}: {error}", path.display()))?;
    untrack_pending_backup(rec, path);
    rec.updated_at = chrono_like_now();
    save_ownership(rec).map_err(|error| {
        format!(
            "旧 tx 已删除，但 ownership 清理状态保存失败 {}: {error}",
            path.display()
        )
    })
}

/// 回滚一次尚未成功的备份轮换。先将旧 ownership 与新文件
/// 的待清理路径一起持久化，再删新文件，最后清掉路径。
/// 任一步失败时，仍存在的备份都会被 durable ownership 跟踪。
fn rollback_backup_rotation(
    original: &OwnershipRecord,
    new_paths: &[std::path::PathBuf],
) -> Result<(), String> {
    let mut rollback = original.clone();
    for path in new_paths {
        track_pending_backup(&mut rollback, path);
    }
    rollback.updated_at = chrono_like_now();
    save_ownership(&rollback)
        .map_err(|error| format!("回滚 ownership 失败，新备份已保留且仍受当前记录跟踪: {error}"))?;

    for path in new_paths {
        remove_path_if_exists(path).map_err(|error| {
            format!(
                "ownership 已回滚，但删除新备份失败 {}: {error}",
                path.display()
            )
        })?;
    }
    for path in new_paths {
        untrack_pending_backup(&mut rollback, path);
    }
    rollback.updated_at = chrono_like_now();
    save_ownership(&rollback)
        .map_err(|error| format!("ownership 已回滚且新备份已删除，但清理记录保存失败: {error}"))
}

/// 写一组 env；任一项失败则回滚本批已写键
fn set_user_envs_transactional(
    before: &std::collections::BTreeMap<String, Option<String>>,
    pairs: &[(&str, &str)],
) -> Result<Vec<String>, String> {
    let mut written_keys: Vec<&str> = Vec::new();
    for (k, v) in pairs {
        // 即使平台写函数返回错误，也按“可能部分落盘”处理并恢复该键。
        written_keys.push(k);
        if let Err(e) = set_user_env(k, v) {
            let rollback = restore_env_snapshot(before, &written_keys);
            return match rollback {
                Ok(()) => Err(e),
                Err(re) => Err(format!("{e}；且环境变量回滚不完整: {re}")),
            };
        }
    }
    Ok(written_keys
        .into_iter()
        .map(|k| format!("ENV:{}", k))
        .collect())
}

fn load_claude_settings_for_edit() -> Result<(std::path::PathBuf, Value), String> {
    let dir = claude_config_dir().ok_or("找不到 ~/.claude")?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.join("settings.json");
    reject_top_level_config_link(&path, "~/.claude/settings.json")?;
    let root: Value = if path.exists() {
        let raw = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
        match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(e) => {
                return Err(format!(
                    "~/.claude/settings.json 解析失败，已中止写入以免破坏你的配置: {}\n路径: {}",
                    e,
                    path.display()
                ));
            }
        }
    } else {
        json!({})
    };
    if !root.is_object() {
        return Err(format!(
            "~/.claude/settings.json 根节点不是对象，已中止。路径: {}",
            path.display()
        ));
    }
    if let Some(env) = root.get("env") {
        if !env.is_object() {
            return Err("~/.claude/settings.json 的 env 字段不是对象，已中止以免破坏".into());
        }
    }
    Ok((path, root))
}

fn apply_claude_settings_env_in_memory(
    mut root: Value,
    base_url: &str,
    api_key: &str,
    model: Option<&str>,
    rec: &mut OwnershipRecord,
) -> Result<Value, String> {
    let obj = root.as_object_mut().unwrap();
    let env_entry = obj.entry("env").or_insert_with(|| json!({}));
    let env_obj = env_entry.as_object_mut().ok_or("env 不是对象")?;

    for k in CLAUDE_ENV_KEYS {
        if !rec.settings_env_prev.contains_key(*k) {
            let prev = env_obj
                .get(*k)
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            rec.settings_env_prev.insert((*k).to_string(), prev);
        }
    }

    env_obj.insert("ANTHROPIC_BASE_URL".into(), json!(base_url));
    env_obj.insert("ANTHROPIC_API_KEY".into(), json!(api_key));
    env_obj.insert("ANTHROPIC_AUTH_TOKEN".into(), json!(api_key));
    match model {
        Some(m) if !m.is_empty() => {
            env_obj.insert("ANTHROPIC_MODEL".into(), json!(m));
        }
        _ => {
            env_obj.remove("ANTHROPIC_MODEL");
        }
    }
    Ok(root)
}

fn restore_claude_settings_from_ownership(rec: &OwnershipRecord) -> Result<(), String> {
    let Some(dir) = claude_config_dir() else {
        return Ok(());
    };
    let path = dir.join("settings.json");
    reject_top_level_config_link(&path, "~/.claude/settings.json")?;

    // 若文件是我们创建的且原先不存在任何 settings_env，可删空文件
    if !path.exists() {
        return Ok(());
    }

    let raw = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    let mut root: Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            // 有备份则尝试还原备份
            for bak in [&rec.settings_tx_bak, &rec.settings_baseline_bak]
                .into_iter()
                .flatten()
            {
                if bak == BASELINE_ABSENT {
                    remove_path_if_exists(&path)?;
                    return Ok(());
                }
                if std::path::Path::new(bak).exists() {
                    restore_text_backup_atomic(std::path::Path::new(bak), &path, "settings 备份")
                        .map_err(|e2| format!("settings 损坏且备份恢复失败: {e} / {e2}"))?;
                    return Ok(());
                }
            }
            return Err(format!("清除时 settings.json 解析失败，未改动文件: {}", e));
        }
    };
    let Some(obj) = root.as_object_mut() else {
        return Ok(());
    };

    // 无 ownership 记录：不要乱删用户字段
    if rec.settings_env_prev.is_empty() {
        return Ok(());
    } else {
        let env_entry = obj.entry("env").or_insert_with(|| json!({}));
        if let Some(env) = env_entry.as_object_mut() {
            for (k, prev) in &rec.settings_env_prev {
                match prev {
                    Some(v) => {
                        env.insert(k.clone(), json!(v));
                    }
                    None => {
                        env.remove(k);
                    }
                }
            }
            if env.is_empty() {
                obj.remove("env");
            }
        }
    }

    // 若根只剩空对象且我们有「原先不存在」线索，删文件
    let only_empty = root.as_object().map(|o| o.is_empty()).unwrap_or(false);
    if only_empty && rec.settings_baseline_bak.is_none() {
        remove_path_if_exists(&path)?;
        return Ok(());
    }

    let pretty = serde_json::to_string_pretty(&root).map_err(|e| e.to_string())?;
    atomic_write(&path, &pretty)?;
    Ok(())
}

fn restore_file_from_tx_backup(
    tx_backup: Option<&str>,
    destination: &std::path::Path,
    label: &str,
) -> Result<(), String> {
    reject_top_level_config_link(destination, label)?;
    let backup = tx_backup.ok_or_else(|| format!("{label} 缺少事务回滚备份记录"))?;
    if backup == BASELINE_ABSENT {
        return remove_path_if_exists(destination);
    }
    let backup_path = std::path::Path::new(backup);
    if !backup_path.exists() {
        return Err(format!(
            "{label} 事务回滚备份不存在: {}",
            backup_path.display()
        ));
    }
    restore_text_backup_atomic(backup_path, destination, label)
}

fn format_transaction_failure(
    primary: String,
    config_rollback: Result<(), String>,
    ownership_rollback: Option<Result<(), String>>,
) -> String {
    let mut details = vec![primary];
    if let Err(error) = config_rollback {
        details.push(format!("主配置/环境回滚不完整: {error}"));
        details.push("备份与 ownership 已保留，未冒险删除".into());
    }
    if let Some(Err(error)) = ownership_rollback {
        details.push(format!("ownership/新备份回滚不完整: {error}"));
    }
    details.join("；")
}

fn write_claude_config(base_url: &str, api_key: &str, model: &str) -> Result<Vec<String>, String> {
    validate_env_value("API Key", api_key)?;
    validate_env_value("Base URL", base_url)?;
    let model = model.trim();
    if model.is_empty() {
        return Err("模型名不能为空".into());
    }
    validate_env_value("model", model)?;

    // 1) 预校验 settings，并记录“本次 apply 前”的事务快照。
    let (settings_path, root) = load_claude_settings_for_edit()?;
    let settings_existed = settings_path.exists();
    let env_before = snapshot_env(CLAUDE_ENV_KEYS)?;

    let mut rec = load_ownership()?;
    validate_recorded_backup_paths(&rec)?;
    gc_pending_backups(&mut rec)?;
    let original_rec = rec.clone();
    for k in CLAUDE_ENV_KEYS {
        remember_env(&mut rec, k)?;
    }

    // 2) 内存改 settings + 记字段 ownership
    let new_root =
        apply_claude_settings_env_in_memory(root, base_url, api_key, Some(model), &mut rec)?;
    let pretty = serde_json::to_string_pretty(&new_root).map_err(|e| e.to_string())?;

    // 3) 先在 durable ownership 登记即将创建的备份路径，再复制文件。
    // 这样 save/copy/进程崩溃的任意失败点都不会产生无法跟踪的明文 Key 备份。
    let new_baseline_path = if rec.settings_baseline_bak.is_none() && settings_existed {
        Some(
            settings_path
                .with_file_name(format!("settings.json.codecli-baseline.{}", unique_stamp())),
        )
    } else {
        None
    };
    let new_tx_path = settings_existed.then(|| {
        settings_path.with_file_name(format!("settings.json.codecli-tx.{}", unique_stamp()))
    });
    let new_tx = new_tx_path
        .as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| BASELINE_ABSENT.into());
    let new_paths: Vec<std::path::PathBuf> = new_baseline_path
        .iter()
        .chain(new_tx_path.iter())
        .cloned()
        .collect();
    for path in &new_paths {
        track_pending_backup(&mut rec, path);
    }
    rec.updated_at = chrono_like_now();
    save_ownership(&rec)?;

    let backup_copy_result = (|| -> Result<(), String> {
        if let Some(path) = &new_baseline_path {
            copy_private_text_backup(&settings_path, path, "settings.json baseline")?;
        }
        if let Some(path) = &new_tx_path {
            copy_private_text_backup(&settings_path, path, "settings.json tx")?;
        }
        Ok(())
    })();
    if let Err(error) = backup_copy_result {
        let rollback = rollback_backup_rotation(&original_rec, &new_paths);
        return Err(format_transaction_failure(error, Ok(()), Some(rollback)));
    }

    let old_tx = original_rec.settings_tx_bak.clone();
    if rec.settings_baseline_bak.is_none() {
        rec.settings_baseline_bak = Some(
            new_baseline_path
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| BASELINE_ABSENT.into()),
        );
    }
    rec.settings_tx_bak = Some(new_tx.clone());
    for path in &new_paths {
        untrack_pending_backup(&mut rec, path);
    }
    if let Some(raw) = old_tx.as_deref() {
        if raw != BASELINE_ABSENT && raw != new_tx {
            track_pending_backup(&mut rec, std::path::Path::new(raw));
        }
    }
    rec.updated_at = chrono_like_now();
    if let Err(error) = save_ownership(&rec) {
        let rollback = rollback_backup_rotation(&original_rec, &new_paths);
        return Err(format_transaction_failure(
            format!("提交 settings 备份 ownership 失败: {error}"),
            Ok(()),
            Some(rollback),
        ));
    }

    // 4) 主配置与 env 任一失败，先用新 tx 恢复；全部恢复成功后
    // 才回滚 ownership 并删除新备份。否则保留备份以便后续人工恢复。
    let apply_result = (|| -> Result<Vec<String>, String> {
        atomic_write(&settings_path, &pretty)?;
        let mut written = vec![settings_path.display().to_string()];
        written.extend(set_user_envs_transactional(
            &env_before,
            &[
                ("ANTHROPIC_BASE_URL", base_url),
                ("ANTHROPIC_API_KEY", api_key),
                ("ANTHROPIC_AUTH_TOKEN", api_key),
            ],
        )?);
        set_user_env("ANTHROPIC_MODEL", model)?;
        written.push("ENV:ANTHROPIC_MODEL".into());

        if let Some(dir) = home_state_dir() {
            std::fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
            let path = dir.join("last-claude.json");
            let body = json!({
                "baseUrl": base_url,
                "apiKeyMasked": mask_key(api_key),
                "model": model,
                "updatedAt": chrono_like_now(),
            });
            // 诊断元数据失败不影响已成功的主配置，也不宣称已写入。
            if atomic_write(
                &path,
                &serde_json::to_string_pretty(&body).unwrap_or_default(),
            )
            .is_ok()
            {
                written.push(path.display().to_string());
            }
        }
        Ok(written)
    })();

    let written = match apply_result {
        Ok(written) => written,
        Err(error) => {
            let env_rollback = restore_env_snapshot(&env_before, CLAUDE_ENV_KEYS);
            let file_rollback = restore_file_from_tx_backup(
                rec.settings_tx_bak.as_deref(),
                &settings_path,
                "settings.json",
            );
            let config_rollback = match (env_rollback, file_rollback) {
                (Ok(()), Ok(())) => Ok(()),
                (env, file) => {
                    let mut details = Vec::new();
                    if let Err(error) = env {
                        details.push(format!("环境变量: {error}"));
                    }
                    if let Err(error) = file {
                        details.push(format!("settings.json: {error}"));
                    }
                    Err(details.join("；"))
                }
            };
            let ownership_rollback = if config_rollback.is_ok() {
                Some(rollback_backup_rotation(&original_rec, &new_paths))
            } else {
                None
            };
            return Err(format_transaction_failure(
                format!("Claude 配置写入失败: {error}"),
                config_rollback,
                ownership_rollback,
            ));
        }
    };

    // 只有主配置和 env 都成功后才 GC 旧 tx。删除失败必须显式报错，
    // 而 durable pending_backup_cleanup 仍保留旧路径供下次重试。
    gc_old_tx(&mut rec, &old_tx, &new_tx)
        .map_err(|error| format!("Claude 配置已写入，但旧 tx 备份清理失败: {error}"))?;
    Ok(written)
}

fn load_codex_doc_for_edit() -> Result<(std::path::PathBuf, DocumentMut, bool), String> {
    let dir = codex_config_dir().ok_or("找不到 Codex 配置目录")?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = codex_config_toml().ok_or("找不到 config.toml 路径")?;
    reject_top_level_config_link(&path, "~/.codex/config.toml")?;
    let existed = path.exists();
    let existing = if existed {
        std::fs::read_to_string(&path).map_err(|e| e.to_string())?
    } else {
        String::new()
    };
    let doc: DocumentMut = if existing.trim().is_empty() {
        DocumentMut::new()
    } else {
        existing
            .parse::<DocumentMut>()
            .map_err(|e| format!("~/.codex/config.toml 解析失败，已中止: {}", e))?
    };
    Ok((path, doc, existed))
}

fn write_codex_config(base_url: &str, api_key: &str, model: &str) -> Result<Vec<String>, String> {
    validate_env_value("API Key", api_key)?;
    validate_env_value("Base URL", base_url)?;
    let model_line = model.trim();
    if model_line.is_empty() {
        return Err("模型名不能为空".into());
    }
    validate_env_value("model", model_line)?;

    let (path, mut doc, existed) = load_codex_doc_for_edit()?;
    let env_before = snapshot_env(CODEX_ENV_KEYS)?;
    let mut rec = load_ownership()?;
    validate_recorded_backup_paths(&rec)?;
    gc_pending_backups(&mut rec)?;
    let original_rec = rec.clone();
    for k in CODEX_ENV_KEYS {
        remember_env(&mut rec, k)?;
    }

    if !rec.codex_root_prev.contains_key("model") {
        rec.codex_root_prev.insert(
            "model".into(),
            doc.get("model")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
        );
    }
    if !rec.codex_root_prev.contains_key("model_provider") {
        rec.codex_root_prev.insert(
            "model_provider".into(),
            doc.get("model_provider")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
        );
    }

    // 快照既有同名 provider（极少，我们用专用 id）
    let had_provider = doc
        .get("model_providers")
        .and_then(|i| i.as_table())
        .map(|t| t.contains_key(CODEX_PROVIDER_ID))
        .unwrap_or(false);
    if had_provider && !rec.codex_provider_created && rec.codex_provider_prev_toml.is_none() {
        if let Some(item) = doc
            .get("model_providers")
            .and_then(|i| i.as_table())
            .and_then(|t| t.get(CODEX_PROVIDER_ID))
        {
            rec.codex_provider_prev_toml = Some(item.to_string());
            rec.codex_provider_created = false;
        }
    } else if !had_provider && rec.codex_provider_prev_toml.is_none() {
        rec.codex_provider_created = true;
    }

    let base = base_url.trim_end_matches('/');

    doc["model"] = Item::Value(TomlValue::from(model_line));
    doc["model_provider"] = Item::Value(TomlValue::from(CODEX_PROVIDER_ID));

    let providers = doc["model_providers"]
        .or_insert(Item::Table(Table::new()))
        .as_table_mut()
        .ok_or("config.toml 的 model_providers 不是表")?;
    let custom = providers
        .entry(CODEX_PROVIDER_ID)
        .or_insert(Item::Table(Table::new()))
        .as_table_mut()
        .ok_or("无法创建 model_providers.codecli_installer")?;
    custom["name"] = Item::Value(TomlValue::from("CodeCLI Installer"));
    custom["base_url"] = Item::Value(TomlValue::from(base));
    custom["env_key"] = Item::Value(TomlValue::from("OPENAI_API_KEY"));
    // Codex CLI 0.144+ only accepts the Responses wire protocol for custom
    // providers.  `chat` makes strict config loading fail before any request
    // is sent, so the connectivity probe must exercise the same protocol too.
    custom["wire_api"] = Item::Value(TomlValue::from("responses"));

    let out = doc.to_string();
    let _reparse: DocumentMut = out
        .parse()
        .map_err(|e| format!("写出后 TOML 自检失败: {}", e))?;

    // 与 Claude 路径相同：先 durable 登记新备份名，再创建文件。
    let new_baseline_path = if rec.codex_baseline_bak.is_none() && existed {
        Some(path.with_extension(format!("toml.codecli-baseline.{}", unique_stamp())))
    } else {
        None
    };
    let new_tx_path =
        existed.then(|| path.with_extension(format!("toml.codecli-tx.{}", unique_stamp())));
    let new_tx = new_tx_path
        .as_ref()
        .map(|backup| backup.display().to_string())
        .unwrap_or_else(|| BASELINE_ABSENT.into());
    let new_paths: Vec<std::path::PathBuf> = new_baseline_path
        .iter()
        .chain(new_tx_path.iter())
        .cloned()
        .collect();
    for backup in &new_paths {
        track_pending_backup(&mut rec, backup);
    }
    rec.updated_at = chrono_like_now();
    save_ownership(&rec)?;

    let backup_copy_result = (|| -> Result<(), String> {
        if let Some(backup) = &new_baseline_path {
            copy_private_text_backup(&path, backup, "config.toml baseline")?;
        }
        if let Some(backup) = &new_tx_path {
            copy_private_text_backup(&path, backup, "config.toml tx")?;
        }
        Ok(())
    })();
    if let Err(error) = backup_copy_result {
        let rollback = rollback_backup_rotation(&original_rec, &new_paths);
        return Err(format_transaction_failure(error, Ok(()), Some(rollback)));
    }

    let old_tx = original_rec.codex_tx_bak.clone();
    if rec.codex_baseline_bak.is_none() {
        rec.codex_baseline_bak = Some(
            new_baseline_path
                .as_ref()
                .map(|backup| backup.display().to_string())
                .unwrap_or_else(|| BASELINE_ABSENT.into()),
        );
    }
    rec.codex_tx_bak = Some(new_tx.clone());
    for backup in &new_paths {
        untrack_pending_backup(&mut rec, backup);
    }
    if let Some(raw) = old_tx.as_deref() {
        if raw != BASELINE_ABSENT && raw != new_tx {
            track_pending_backup(&mut rec, std::path::Path::new(raw));
        }
    }
    rec.updated_at = chrono_like_now();
    if let Err(error) = save_ownership(&rec) {
        let rollback = rollback_backup_rotation(&original_rec, &new_paths);
        return Err(format_transaction_failure(
            format!("提交 Codex 备份 ownership 失败: {error}"),
            Ok(()),
            Some(rollback),
        ));
    }

    let apply_result = (|| -> Result<Vec<String>, String> {
        atomic_write(&path, &out)?;
        let mut written = vec![path.display().to_string()];
        written.extend(set_user_envs_transactional(
            &env_before,
            &[("OPENAI_API_KEY", api_key), ("OPENAI_BASE_URL", base_url)],
        )?);
        Ok(written)
    })();

    let written = match apply_result {
        Ok(written) => written,
        Err(error) => {
            let env_rollback = restore_env_snapshot(&env_before, CODEX_ENV_KEYS);
            let file_rollback =
                restore_file_from_tx_backup(rec.codex_tx_bak.as_deref(), &path, "config.toml");
            let config_rollback = match (env_rollback, file_rollback) {
                (Ok(()), Ok(())) => Ok(()),
                (env, file) => {
                    let mut details = Vec::new();
                    if let Err(error) = env {
                        details.push(format!("环境变量: {error}"));
                    }
                    if let Err(error) = file {
                        details.push(format!("config.toml: {error}"));
                    }
                    Err(details.join("；"))
                }
            };
            let ownership_rollback = if config_rollback.is_ok() {
                Some(rollback_backup_rotation(&original_rec, &new_paths))
            } else {
                None
            };
            return Err(format_transaction_failure(
                format!("Codex 配置写入失败: {error}"),
                config_rollback,
                ownership_rollback,
            ));
        }
    };

    gc_old_tx(&mut rec, &old_tx, &new_tx)
        .map_err(|error| format!("Codex 配置已写入，但旧 tx 备份清理失败: {error}"))?;
    Ok(written)
}

/// full_file_rollback=true：事务失败，用 tx/baseline 整文件回滚
/// full_file_rollback=false：clear 字段级恢复，保留用户后续改动
fn parse_codex_provider_snapshot(raw: &str) -> Result<Item, String> {
    // `Item::to_string()` 对 Table 只输出表体，包一层临时表名后再解析。
    let wrapped = format!("[snapshot]\n{}", raw);
    let mut doc: DocumentMut = wrapped
        .parse()
        .map_err(|e| format!("原 Codex provider 快照解析失败: {}", e))?;
    doc.as_table_mut()
        .remove("snapshot")
        .ok_or_else(|| "原 Codex provider 快照为空".to_string())
}

/// Some(Some(item)) = baseline 中原有同名 provider；Some(None) = baseline 明确没有；
/// None = 无可用 baseline，由调用方回退到 ownership 字段快照。
fn codex_provider_from_baseline(rec: &OwnershipRecord) -> Result<Option<Option<Item>>, String> {
    let Some(baseline) = rec.codex_baseline_bak.as_deref() else {
        return Ok(None);
    };
    if baseline == BASELINE_ABSENT {
        return Ok(Some(None));
    }
    let path = std::path::Path::new(baseline);
    if !path.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(path)
        .map_err(|e| format!("读取 Codex baseline 失败 {}: {}", path.display(), e))?;
    let doc: DocumentMut = raw
        .parse()
        .map_err(|e| format!("Codex baseline TOML 解析失败: {}", e))?;
    Ok(Some(
        doc.get("model_providers")
            .and_then(|i| i.as_table())
            .and_then(|t| t.get(CODEX_PROVIDER_ID))
            .cloned(),
    ))
}

fn restore_codex_from_ownership(
    rec: &OwnershipRecord,
    full_file_rollback: bool,
) -> Result<(), String> {
    let Some(path) = codex_config_toml() else {
        return Ok(());
    };
    reject_top_level_config_link(&path, "~/.codex/config.toml")?;

    if full_file_rollback {
        for bak in [&rec.codex_tx_bak, &rec.codex_baseline_bak]
            .into_iter()
            .flatten()
        {
            if bak == BASELINE_ABSENT {
                remove_path_if_exists(&path)?;
                return Ok(());
            }
            if std::path::Path::new(bak).exists() {
                restore_text_backup_atomic(std::path::Path::new(bak), &path, "config.toml 备份")?;
                return Ok(());
            }
        }
    }

    if !path.exists() {
        return Ok(());
    }
    let existing = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    if existing.trim().is_empty() {
        return Ok(());
    }
    let mut doc: DocumentMut = match existing.parse() {
        Ok(d) => d,
        Err(e) => {
            return Err(format!("清除时 config.toml 解析失败，未改动: {}", e));
        }
    };

    for key in ["model", "model_provider"] {
        if let Some(prev) = rec.codex_root_prev.get(key) {
            match prev {
                Some(v) => {
                    doc[key] = Item::Value(TomlValue::from(v.as_str()));
                }
                None => {
                    doc.as_table_mut().remove(key);
                }
            }
        } else {
            let is_ours = doc
                .get("model_provider")
                .and_then(|v| v.as_str())
                .map(|s| s == CODEX_PROVIDER_ID)
                .unwrap_or(false);
            if is_ours {
                doc.as_table_mut().remove(key);
            }
        }
    }

    // 仅当我们有安装痕迹时才恢复/移除专用 provider。
    // baseline 是首次写入前的权威值，可同时修复旧版「第二次 apply
    // 把自己的 provider 误记为用户快照」的 ownership 数据。
    let touched = rec.codex_provider_created
        || rec.codex_provider_prev_toml.is_some()
        || rec.codex_root_prev.contains_key("model_provider")
        || rec.codex_baseline_bak.is_some()
        || rec.codex_tx_bak.is_some()
        || !rec.env_prev.is_empty();
    if touched {
        let baseline_provider = codex_provider_from_baseline(rec)?;
        let provider_to_restore = match baseline_provider {
            Some(item) => item,
            None => rec
                .codex_provider_prev_toml
                .as_deref()
                .map(parse_codex_provider_snapshot)
                .transpose()?,
        };

        if let Some(item) = provider_to_restore {
            let providers = doc["model_providers"]
                .or_insert(Item::Table(Table::new()))
                .as_table_mut()
                .ok_or("config.toml 的 model_providers 不是表，无法恢复原 provider")?;
            providers.insert(CODEX_PROVIDER_ID, item);
        } else if let Some(providers) = doc
            .get_mut("model_providers")
            .and_then(|i| i.as_table_mut())
        {
            providers.remove(CODEX_PROVIDER_ID);
            if providers.is_empty() {
                doc.as_table_mut().remove("model_providers");
            }
        }
    }

    let out = doc.to_string();
    if out.trim().is_empty() {
        remove_path_if_exists(&path)?;
    } else {
        atomic_write(&path, &out)?;
    }
    Ok(())
}

fn restore_env_from_ownership(keys: &[&str]) -> Result<Vec<String>, String> {
    let rec = load_ownership()?;
    validate_recorded_backup_paths(&rec)?;
    let mut notes = Vec::new();
    for k in keys {
        match rec.env_prev.get(*k) {
            Some(Some(prev)) => {
                set_user_env(k, prev)?;
                notes.push(format!("restored:{}", k));
            }
            Some(None) => {
                unset_user_env(k)?;
                notes.push(format!("removed:{}", k));
            }
            None => {
                notes.push(format!("untouched:{}", k));
            }
        }
    }
    Ok(notes)
}

#[tauri::command]
pub async fn apply_config(req: ConfigApplyRequest) -> Result<ConfigApplyResult, String> {
    super::util::spawn_blocking_result(move || with_new_operation(|| apply_config_sync(req))).await
}

pub fn apply_config_sync(req: ConfigApplyRequest) -> Result<ConfigApplyResult, String> {
    super::schemes::apply_config_with_scheme_tx(req)
}

/// 方案管理已经事务性地持久化 metadata/secret 时，禁止这里再次
/// 按 endpoint 推测并改写方案，否则更新方案时可能产生重复 ID 或覆盖其它 Key。
pub(crate) fn apply_config_without_scheme_record(
    req: ConfigApplyRequest,
) -> Result<ConfigApplyResult, String> {
    // 只能由已持有 op_lock、且已经建立 durable scheme journal 的内部
    // 路径调用。恢复旧事务时不再执行与当前请求相关的预处理。
    apply_config_sync_inner(req)
}

fn apply_config_sync_inner(req: ConfigApplyRequest) -> Result<ConfigApplyResult, String> {
    let key = req.api_key.trim();
    if key.is_empty() {
        return Err("API Key 不能为空".into());
    }

    let provider = find_provider(&req.provider_id);
    let base_url_raw = req
        .base_url
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .or_else(|| provider.as_ref().map(|p| p.base_url.clone()))
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "Base URL 不能为空，请选择服务商或手动填写".to_string())?;

    let base_url = validate_base_url(&base_url_raw)?;

    let target = req.target.to_lowercase();
    if target == "both" {
        return Err(
            "v1 已取消「两者」：请分别配置 Claude（Anthropic 兼容）与 Codex（OpenAI 兼容）".into(),
        );
    }
    if target != "claude" && target != "codex" {
        return Err(format!("未知 target: {}（用 claude|codex）", req.target));
    }

    let model = req
        .model
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .or_else(|| provider.as_ref().and_then(|p| p.default_model.clone()))
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| "模型名不能为空；自定义服务商必须填写实际模型".to_string())?;

    let protocol = provider.as_ref().map(|p| p.protocol.as_str()).unwrap_or("");

    if target == "claude" {
        if protocol == "openai" {
            return Err(
                "当前服务商是 OpenAI 兼容，不能写入 Claude。请切到「Codex」用途或换 Anthropic 兼容服务商"
                    .into(),
            );
        }
        let written = write_claude_config(&base_url, key, model.as_str())?;
        return Ok(ConfigApplyResult {
            ok: true,
            message: format!(
                "Claude 配置已写入（Key: {}）。新开终端后 shell 环境变量生效；settings.json env 立即可用。",
                mask_key(key)
            ),
            written,
        });
    }
    if target == "codex" {
        if protocol == "anthropic" {
            return Err(
                "当前服务商是 Anthropic 兼容，不能写入 Codex。请切到「Claude」用途或换 OpenAI 兼容服务商"
                    .into(),
            );
        }
        let written = write_codex_config(&base_url, key, model.as_str())?;
        return Ok(ConfigApplyResult {
            ok: true,
            message: format!(
                "Codex 配置已写入（Key: {}）。新开终端后环境变量生效。",
                mask_key(key)
            ),
            written,
        });
    }

    unreachable!("target 已在写入前校验")
}

#[tauri::command]
pub async fn clear_config(target: String) -> Result<ConfigApplyResult, String> {
    super::util::spawn_blocking_result(move || {
        with_new_operation(|| {
            let normalized = target.to_lowercase();
            if normalized != "claude" && normalized != "codex" && normalized != "both" {
                return Err(format!("未知 target: {target}"));
            }
            let pending_clear = super::schemes::prepare_pending_for_config_clear(&normalized)?;
            let mut result = clear_config_sync(normalized.clone())?;
            if pending_clear {
                super::schemes::finish_pending_after_config_clear()?;
                result.written.push("finished-pending-scheme-clear".into());
            }
            result
                .written
                .extend(super::schemes::deactivate_after_config_clear(&normalized)?);
            Ok(result)
        })
    })
    .await
}

/// 先恢复配置，再删除本工具自己的状态目录。
/// 不删除 ~/.claude、~/.codex 或其中任何非本工具文件。
#[tauri::command]
pub async fn purge_tool_data() -> Result<ConfigApplyResult, String> {
    super::util::spawn_blocking_result(|| {
        with_new_operation(|| {
            super::log_bus::suspend_diagnostic_writes_for(|| {
                // 上次若崩溃在 rename / remove_dir_all 中间，先依据
                // 父目录中的 durable marker 完成隔离目录清理。
                recover_pending_state_dir_purge()?;
                // 所有 clear/read/delete 之前先验证状态目录，防止跟随恶意链接。
                let original_state_dir = validated_state_dir()?
                    .map(|dir| state_dir_identity(&dir).map(|identity| (dir, identity)))
                    .transpose()?;
                // 完全卸载也必须在清 CLI 前 durable 标记 Clear；否则
                // 进程若在 clear 成功后崩溃，旧 Apply journal 会在下次
                // 启动前滚重放，撤销用户的卸载意图。
                let pending_clear = super::schemes::prepare_pending_for_purge()?;
                let mut result = clear_config_sync("both".into())?;
                if pending_clear {
                    super::schemes::finish_pending_after_config_clear()?;
                    result.written.push("finished-pending-scheme-clear".into());
                }
                result
                    .written
                    .extend(super::schemes::clear_all_scheme_secrets()?);
                // 状态目录里保存着扩展/CLI 的唯一 ownership。必须先完成所有
                // 外部副作用清理，再删状态；任一步失败都 fail-closed，保留记录重试。
                result
                    .written
                    .extend(super::extensions::uninstall_owned_extensions_for_purge()?);
                result
                    .written
                    .extend(super::runtime::prepare_runtime_state_for_purge()?);
                // profile source block 同样必须在状态目录删除前确认清理完成。
                super::platform::remove_tool_secret_source_block()?;
                if let Some((dir, identity)) = original_state_dir {
                    remove_state_dir_with_identity(&dir, &identity)?;
                    result.written.push(format!("removed:{}", dir.display()));
                }
                result.message =
                    "已恢复本工具修改的配置，并删除扩展、运行时路径、方案、备份、日志和本地状态"
                        .into();
                Ok(result)
            })
        })
    })
    .await
}

pub fn clear_config_sync(target: String) -> Result<ConfigApplyResult, String> {
    let mut written = Vec::new();
    let t = target.to_lowercase();
    if t != "claude" && t != "codex" && t != "both" {
        return Err(format!("未知 target: {}", target));
    }

    let mut rec = load_ownership()?;
    validate_recorded_backup_paths(&rec)?;
    // 先继续上次失败/崩溃留下的「已跟踪待清理」备份。
    // 否则 clear/purge 会删状态记录却把可能含 Key 的文件留在外部。
    gc_pending_backups(&mut rec)?;
    let has_claude_own = !rec.settings_env_prev.is_empty()
        || rec.settings_baseline_bak.is_some()
        || rec.settings_tx_bak.is_some()
        || CLAUDE_ENV_KEYS
            .iter()
            .any(|k| rec.env_prev.contains_key(*k));
    let has_codex_own = !rec.codex_root_prev.is_empty()
        || rec.codex_provider_created
        || rec.codex_provider_prev_toml.is_some()
        || rec.codex_baseline_bak.is_some()
        || rec.codex_tx_bak.is_some()
        || CODEX_ENV_KEYS.iter().any(|k| rec.env_prev.contains_key(*k));

    let want_claude = t == "claude" || t == "both";
    let want_codex = t == "codex" || t == "both";
    if want_claude && !has_claude_own {
        written.push("skip:claude(no ownership — 未改你的配置)".into());
    }
    if want_codex && !has_codex_own {
        written.push("skip:codex(no ownership — 未改你的配置)".into());
    }
    // 请求范围内都无 ownership。
    let nothing = (!want_claude || !has_claude_own) && (!want_codex || !has_codex_own);
    if nothing {
        return Ok(ConfigApplyResult {
            ok: true,
            message: "未发现本工具写入记录，未修改任何配置".into(),
            written,
        });
    }

    if (t == "claude" || t == "both") && has_claude_own {
        written.extend(restore_env_from_ownership(CLAUDE_ENV_KEYS)?);
        // 默认字段级恢复，保留用户装后自改的 hooks/permissions
        restore_claude_settings_from_ownership(&rec)?;
        written.push("restored:settings.json env fields only".into());
        // 若我们创建了文件且清除后为空，删除
        if rec.settings_baseline_bak.as_deref() == Some(BASELINE_ABSENT) {
            if let Some(dir) = claude_config_dir() {
                let path = dir.join("settings.json");
                if path.exists() {
                    if let Ok(raw) = std::fs::read_to_string(&path) {
                        if let Ok(v) = serde_json::from_str::<Value>(&raw) {
                            if v.as_object().map(|o| o.is_empty()).unwrap_or(false) {
                                remove_path_if_exists(&path)?;
                                written.push("removed:empty settings.json".into());
                            }
                        }
                    }
                }
            }
        }
        if let Some(dir) = home_state_dir() {
            remove_path_if_exists(&dir.join("last-claude.json"))?;
        }
        if let Some(dir) = claude_config_dir() {
            remove_path_if_exists(&dir.join("codecli-installer.json"))?;
        }
        // GC 备份文件（含旧 Key）— 失败要报错
        for p in [&rec.settings_tx_bak, &rec.settings_baseline_bak]
            .into_iter()
            .flatten()
        {
            if p != BASELINE_ABSENT {
                let path = std::path::Path::new(p);
                if path.exists() {
                    remove_path_if_exists(path)?;
                    written.push(format!("deleted-bak:{}", p));
                }
            }
        }
        for k in CLAUDE_ENV_KEYS {
            rec.env_prev.remove(*k);
        }
        rec.settings_env_prev.clear();
        rec.settings_baseline_bak = None;
        rec.settings_tx_bak = None;
    }
    if (t == "codex" || t == "both") && has_codex_own {
        written.extend(restore_env_from_ownership(CODEX_ENV_KEYS)?);
        // 字段级恢复，不用整文件 baseline 覆盖用户后续改动
        restore_codex_from_ownership(&rec, false)?;
        written.push("restored:codex fields only".into());
        for p in [&rec.codex_tx_bak, &rec.codex_baseline_bak]
            .into_iter()
            .flatten()
        {
            if p != BASELINE_ABSENT {
                let path = std::path::Path::new(p);
                if path.exists() {
                    remove_path_if_exists(path)?;
                    written.push(format!("deleted-bak:{}", p));
                }
            }
        }
        for k in CODEX_ENV_KEYS {
            rec.env_prev.remove(*k);
        }
        rec.codex_root_prev.clear();
        rec.codex_provider_created = false;
        rec.codex_provider_prev_toml = None;
        rec.codex_baseline_bak = None;
        rec.codex_tx_bak = None;
    }

    rec.updated_at = chrono_like_now();
    save_ownership(&rec).map_err(|e| {
        format!(
            "配置已清除，但 ownership 状态保存失败（请勿重复操作，查看项目支持文档）: {}",
            e
        )
    })?;

    Ok(ConfigApplyResult {
        ok: true,
        message:
            "已清除本工具写入的配置（字段级恢复，保留你之后自改的 hooks/其它配置）；备份文件已清理"
                .into(),
        written,
    })
}

#[cfg(test)]
mod config_injection_tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    // 串行化 HOME 切换，避免并行测试互踩
    static HOME_LOCK: Mutex<()> = Mutex::new(());

    struct TempHome {
        dir: PathBuf,
        old_test_home: Option<PathBuf>,
        _guard: std::sync::MutexGuard<'static, ()>,
    }

    impl TempHome {
        fn new() -> Self {
            let guard = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let dir = std::env::temp_dir().join(format!(
                "codecli-cfg-test-{}-{}",
                std::process::id(),
                unique_stamp()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let old_test_home = super::super::platform::replace_test_home_dir(Some(dir.clone()));
            // 清本进程里可能残留的相关 env，避免污染断言
            for k in CLAUDE_ENV_KEYS.iter().chain(CODEX_ENV_KEYS.iter()) {
                unsafe { std::env::remove_var(k) };
            }
            Self {
                dir,
                old_test_home,
                _guard: guard,
            }
        }

        fn path(&self) -> &Path {
            &self.dir
        }
    }

    impl Drop for TempHome {
        fn drop(&mut self) {
            super::super::platform::replace_test_home_dir(self.old_test_home.take());
            for k in CLAUDE_ENV_KEYS.iter().chain(CODEX_ENV_KEYS.iter()) {
                unsafe { std::env::remove_var(k) };
            }
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn claude_req(key: &str, model: &str) -> ConfigApplyRequest {
        ConfigApplyRequest {
            provider_id: "deepseek-anthropic".into(),
            api_key: key.into(),
            base_url: None,
            model: Some(model.into()),
            target: "claude".into(),
        }
    }

    fn codex_req(key: &str) -> ConfigApplyRequest {
        ConfigApplyRequest {
            provider_id: "qwen-openai".into(),
            api_key: key.into(),
            base_url: None,
            model: Some("qwen-plus".into()),
            target: "codex".into(),
        }
    }

    #[test]
    fn reject_empty_key_and_both_and_protocol_mismatch() {
        let _h = TempHome::new();
        let mut r = claude_req("sk-testkey123456", "m");
        r.api_key = "  ".into();
        assert!(apply_config_sync(r).is_err());

        let mut r = claude_req("sk-testkey123456", "m");
        r.target = "both".into();
        assert!(apply_config_sync(r).unwrap_err().contains("两者"));

        // openai provider -> claude target
        let r = ConfigApplyRequest {
            provider_id: "qwen-openai".into(),
            api_key: "sk-testkey123456".into(),
            base_url: None,
            model: None,
            target: "claude".into(),
        };
        assert!(apply_config_sync(r).unwrap_err().contains("OpenAI"));
    }

    #[test]
    fn bad_settings_json_aborts_without_overwrite() {
        let h = TempHome::new();
        let claude = h.path().join(".claude");
        std::fs::create_dir_all(&claude).unwrap();
        let settings = claude.join("settings.json");
        let poison = "{ not-json, hooks: keep-me";
        std::fs::write(&settings, poison).unwrap();

        let err =
            apply_config_sync(claude_req("sk-testkey1234567890", "deepseek-chat")).unwrap_err();
        assert!(
            err.contains("解析失败") || err.contains("中止"),
            "err={err}"
        );
        let after = std::fs::read_to_string(&settings).unwrap();
        assert_eq!(after, poison, "坏 settings 不得被覆盖");
    }

    #[test]
    fn two_applies_then_clear_restores_baseline_s0() {
        let h = TempHome::new();
        let claude = h.path().join(".claude");
        std::fs::create_dir_all(&claude).unwrap();
        let settings = claude.join("settings.json");
        let s0 = r#"{
  "hooks": { "keep": true },
  "permissions": { "allow": ["Bash"] },
  "env": { "MY_CUSTOM": "preserve" }
}"#;
        std::fs::write(&settings, s0).unwrap();

        apply_config_sync(claude_req("sk-first-aaaaaaaa", "model-a")).unwrap();
        apply_config_sync(claude_req("sk-second-bbbbbbbb", "model-b")).unwrap();

        // 当前文件应含第二次 key 痕迹（settings 明文）
        let mid = std::fs::read_to_string(&settings).unwrap();
        assert!(mid.contains("sk-second-bbbbbbbb"));
        assert!(mid.contains("hooks"));

        clear_config_sync("claude".into()).unwrap();
        let restored = std::fs::read_to_string(&settings).unwrap();
        // 回到 S0：hooks 在，第二次 key 不在
        assert!(restored.contains("\"keep\": true") || restored.contains("keep"));
        assert!(restored.contains("MY_CUSTOM") || restored.contains("preserve"));
        assert!(
            !restored.contains("sk-second-bbbbbbbb"),
            "clear 后不应残留第二次 key: {restored}"
        );
        assert!(
            !restored.contains("sk-first-aaaaaaaa"),
            "clear 后不应残留第一次 key: {restored}"
        );
    }

    #[test]
    fn absent_baseline_clear_removes_created_settings() {
        let h = TempHome::new();
        // 无 settings
        let settings = h.path().join(".claude/settings.json");
        assert!(!settings.exists());

        apply_config_sync(claude_req("sk-only-cccccccccccc", "m1")).unwrap();
        assert!(settings.exists());

        clear_config_sync("claude".into()).unwrap();
        assert!(
            !settings.exists(),
            "基线 ABSENT 时 clear 应删掉本工具创建的 settings"
        );
    }

    #[test]
    fn codex_writes_dedicated_provider_and_clear_uses_baseline() {
        let h = TempHome::new();
        let codex_dir = h.path().join(".codex");
        std::fs::create_dir_all(&codex_dir).unwrap();
        let toml_path = codex_dir.join("config.toml");
        let s0 = r#"model = "user-own"
model_provider = "user_custom"

[model_providers.user_custom]
name = "User"
base_url = "https://example.com/v1"
"#;
        std::fs::write(&toml_path, s0).unwrap();

        apply_config_sync(codex_req("sk-codex-dddddddd")).unwrap();
        let mid = std::fs::read_to_string(&toml_path).unwrap();
        assert!(mid.contains("codecli_installer"));
        assert!(mid.contains("user_custom"), "不得删用户 provider");
        let mid_doc: DocumentMut = mid.parse().unwrap();
        assert_eq!(
            mid_doc["model_providers"][CODEX_PROVIDER_ID]["wire_api"].as_str(),
            Some("responses"),
            "当前 Codex CLI 只接受 Responses 协议"
        );

        apply_config_sync(codex_req("sk-codex-eeeeeeee")).unwrap();
        clear_config_sync("codex".into()).unwrap();
        let restored = std::fs::read_to_string(&toml_path).unwrap();
        assert!(restored.contains("user-own") || restored.contains("user_custom"));
        assert!(
            !restored.contains("codecli_installer"),
            "clear 后不应残留安装器 provider: {restored}"
        );
    }

    #[test]
    fn existing_same_name_codex_provider_survives_two_applies_and_clear() {
        let h = TempHome::new();
        let codex_dir = h.path().join(".codex");
        std::fs::create_dir_all(&codex_dir).unwrap();
        let toml_path = codex_dir.join("config.toml");
        let s0 = r#"model = "user-model"
model_provider = "codecli_installer"

[model_providers.codecli_installer]
name = "User Same Name"
base_url = "https://user.example/v1"
env_key = "USER_PROVIDER_KEY"
wire_api = "responses"
custom_flag = "must-survive"
"#;
        std::fs::write(&toml_path, s0).unwrap();

        apply_config_sync(codex_req("sk-codex-first-1111")).unwrap();
        apply_config_sync(codex_req("sk-codex-second-2222")).unwrap();
        clear_config_sync("codex".into()).unwrap();

        let restored = std::fs::read_to_string(&toml_path).unwrap();
        let doc: DocumentMut = restored.parse().unwrap();
        assert_eq!(doc["model"].as_str(), Some("user-model"));
        assert_eq!(doc["model_provider"].as_str(), Some("codecli_installer"));
        let provider = doc["model_providers"][CODEX_PROVIDER_ID]
            .as_table()
            .unwrap();
        assert_eq!(provider["name"].as_str(), Some("User Same Name"));
        assert_eq!(
            provider["base_url"].as_str(),
            Some("https://user.example/v1")
        );
        assert_eq!(provider["env_key"].as_str(), Some("USER_PROVIDER_KEY"));
        assert_eq!(provider["wire_api"].as_str(), Some("responses"));
        assert_eq!(provider["custom_flag"].as_str(), Some("must-survive"));
    }

    #[test]
    fn custom_provider_requires_model_without_mutating_previous_config() {
        let h = TempHome::new();
        apply_config_sync(claude_req("sk-model-first-1111", "model-a")).unwrap();
        assert_eq!(
            get_persistent_env_strict("ANTHROPIC_MODEL")
                .unwrap()
                .as_deref(),
            Some("model-a")
        );

        let without_model = ConfigApplyRequest {
            provider_id: "custom-anthropic".into(),
            api_key: "sk-model-second-2222".into(),
            base_url: Some("https://example.com/anthropic".into()),
            model: None,
            target: "claude".into(),
        };
        let before = std::fs::read_to_string(h.path().join(".claude/settings.json")).unwrap();
        let err = apply_config_sync(without_model).unwrap_err();

        assert!(err.contains("模型名不能为空"));
        assert_eq!(
            get_persistent_env_strict("ANTHROPIC_MODEL")
                .unwrap()
                .as_deref(),
            Some("model-a")
        );
        let after = std::fs::read_to_string(h.path().join(".claude/settings.json")).unwrap();
        assert_eq!(after, before, "校验失败不得写入 settings");
    }

    #[test]
    fn known_provider_uses_catalog_default_model_when_omitted() {
        let h = TempHome::new();
        let mut req = codex_req("sk-codex-default-model");
        req.model = None;
        apply_config_sync(req).unwrap();

        let config = std::fs::read_to_string(h.path().join(".codex/config.toml")).unwrap();
        let doc: DocumentMut = config.parse().unwrap();
        assert_eq!(doc["model"].as_str(), Some("qwen3.7-plus"));
    }

    #[test]
    fn clear_config_restores_managed_values() {
        let _h = TempHome::new();
        apply_config_sync(claude_req("sk-clear-community-1111", "m1")).unwrap();

        let result = clear_config_sync("claude".into()).unwrap();
        assert!(result.ok);
    }

    #[test]
    fn reject_http_base_url_in_apply() {
        let _h = TempHome::new();
        let r = ConfigApplyRequest {
            provider_id: "custom-anthropic".into(),
            api_key: "sk-testkey123456".into(),
            base_url: Some("http://insecure.example/anthropic".into()),
            model: None,
            target: "claude".into(),
        };
        assert!(apply_config_sync(r).unwrap_err().contains("HTTPS"));
    }

    #[test]
    fn no_ownership_clear_is_noop() {
        let h = TempHome::new();
        let claude = h.path().join(".claude");
        std::fs::create_dir_all(&claude).unwrap();
        let settings = claude.join("settings.json");
        let s0 = r#"{"hooks":{"keep":true},"env":{"ANTHROPIC_API_KEY":"user-own-key-not-ours"}}"#;
        std::fs::write(&settings, s0).unwrap();
        let r = clear_config_sync("both".into()).unwrap();
        assert!(r.message.contains("未发现") || r.written.iter().any(|w| w.contains("skip")));
        let after = std::fs::read_to_string(&settings).unwrap();
        assert!(
            after.contains("user-own-key-not-ours"),
            "must not strip user key: {after}"
        );
        assert!(after.contains("hooks"), "must keep hooks: {after}");
    }

    #[test]
    fn corrupt_ownership_fails_closed_without_touching_user_config() {
        let h = TempHome::new();
        let claude = h.path().join(".claude");
        let state = claude.join("codecli-installer");
        std::fs::create_dir_all(&state).unwrap();
        let settings = claude.join("settings.json");
        let original = r#"{"hooks":{"keep":true},"env":{"USER_VALUE":"keep"}}"#;
        std::fs::write(&settings, original).unwrap();
        std::fs::write(state.join("ownership.json"), "{broken-json").unwrap();

        let error = clear_config_sync("both".into()).unwrap_err();
        assert!(error.contains("ownership.json 损坏"));
        assert_eq!(std::fs::read_to_string(settings).unwrap(), original);
    }

    #[test]
    fn ownership_backup_path_outside_expected_directory_is_rejected() {
        let _h = TempHome::new();
        let mut rec = OwnershipRecord {
            settings_baseline_bak: Some("/tmp/not-a-codecli-settings-backup".into()),
            ..OwnershipRecord::default()
        };
        rec.settings_env_prev
            .insert("ANTHROPIC_API_KEY".into(), None);
        save_ownership(&rec).unwrap();

        let error = clear_config_sync("claude".into()).unwrap_err();
        assert!(error.contains("备份路径越界"));
    }

    #[test]
    fn ownership_commit_failure_removes_new_plaintext_tx_and_restores_old_record() {
        let h = TempHome::new();
        let claude = h.path().join(".claude");
        std::fs::create_dir_all(&claude).unwrap();
        let settings = claude.join("settings.json");
        std::fs::write(&settings, r#"{"hooks":{"keep":true}}"#).unwrap();

        write_claude_config(
            "https://api.deepseek.com/anthropic",
            "sk-first-plaintext-must-not-leak",
            "model-a",
        )
        .unwrap();
        let before = load_ownership().unwrap();
        let before_tx = before.settings_tx_bak.clone();
        let backup_count_before = std::fs::read_dir(&claude)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("settings.json.codecli-")
            })
            .count();

        // 第 1 次 save 登记新路径后，第 2 次（提交 active tx）注入失败。
        TEST_FAIL_OWNERSHIP_SAVE_AFTER.with(|slot| slot.set(Some(1)));
        let error = write_claude_config(
            "https://api.deepseek.com/anthropic",
            "sk-second-never-written",
            "model-b",
        )
        .unwrap_err();
        assert!(error.contains("提交 settings 备份 ownership"), "{error}");

        let after = load_ownership().unwrap();
        assert_eq!(after.settings_tx_bak, before_tx);
        assert!(after.pending_backup_cleanup.is_empty());
        let backup_files: Vec<_> = std::fs::read_dir(&claude)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("settings.json.codecli-")
            })
            .collect();
        assert_eq!(backup_files.len(), backup_count_before);
        for entry in backup_files {
            let raw = std::fs::read_to_string(entry.path()).unwrap();
            assert!(
                !raw.contains("sk-first-plaintext-must-not-leak"),
                "失败的新 tx 不得留下旧明文 Key: {}",
                entry.path().display()
            );
        }
        assert!(
            std::fs::read_to_string(settings)
                .unwrap()
                .contains("sk-first-plaintext-must-not-leak"),
            "提交 ownership 失败时不得改写主配置"
        );
    }

    #[test]
    fn old_tx_delete_failure_is_reported_and_remains_durably_tracked() {
        let h = TempHome::new();
        let claude = h.path().join(".claude");
        std::fs::create_dir_all(&claude).unwrap();
        let old_tx = claude.join("settings.json.codecli-tx.injected");
        std::fs::write(&old_tx, r#"{"env":{"ANTHROPIC_API_KEY":"old-secret"}}"#).unwrap();

        let mut rec = OwnershipRecord {
            settings_tx_bak: Some(BASELINE_ABSENT.into()),
            pending_backup_cleanup: vec![old_tx.display().to_string()],
            ..OwnershipRecord::default()
        };
        save_ownership(&rec).unwrap();
        TEST_FAIL_REMOVE_PATH.with(|slot| *slot.borrow_mut() = Some(old_tx.clone()));
        let error = gc_old_tx(
            &mut rec,
            &Some(old_tx.display().to_string()),
            BASELINE_ABSENT,
        )
        .unwrap_err();
        TEST_FAIL_REMOVE_PATH.with(|slot| *slot.borrow_mut() = None);

        assert!(error.contains("删除旧 tx 备份失败"), "{error}");
        assert!(old_tx.exists(), "注入删除失败后文件应仍在");
        let durable = load_ownership().unwrap();
        assert!(
            durable
                .pending_backup_cleanup
                .contains(&old_tx.display().to_string()),
            "删除失败时必须保留 durable 跟踪"
        );
    }

    #[cfg(unix)]
    #[test]
    fn plaintext_backup_is_private_from_creation() {
        use std::os::unix::fs::PermissionsExt;

        let h = TempHome::new();
        let source = h.path().join("settings.json");
        let backup = h.path().join("settings.json.codecli-tx.private");
        std::fs::write(&source, r#"{"apiKey":"old-plaintext-key"}"#).unwrap();
        std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o644)).unwrap();

        copy_private_text_backup(&source, &backup, "test").expect("含 Key 的备份应以私有模式创建");
        assert_eq!(
            std::fs::metadata(&backup).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(std::fs::read_to_string(backup)
            .unwrap()
            .contains("old-plaintext-key"));
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_state_directory_is_rejected() {
        use std::os::unix::fs::symlink;

        let h = TempHome::new();
        let claude = h.path().join(".claude");
        let outside = h.path().join("outside-state");
        std::fs::create_dir_all(&claude).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        symlink(&outside, claude.join("codecli-installer")).unwrap();

        let error = clear_config_sync("both".into()).unwrap_err();
        assert!(error.contains("符号链接"));
        assert!(outside.exists(), "拒绝操作时不得删除链接目标");
    }

    #[cfg(unix)]
    #[test]
    fn top_level_claude_and_codex_config_symlinks_are_not_replaced() {
        use std::os::unix::fs::symlink;

        let h = TempHome::new();
        let claude = h.path().join(".claude");
        let codex = h.path().join(".codex");
        std::fs::create_dir_all(&claude).unwrap();
        std::fs::create_dir_all(&codex).unwrap();
        let claude_target = h.path().join("dotfiles-claude.json");
        let codex_target = h.path().join("dotfiles-codex.toml");
        std::fs::write(&claude_target, "{}\n").unwrap();
        std::fs::write(&codex_target, "model = 'user-model'\n").unwrap();
        let claude_link = claude.join("settings.json");
        let codex_link = codex.join("config.toml");
        symlink(&claude_target, &claude_link).unwrap();
        symlink(&codex_target, &codex_link).unwrap();

        let claude_error = write_claude_config(
            "https://api.deepseek.com/anthropic",
            "sk-symlink-claude-123456",
            "model-a",
        )
        .unwrap_err();
        let codex_error = write_codex_config(
            "https://dashscope.aliyuncs.com/api/v2/apps/protocols/compatible-mode/v1",
            "sk-symlink-codex-123456",
            "qwen3.7-plus",
        )
        .unwrap_err();
        assert!(claude_error.contains("符号链接"), "{claude_error}");
        assert!(codex_error.contains("符号链接"), "{codex_error}");
        assert!(std::fs::symlink_metadata(&claude_link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(std::fs::symlink_metadata(&codex_link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(std::fs::read_to_string(&claude_target).unwrap(), "{}\n");
        assert_eq!(
            std::fs::read_to_string(&codex_target).unwrap(),
            "model = 'user-model'\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn scheme_apply_rejects_config_symlink_before_creating_journal() {
        use std::os::unix::fs::symlink;

        let h = TempHome::new();
        let claude = h.path().join(".claude");
        std::fs::create_dir_all(&claude).unwrap();
        let target = h.path().join("dotfiles-scheme-apply.json");
        std::fs::write(&target, "{}\n").unwrap();
        let logical = claude.join("settings.json");
        symlink(&target, &logical).unwrap();

        let error = apply_config_sync(ConfigApplyRequest {
            provider_id: "custom-anthropic".into(),
            api_key: "sk-symlink-preflight-123456".into(),
            base_url: Some("https://example.com/anthropic".into()),
            model: Some("model-a".into()),
            target: "claude".into(),
        })
        .unwrap_err();

        assert!(error.contains("符号链接"), "{error}");
        assert!(
            !h.path()
                .join(".claude/codecli-installer/schemes.tx.json")
                .exists(),
            "配置预检失败时不得创建方案事务日志"
        );
        assert!(std::fs::symlink_metadata(&logical)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(std::fs::read_to_string(target).unwrap(), "{}\n");
    }

    #[cfg(unix)]
    #[test]
    fn clear_rejects_new_top_level_config_symlink_without_unlinking_it() {
        use std::os::unix::fs::symlink;

        let h = TempHome::new();
        write_claude_config(
            "https://api.deepseek.com/anthropic",
            "sk-before-clear-symlink-123456",
            "model-a",
        )
        .unwrap();
        let logical = h.path().join(".claude/settings.json");
        std::fs::remove_file(&logical).unwrap();
        let target = h.path().join("dotfiles-clear-target.json");
        std::fs::write(&target, "{\"user\":true}\n").unwrap();
        symlink(&target, &logical).unwrap();

        let error = clear_config_sync("claude".into()).unwrap_err();
        assert!(error.contains("符号链接"), "{error}");
        assert!(std::fs::symlink_metadata(&logical)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(
            std::fs::read_to_string(target).unwrap(),
            "{\"user\":true}\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn state_directory_same_path_replacement_is_not_deleted() {
        let h = TempHome::new();
        let claude = h.path().join(".claude");
        let state = claude.join("codecli-installer");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::write(state.join("original"), "original").unwrap();
        let original_identity = state_dir_identity(&state).unwrap();

        let moved_original = claude.join("old-state");
        std::fs::rename(&state, &moved_original).unwrap();
        std::fs::create_dir(&state).unwrap();
        std::fs::write(state.join("replacement"), "must-survive").unwrap();

        let error = remove_state_dir_with_identity(&state, &original_identity)
            .expect_err("同路径替换目录必须 fail closed");
        assert!(error.contains("同路径替换"), "{error}");
        assert_eq!(
            std::fs::read_to_string(state.join("replacement")).unwrap(),
            "must-survive"
        );
        assert!(moved_original.join("original").exists());
    }

    #[cfg(unix)]
    #[test]
    fn matching_state_directory_is_quarantined_then_removed() {
        let h = TempHome::new();
        let state = h.path().join(".claude/codecli-installer");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::write(state.join("owned"), "data").unwrap();
        let identity = state_dir_identity(&state).unwrap();

        remove_state_dir_with_identity(&state, &identity).unwrap();
        assert!(!state.exists());
        let parent = state.parent().unwrap();
        assert!(std::fs::read_dir(parent).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".codecli-installer.purge-")
        }));
    }

    #[cfg(unix)]
    #[test]
    fn prepared_purge_marker_recovers_crash_after_rename() {
        let h = TempHome::new();
        let state = h.path().join(".claude/codecli-installer");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::write(state.join("secret-backup"), "must-be-removed").unwrap();
        let identity = state_dir_identity(&state).unwrap();
        let marker = StateDirPurgeMarker {
            version: STATE_PURGE_MARKER_VERSION,
            phase: StateDirPurgePhase::Prepared,
            quarantine_name: ".codecli-installer.purge-injected-prepared".into(),
            expected_identity: identity,
        };
        let quarantine = state_dir_quarantine_path(&marker).unwrap();
        save_state_dir_purge_marker(&marker).unwrap();
        std::fs::rename(&state, &quarantine).unwrap();

        recover_pending_state_dir_purge().unwrap();
        assert!(!quarantine.exists());
        assert!(!state_dir_purge_marker_path().unwrap().exists());
    }

    #[cfg(unix)]
    #[test]
    fn ordinary_operation_recovers_prepared_purge_before_writing_state() {
        let h = TempHome::new();
        let state = h.path().join(".claude/codecli-installer");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::write(state.join("old-ownership"), "old").unwrap();
        let marker = StateDirPurgeMarker {
            version: STATE_PURGE_MARKER_VERSION,
            phase: StateDirPurgePhase::Prepared,
            quarantine_name: ".codecli-installer.purge-before-ordinary-op".into(),
            expected_identity: state_dir_identity(&state).unwrap(),
        };
        save_state_dir_purge_marker(&marker).unwrap();

        super::super::op_lock::with_op_lock(|| {
            assert!(!state.exists(), "公共操作闭包前必须先完成旧 purge");
            std::fs::create_dir_all(&state).unwrap();
            std::fs::write(state.join("new-ownership"), "new").unwrap();
            Ok(())
        })
        .unwrap();

        assert_eq!(
            std::fs::read_to_string(state.join("new-ownership")).unwrap(),
            "new"
        );
        assert!(!state.join("old-ownership").exists());
    }

    #[cfg(unix)]
    #[test]
    fn quarantined_marker_never_deletes_new_original_path() {
        let h = TempHome::new();
        let state = h.path().join(".claude/codecli-installer");
        std::fs::create_dir_all(&state).unwrap();
        let identity = state_dir_identity(&state).unwrap();
        let marker = StateDirPurgeMarker {
            version: STATE_PURGE_MARKER_VERSION,
            phase: StateDirPurgePhase::Quarantined,
            quarantine_name: ".codecli-installer.purge-injected-complete".into(),
            expected_identity: identity,
        };
        let quarantine = state_dir_quarantine_path(&marker).unwrap();
        save_state_dir_purge_marker(&marker).unwrap();
        std::fs::rename(&state, &quarantine).unwrap();
        std::fs::remove_dir_all(&quarantine).unwrap();
        std::fs::create_dir(&state).unwrap();
        std::fs::write(state.join("replacement"), "must-survive").unwrap();

        recover_pending_state_dir_purge().unwrap();
        assert_eq!(
            std::fs::read_to_string(state.join("replacement")).unwrap(),
            "must-survive"
        );
        assert!(!state_dir_purge_marker_path().unwrap().exists());
    }
}
