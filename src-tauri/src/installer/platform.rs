// SPDX-License-Identifier: MPL-2.0
//! 跨平台路径 / shell / 环境变量（Key 进 0600 secrets 文件）

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use super::util::{shell_single_quote, validate_env_key, validate_env_value};

static PROFILE_TMP_COUNTER: AtomicU64 = AtomicU64::new(0);
const MAX_SECRETS_ENV_BYTES: u64 = 1024 * 1024;
const MAX_PROFILE_BYTES: u64 = 4 * 1024 * 1024;

/// 一次 profile 操作的固定目标。顶层符号链接只在这里解析一次，
/// 后续读、写与回滚都使用同一个 target，不会用 rename 替换链接本身。
#[derive(Debug)]
struct ProfileSnapshot {
    logical_path: PathBuf,
    target_path: PathBuf,
    previous: Option<String>,
    previous_permissions: Option<std::fs::Permissions>,
}

fn load_profile_snapshot(path: &Path) -> Result<ProfileSnapshot, String> {
    let entry = match std::fs::symlink_metadata(path) {
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(format!("检查 profile {} 失败: {error}", path.display())),
    };

    let Some(entry) = entry else {
        return Ok(ProfileSnapshot {
            logical_path: path.to_path_buf(),
            target_path: path.to_path_buf(),
            previous: None,
            previous_permissions: None,
        });
    };

    let target = if entry.file_type().is_symlink() {
        let raw_target = std::fs::read_link(path)
            .map_err(|error| format!("读取 profile 链接 {} 失败: {error}", path.display()))?;
        let joined = if raw_target.is_absolute() {
            raw_target
        } else {
            path.parent()
                .ok_or_else(|| format!("profile 链接没有父目录: {}", path.display()))?
                .join(raw_target)
        };
        std::fs::canonicalize(&joined).map_err(|error| {
            format!(
                "profile 链接 {} 的目标无效或已断开: {error}",
                path.display()
            )
        })?
    } else {
        path.to_path_buf()
    };

    let metadata = std::fs::metadata(&target)
        .map_err(|error| format!("检查 profile 目标 {} 失败: {error}", target.display()))?;
    if !metadata.is_file() {
        return Err(format!(
            "profile {} 不是可信普通文件，已拒绝改写",
            path.display()
        ));
    }
    if metadata.len() > MAX_PROFILE_BYTES {
        return Err(format!(
            "profile {} 超过 4 MiB，已拒绝建立改写快照",
            path.display()
        ));
    }
    let previous = std::fs::read_to_string(&target)
        .map_err(|error| format!("读取 profile {} 失败: {error}", target.display()))?;
    Ok(ProfileSnapshot {
        logical_path: path.to_path_buf(),
        target_path: target,
        previous: Some(previous),
        previous_permissions: Some(metadata.permissions()),
    })
}

/// 写前 CAS：重新解析逻辑 profile，要求它仍指向同一固定目标，
/// 且内容与调用方期待完全一致。这使编辑器/其它安装器在我们
/// 快照后保存时 fail-closed，而不是用旧快照静默覆盖。
fn ensure_profile_current(
    snapshot: &ProfileSnapshot,
    expected_content: Option<&str>,
) -> Result<(), String> {
    let current = load_profile_snapshot(&snapshot.logical_path)?;
    if current.target_path != snapshot.target_path {
        return Err(format!(
            "profile {} 在操作期间改变了链接/目标，已拒绝覆盖",
            snapshot.logical_path.display()
        ));
    }
    if current.previous.as_deref() != expected_content {
        return Err(format!(
            "profile {} 在操作期间被其它程序修改，已拒绝覆盖",
            snapshot.logical_path.display()
        ));
    }

    // 同时保护用户并发 chmod/只读属性修改。新建文件的快照
    // 没有 previous_permissions，此时仅由内容 CAS 决定。
    if let (Some(expected), Some(actual)) = (
        snapshot.previous_permissions.as_ref(),
        current.previous_permissions.as_ref(),
    ) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if expected.mode() != actual.mode() {
                return Err(format!(
                    "profile {} 在操作期间权限已改变，已拒绝覆盖",
                    snapshot.logical_path.display()
                ));
            }
        }
        #[cfg(not(unix))]
        if expected.readonly() != actual.readonly() {
            return Err(format!(
                "profile {} 在操作期间只读属性已改变，已拒绝覆盖",
                snapshot.logical_path.display()
            ));
        }
    }
    Ok(())
}

fn profile_entry_present(path: &Path) -> Result<bool, String> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!("检查 profile {} 失败: {error}", path.display())),
    }
}

fn durable_write_profile_expected(
    snapshot: &ProfileSnapshot,
    expected_current: Option<&str>,
    content: &str,
) -> Result<(), String> {
    ensure_profile_current(snapshot, expected_current)?;
    let target = &snapshot.target_path;
    let parent = target
        .parent()
        .ok_or_else(|| format!("profile 没有父目录: {}", target.display()))?;
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("创建 profile 目录 {} 失败: {error}", parent.display()))?;

    let name = target
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("shell-profile");
    let mut selected = None;
    for _ in 0..32 {
        let sequence = PROFILE_TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let candidate = parent.join(format!(
            ".{name}.codecli.tmp.{}.{}",
            std::process::id(),
            sequence
        ));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        match options.open(&candidate) {
            Ok(file) => {
                selected = Some((candidate, file));
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(format!(
                    "创建 profile 原子写临时文件 {} 失败: {error}",
                    candidate.display()
                ))
            }
        }
    }
    let (tmp, mut file) = selected.ok_or("创建 profile 原子写临时文件连续冲突")?;

    let prepare = (|| -> Result<(), String> {
        file.write_all(content.as_bytes())
            .map_err(|error| format!("写入 profile 临时文件失败: {error}"))?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = snapshot
                .previous_permissions
                .as_ref()
                .map(|permissions| permissions.mode() & 0o777)
                .unwrap_or(0o600);
            file.set_permissions(std::fs::Permissions::from_mode(mode))
                .map_err(|error| format!("设置 profile 临时文件权限失败: {error}"))?;
        }
        #[cfg(not(unix))]
        if let Some(permissions) = &snapshot.previous_permissions {
            file.set_permissions(permissions.clone())
                .map_err(|error| format!("保留 profile 权限失败: {error}"))?;
        }

        file.sync_all()
            .map_err(|error| format!("同步 profile 临时文件失败: {error}"))
    })();
    drop(file);
    if let Err(error) = prepare {
        let _ = std::fs::remove_file(&tmp);
        return Err(error);
    }

    // 临时文件准备期间用户仍可能保存 profile；在真正
    // replace 前再做一次 CAS，将 lost-update 窗口缩到最小。
    if let Err(error) = ensure_profile_current(snapshot, expected_current) {
        let _ = std::fs::remove_file(&tmp);
        return Err(error);
    }

    if let Err(error) = super::util::atomic_replace_file(&tmp, target) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!(
            "原子替换 profile {} 失败: {error}",
            snapshot.logical_path.display()
        ));
    }

    let metadata = std::fs::symlink_metadata(target)
        .map_err(|error| format!("复查 profile {} 失败: {error}", target.display()))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(format!(
            "profile {} 原子替换后不是普通文件",
            target.display()
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let expected = snapshot
            .previous_permissions
            .as_ref()
            .map(|permissions| permissions.mode() & 0o777)
            .unwrap_or(0o600);
        let actual = metadata.permissions().mode() & 0o777;
        if actual != expected {
            return Err(format!(
                "profile {} 权限复查失败: {actual:03o}，期望 {expected:03o}",
                target.display()
            ));
        }
    }
    Ok(())
}

fn durable_write_profile(snapshot: &ProfileSnapshot, content: &str) -> Result<(), String> {
    durable_write_profile_expected(snapshot, snapshot.previous.as_deref(), content)
}

fn durable_remove_created_profile_expected(
    snapshot: &ProfileSnapshot,
    expected_current: &str,
) -> Result<(), String> {
    ensure_profile_current(snapshot, Some(expected_current))?;
    match std::fs::symlink_metadata(&snapshot.target_path) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
            super::util::remove_file_durable(&snapshot.target_path).map_err(|error| {
                format!(
                    "回滚删除 profile {} 失败: {error}",
                    snapshot.logical_path.display()
                )
            })
        }
        Ok(_) => Err(format!(
            "回滚时 profile {} 已变为非普通文件，已拒绝删除",
            snapshot.logical_path.display()
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!(
            "回滚检查 profile {} 失败: {error}",
            snapshot.logical_path.display()
        )),
    }
}

/// 事务回滚也使用 CAS：只有当当前内容仍是本次计划写入值
/// 时才恢复旧值/删除新建文件。如果编辑器在我们写入后又保存，
/// 回滚必须保留用户新内容并报错。
fn rollback_profile_if_ours(
    snapshot: &ProfileSnapshot,
    attempted_content: &str,
) -> Result<(), String> {
    let current = load_profile_snapshot(&snapshot.logical_path)?;
    if current.target_path != snapshot.target_path {
        return Err(format!(
            "回滚时 profile {} 已改变链接/目标，已保留",
            snapshot.logical_path.display()
        ));
    }
    if current.previous == snapshot.previous {
        // 写入未提交，或已恢复到原快照。
        return Ok(());
    }
    if current.previous.as_deref() != Some(attempted_content) {
        return Err(format!(
            "回滚时 profile {} 已被其它程序修改，已保留并拒绝覆盖",
            snapshot.logical_path.display()
        ));
    }
    match snapshot.previous.as_deref() {
        Some(previous) => {
            durable_write_profile_expected(snapshot, Some(attempted_content), previous)
        }
        None => durable_remove_created_profile_expected(snapshot, attempted_content),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OsKind {
    Windows,
    Macos,
    Linux,
    Unknown,
}

pub fn os_kind() -> OsKind {
    if cfg!(target_os = "windows") {
        OsKind::Windows
    } else if cfg!(target_os = "macos") {
        OsKind::Macos
    } else if cfg!(target_os = "linux") {
        OsKind::Linux
    } else {
        OsKind::Unknown
    }
}

pub fn os_display_name() -> String {
    match os_kind() {
        OsKind::Windows => "Windows".to_string(),
        OsKind::Macos => {
            if std::env::consts::ARCH == "aarch64" {
                "macOS (Apple Silicon)".to_string()
            } else {
                "macOS (Intel)".to_string()
            }
        }
        OsKind::Linux => "Linux".to_string(),
        OsKind::Unknown => "Unknown".to_string(),
    }
}

#[cfg(test)]
thread_local! {
    /// 测试不能修改进程级 HOME/USERPROFILE；Rust 默认并行
    /// 跑测试，其他模块会在窗口内误操作这个临时 HOME。
    static TEST_HOME_DIR: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn replace_test_home_dir(value: Option<PathBuf>) -> Option<PathBuf> {
    TEST_HOME_DIR.with(|slot| std::mem::replace(&mut *slot.borrow_mut(), value))
}

pub fn home_dir() -> Option<PathBuf> {
    #[cfg(test)]
    if let Some(path) = TEST_HOME_DIR.with(|slot| slot.borrow().clone()) {
        return Some(path);
    }
    if cfg!(target_os = "windows") {
        std::env::var_os("USERPROFILE")
            .or_else(|| std::env::var_os("HOME"))
            .map(PathBuf::from)
    } else {
        std::env::var_os("HOME").map(PathBuf::from)
    }
}

pub fn claude_config_dir() -> Option<PathBuf> {
    home_dir().map(|h| h.join(".claude"))
}

pub fn codex_config_dir() -> Option<PathBuf> {
    home_dir().map(|h| h.join(".codex"))
}

pub fn codex_config_toml() -> Option<PathBuf> {
    codex_config_dir().map(|d| d.join("config.toml"))
}

/// 工具状态目录
pub fn codecli_state_dir() -> Option<PathBuf> {
    claude_config_dir().map(|d| d.join("codecli-installer"))
}

/// 0600 secrets env 文件（KEY=value 每行）
pub fn secrets_env_path() -> Option<PathBuf> {
    codecli_state_dir().map(|d| d.join("secrets.env"))
}

pub(crate) fn which_cmd_candidates(bin: &str) -> Vec<String> {
    if !bin
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        return Vec::new();
    }
    let output = if cfg!(target_os = "windows") {
        let mut command = Command::new("where");
        command.arg(bin);
        // Windows where 会扫描 PATH（PATH 可包含断开的网络盘）；
        // 不允许它在所有受控命令之前无限挂起。
        super::cmd::run_timed(command, 10).ok()
    } else {
        // 使用系统绝对 sh，避免 PATH 中同名 shim 抢占探测命令；
        // bin 已经上面的 ASCII 白名单校验，不能注入 shell 语法。
        let mut command = Command::new("/bin/sh");
        command.args(["-c", &format!("command -v {}", bin)]);
        super::cmd::run_timed(command, 10).ok()
    };
    let Some(output) = output else {
        return Vec::new();
    };
    if !output.status_ok {
        return Vec::new();
    }
    output
        .stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

pub fn which_cmd(bin: &str) -> Option<String> {
    which_cmd_candidates(bin).into_iter().next()
}

fn is_secret_key(key: &str) -> bool {
    key.contains("KEY") || key.contains("TOKEN") || key.contains("SECRET")
}

/// 事务快照专用：只有“路径/注册表项明确不存在或确实没有该键”才返回
/// None；权限、损坏、链接和解析错误一律上抛，禁止把 unreadable 误记成
/// absent 后在回滚时删除用户原有 Key。
pub(crate) fn get_persistent_env_strict(key: &str) -> Result<Option<String>, String> {
    validate_env_key(key)?;
    let value = if cfg!(target_os = "windows") {
        get_user_env_windows_strict(key)?
    } else {
        let home = home_dir().ok_or("找不到 HOME，无法建立持久环境事务快照")?;
        get_persistent_env_unix_from_home_strict(&home, key)?
    };
    if let Some(value) = value.as_deref() {
        if is_secret_key(key) {
            super::util::validate_secret_value(key, value)?;
        } else {
            validate_env_value(key, value)?;
        }
    }
    Ok(value)
}

fn get_persistent_env_unix_from_home_strict(
    home: &Path,
    key: &str,
) -> Result<Option<String>, String> {
    if is_secret_key(key) {
        let path = home
            .join(".claude")
            .join("codecli-installer")
            .join("secrets.env");
        return read_secret_from_path_strict(&path, key);
    }

    let mut found: Option<String> = None;
    for name in [".zprofile", ".zshrc", ".bash_profile", ".bashrc"] {
        let snapshot = load_profile_snapshot(&home.join(name))?;
        let Some(raw) = snapshot.previous.as_deref() else {
            continue;
        };
        if let Some(value) = parse_managed_env_export_block_strict(raw, key)? {
            if found
                .as_deref()
                .is_some_and(|previous| previous != value.as_str())
            {
                return Err(format!(
                    "持久化环境变量 {key} 在多个 profile 中值不一致，已拒绝建立事务快照"
                ));
            }
            found = Some(value);
        }
    }
    Ok(found)
}

fn parse_managed_env_export_block_strict(raw: &str, key: &str) -> Result<Option<String>, String> {
    let spans = managed_block_spans(raw, key)
        .map_err(|error| format!("CodeCLI {key} profile 标记块{error}"))?;
    if spans.is_empty() {
        return Ok(None);
    }
    if spans.len() != 1 {
        return Err(format!(
            "CodeCLI {key} profile 含重复标记块，已拒绝建立事务快照"
        ));
    }
    let span = spans[0];
    let mut found = None;
    for line in raw[span.body_start..span.body_end].lines() {
        if line.trim().is_empty() {
            continue;
        }
        let Some(assignment) = line.trim().strip_prefix("export ") else {
            return Err(format!("CodeCLI {key} profile 标记块含非预期内容"));
        };
        let (found_key, value) = super::util::parse_secret_line(assignment)
            .ok_or_else(|| format!("CodeCLI {key} profile export 无法解析"))?;
        if found_key != key {
            return Err(format!("CodeCLI {key} profile 标记块含其它 export"));
        }
        if found.replace(value).is_some() {
            return Err(format!("CodeCLI {key} profile 标记块含重复 export"));
        }
    }
    found
        .map(Some)
        .ok_or_else(|| format!("CodeCLI {key} profile 标记块缺少对应 export"))
}

fn read_secret_from_path_strict(path: &Path, key: &str) -> Result<Option<String>, String> {
    Ok(read_secret_map_from_path_strict(path)?.and_then(|map| map.get(key).cloned()))
}

fn read_secret_map_from_path_strict(
    path: &Path,
) -> Result<Option<std::collections::BTreeMap<String, String>>, String> {
    if let Some(parent) = path.parent() {
        match std::fs::symlink_metadata(parent) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err("secrets.env 状态目录不是可信实体目录".into())
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("检查 secrets.env 状态目录失败: {error}")),
        }
    }
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("检查 secrets.env 失败: {error}")),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err("secrets.env 不是可信普通文件，已拒绝建立事务快照".into());
    }
    if metadata.len() > MAX_SECRETS_ENV_BYTES {
        return Err("secrets.env 超过 1 MiB，已拒绝建立事务快照".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err("secrets.env 对组/其他用户可见，已拒绝读取 Key".into());
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
        .open(path)
        .map_err(|error| format!("安全打开 secrets.env 失败: {error}"))?;
    let opened = file
        .metadata()
        .map_err(|error| format!("读取 secrets.env 元数据失败: {error}"))?;
    if !opened.is_file() || opened.len() > MAX_SECRETS_ENV_BYTES {
        return Err("secrets.env 打开后不是可信普通小文件".into());
    }
    let mut bytes = Vec::with_capacity(opened.len() as usize);
    std::io::Read::by_ref(&mut file)
        .take(MAX_SECRETS_ENV_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("读取 secrets.env 失败: {error}"))?;
    if bytes.len() as u64 > MAX_SECRETS_ENV_BYTES {
        return Err("secrets.env 读取期间变大，已拒绝".into());
    }
    let raw =
        std::str::from_utf8(&bytes).map_err(|error| format!("secrets.env 不是 UTF-8: {error}"))?;
    let mut map = std::collections::BTreeMap::new();
    for (line_number, line) in raw.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let (found_key, value) = super::util::parse_secret_line(trimmed)
            .ok_or_else(|| format!("secrets.env 第 {} 行无法解析", line_number + 1))?;
        validate_env_key(&found_key)?;
        super::util::validate_secret_value(&found_key, &value)?;
        if map.insert(found_key.clone(), value).is_some() {
            return Err(format!("secrets.env 含重复键 {found_key}"));
        }
    }
    Ok(Some(map))
}

fn write_secret_to_file(key: &str, value: &str) -> Result<(), String> {
    use super::util::{format_secret_line, validate_secret_value};
    validate_secret_value(key, value)?;
    let path = secrets_env_path().ok_or("找不到 secrets 路径")?;
    let mut map = read_secret_map_from_path_strict(&path)?.unwrap_or_default();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    map.insert(key.to_string(), value.to_string());
    let mut body = String::from(
        "# managed by codecli-installer — mode 0600\n\
         # values are single-quoted for safe shell source\n",
    );
    for (k, v) in &map {
        body.push_str(&format_secret_line(k, v)?);
        body.push('\n');
    }
    super::util::atomic_write_mode(&path, &body, true)?;
    ensure_source_block_in_profiles(&path)?;
    Ok(())
}

fn remove_secret_from_file(key: &str) -> Result<(), String> {
    use super::util::format_secret_line;
    let Some(path) = secrets_env_path() else {
        return Ok(());
    };
    let Some(mut map) = read_secret_map_from_path_strict(&path)? else {
        remove_source_block_from_profiles()?;
        return Ok(());
    };
    let removed = map.remove(key);
    if removed.is_none() && !map.is_empty() {
        return Ok(());
    }
    if map.is_empty() {
        super::util::remove_file_durable(&path)
            .map_err(|error| format!("持久删除 secrets.env 失败: {error}"))?;
        remove_source_block_from_profiles()?;
        return Ok(());
    }
    let mut body = String::from(
        "# managed by codecli-installer — mode 0600\n\
         # values are single-quoted for safe shell source\n",
    );
    for (k, v) in &map {
        body.push_str(&format_secret_line(k, v)?);
        body.push('\n');
    }
    super::util::atomic_write_mode(&path, &body, true)
}

fn remove_source_block_from_profiles() -> Result<(), String> {
    let home = home_dir().ok_or("找不到 HOME")?;
    remove_source_blocks_from_home(&home)
}

fn remove_source_blocks_from_home(home: &Path) -> Result<(), String> {
    let mut errors = Vec::new();
    for name in [".zprofile", ".zshrc", ".bash_profile", ".bashrc"] {
        let profile = home.join(name);
        if let Err(error) = remove_fixed_block(&profile, "secrets") {
            errors.push(error);
        }
    }
    if !errors.is_empty() {
        return Err(format!(
            "移除 secrets source 块未完成（可重试）: {}",
            errors.join("; ")
        ));
    }
    Ok(())
}

/// 完全卸载时移除本工具自己写入的固定 source 块。
/// 不解析、不删除用户其它 export；Windows 无该块。
pub(crate) fn remove_tool_secret_source_block() -> Result<(), String> {
    if cfg!(target_os = "windows") {
        Ok(())
    } else {
        remove_source_block_from_profiles()
    }
}

fn remove_fixed_block(path: &Path, tag: &str) -> Result<(), String> {
    let snapshot = load_profile_snapshot(path)?;
    let Some(raw) = snapshot.previous.as_deref() else {
        return Ok(());
    };
    let body = remove_fixed_blocks_content(raw, tag)
        .map_err(|error| format!("{} 中的 CodeCLI 标记块{error}", path.display()))?;
    if body == raw {
        return Ok(());
    }
    durable_write_profile(&snapshot, &body)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ManagedBlockSpan {
    start: usize,
    body_start: usize,
    body_end: usize,
    /// 包含 end marker 行自己的换行符（若存在）。
    end: usize,
}

/// 严格按“marker 独占整行”解析管理块。普通命令里 echo/引用 marker 文本
/// 不属于管理块，绝不能因为 substring 命中而删除。嵌套、孤立、逆序 marker
/// 一律 fail-closed。
fn managed_block_spans(raw: &str, tag: &str) -> Result<Vec<ManagedBlockSpan>, String> {
    let begin = format!("# >>> codecli-installer {tag} >>>");
    let end = format!("# <<< codecli-installer {tag} <<<");
    let mut spans = Vec::new();
    let mut opened: Option<(usize, usize)> = None;
    let mut cursor = 0usize;

    for segment in raw.split_inclusive('\n') {
        let line_end = cursor + segment.len();
        let line = segment
            .strip_suffix('\n')
            .unwrap_or(segment)
            .strip_suffix('\r')
            .unwrap_or_else(|| segment.strip_suffix('\n').unwrap_or(segment));
        if line == begin {
            if opened.is_some() {
                return Err("嵌套或重复起始 marker，已拒绝改写".into());
            }
            opened = Some((cursor, line_end));
        } else if line == end {
            let Some((start, body_start)) = opened.take() else {
                return Err("存在孤立或逆序结束 marker，已拒绝改写".into());
            };
            spans.push(ManagedBlockSpan {
                start,
                body_start,
                body_end: cursor,
                end: line_end,
            });
        }
        cursor = line_end;
    }
    if opened.is_some() {
        return Err("不完整，已拒绝改写".into());
    }
    Ok(spans)
}

/// 移除所有完整的工具标记块，不 trim 或重排任何用户内容。
/// 只要出现孤立/逆序 marker 就 fail-closed，避免局部改写损坏 profile。
fn remove_fixed_blocks_content(raw: &str, tag: &str) -> Result<String, String> {
    let spans = managed_block_spans(raw, tag)?;
    if spans.is_empty() {
        return Ok(raw.to_string());
    }
    let mut out = String::with_capacity(raw.len());
    let mut cursor = 0usize;
    for span in spans {
        out.push_str(&raw[cursor..span.start]);
        cursor = span.end;
    }
    out.push_str(&raw[cursor..]);
    Ok(out)
}

/// profile 里只写固定 source 块；secrets 文件内值为单引号包裹
fn ensure_source_block_in_profiles(secrets_path: &Path) -> Result<(), String> {
    let home = home_dir().ok_or("找不到 HOME")?;
    let path_q = shell_single_quote(&secrets_path.display().to_string());
    // set -a 导出；文件内 KEY='value' 单引号不展开 $()
    let block = format!(
        "# >>> codecli-installer secrets >>>\n\
         [ -f {p} ] && set -a && . {p} && set +a\n\
         # <<< codecli-installer secrets <<<",
        p = path_q
    );
    for name in [".zprofile", ".zshrc"] {
        let profile = home.join(name);
        upsert_fixed_block(&profile, "secrets", &block)?;
    }
    let bash = home.join(".bash_profile");
    if profile_entry_present(&bash)? {
        upsert_fixed_block(&bash, "secrets", &block)?;
    }
    Ok(())
}

fn upsert_fixed_block(path: &Path, tag: &str, block: &str) -> Result<(), String> {
    let snapshot = load_profile_snapshot(path)?;
    upsert_fixed_block_from_snapshot(&snapshot, tag, block)
}

fn upsert_fixed_block_from_snapshot(
    snapshot: &ProfileSnapshot,
    tag: &str,
    block: &str,
) -> Result<(), String> {
    let new_content = fixed_block_content_from_snapshot(snapshot, tag, block)?;
    durable_write_profile(snapshot, &new_content)
}

fn fixed_block_content_from_snapshot(
    snapshot: &ProfileSnapshot,
    tag: &str,
    block: &str,
) -> Result<String, String> {
    let existing = snapshot.previous.as_deref().unwrap_or_default();
    let mut new_content = remove_fixed_blocks_content(existing, tag)?;
    if !new_content.is_empty() && !new_content.ends_with('\n') {
        new_content.push('\n');
    }
    new_content.push_str(block);
    new_content.push('\n');
    Ok(new_content)
}

/// Node/npm 用户级回退专用 PATH 块。两个 profile 在动手前一次性
/// 建立严格快照；任一读取失败就不写，任一写入失败则持久化回滚。
pub(crate) fn ensure_tool_path_block(tag: &str, path: &Path) -> Result<(), String> {
    if cfg!(target_os = "windows") {
        return Ok(());
    }
    if !matches!(tag, "node-path" | "npm-prefix" | "extension-feishu") {
        return Err(format!("未知的 CodeCLI PATH 块: {tag}"));
    }
    let home = home_dir().ok_or("找不到 HOME")?;
    let targets = [home.join(".zprofile"), home.join(".zshrc")];
    let snapshots = targets
        .iter()
        .map(|target| load_profile_snapshot(target))
        .collect::<Result<Vec<_>, _>>()?;
    let line = format!(
        "export PATH={}:$PATH",
        shell_single_quote(&path.display().to_string())
    );
    let block =
        format!("# >>> codecli-installer {tag} >>>\n{line}\n# <<< codecli-installer {tag} <<<");

    // 所有目标先完成严格解析/生成，再开始任何写入。
    let planned = snapshots
        .iter()
        .map(|snapshot| fixed_block_content_from_snapshot(snapshot, tag, &block))
        .collect::<Result<Vec<_>, _>>()?;

    for (index, (snapshot, content)) in snapshots.iter().zip(planned.iter()).enumerate() {
        if let Err(error) = durable_write_profile(snapshot, content) {
            let mut rollback_errors = Vec::new();
            for attempted_index in (0..=index).rev() {
                let rollback = rollback_profile_if_ours(
                    &snapshots[attempted_index],
                    &planned[attempted_index],
                );
                if let Err(rollback_error) = rollback {
                    rollback_errors.push(rollback_error);
                }
            }
            if rollback_errors.is_empty() {
                return Err(format!("{error}（已回滚本次 PATH profile 改动）"));
            }
            return Err(format!(
                "{error}；PATH profile 回滚不完整（可重试）: {}",
                rollback_errors.join("; ")
            ));
        }
    }
    Ok(())
}

pub(crate) fn remove_tool_path_block(tag: &str) -> Result<Vec<String>, String> {
    if !matches!(tag, "node-path" | "npm-prefix" | "extension-feishu") {
        return Err(format!("未知的 CodeCLI PATH 块: {tag}"));
    }
    if cfg!(target_os = "windows") {
        return Ok(Vec::new());
    }
    let home = home_dir().ok_or("找不到 HOME")?;
    let mut removed = Vec::new();
    let mut errors = Vec::new();
    for name in [".zprofile", ".zshrc"] {
        let profile = home.join(name);
        match remove_fixed_block(&profile, tag) {
            Ok(()) => removed.push(format!("profile:{}:{tag}", profile.display())),
            Err(error) => errors.push(error),
        }
    }
    if errors.is_empty() {
        Ok(removed)
    } else {
        Err(format!(
            "清理 {tag} PATH 块未完成（可重试）: {}",
            errors.join("; ")
        ))
    }
}

pub(crate) fn remove_tool_runtime_path_blocks() -> Result<Vec<String>, String> {
    if cfg!(target_os = "windows") {
        return Ok(Vec::new());
    }
    let mut removed = Vec::new();
    let mut errors = Vec::new();
    for tag in ["node-path", "npm-prefix"] {
        match remove_tool_path_block(tag) {
            Ok(paths) => removed.extend(paths),
            Err(error) => errors.push(error),
        }
    }
    if errors.is_empty() {
        Ok(removed)
    } else {
        Err(format!(
            "清理 Node/npm PATH 块未完成（可重试）: {}",
            errors.join("; ")
        ))
    }
}

/// 非 secret 的普通 env（如 BASE_URL / MODEL）仍可写 profile export
fn set_plain_env_unix(key: &str, value: &str) -> Result<(), String> {
    let home = home_dir().ok_or("找不到 HOME")?;
    set_plain_env_unix_in_home(&home, key, value)
}

fn set_plain_env_unix_in_home(home: &Path, key: &str, value: &str) -> Result<(), String> {
    let targets = {
        let mut t = vec![home.join(".zprofile"), home.join(".zshrc")];
        let bash = home.join(".bash_profile");
        if profile_entry_present(&bash)? {
            t.push(bash);
        }
        t
    };
    let snapshots = targets
        .iter()
        .map(|path| load_profile_snapshot(path))
        .collect::<Result<Vec<_>, _>>()?;

    let planned = snapshots
        .iter()
        .map(|snapshot| env_export_content_from_snapshot(snapshot, key, value))
        .collect::<Result<Vec<_>, _>>()?;

    for (index, (snapshot, content)) in snapshots.iter().zip(planned.iter()).enumerate() {
        if let Err(error) = durable_write_profile(snapshot, content) {
            let mut rollback_errors = Vec::new();
            for attempted_index in (0..=index).rev() {
                let rollback = rollback_profile_if_ours(
                    &snapshots[attempted_index],
                    &planned[attempted_index],
                );
                if let Err(rollback_error) = rollback {
                    rollback_errors.push(rollback_error);
                }
            }
            if rollback_errors.is_empty() {
                return Err(format!("{error}（已持久化回滚本次 profile 改动）"));
            }
            return Err(format!(
                "{error}；profile 回滚不完整（可重试）: {}",
                rollback_errors.join("; ")
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
fn upsert_env_export_block(path: &Path, key: &str, value: &str) -> Result<(), String> {
    let snapshot = load_profile_snapshot(path)?;
    upsert_env_export_block_from_snapshot(&snapshot, key, value)
}

#[cfg(test)]
fn upsert_env_export_block_from_snapshot(
    snapshot: &ProfileSnapshot,
    key: &str,
    value: &str,
) -> Result<(), String> {
    let new_content = env_export_content_from_snapshot(snapshot, key, value)?;
    durable_write_profile(snapshot, &new_content)
}

fn env_export_content_from_snapshot(
    snapshot: &ProfileSnapshot,
    key: &str,
    value: &str,
) -> Result<String, String> {
    let begin = format!("# >>> codecli-installer {} >>>", key);
    let end = format!("# <<< codecli-installer {} <<<", key);
    let line = format!("export {}={}", key, shell_single_quote(value));
    let existing = snapshot.previous.as_deref().unwrap_or_default();
    let mut new_content = remove_fixed_blocks_content(existing, key)
        .map_err(|error| format!("CodeCLI {key} profile 标记块{error}"))?;
    if !new_content.is_empty() && !new_content.ends_with('\n') {
        new_content.push('\n');
    }
    new_content.push_str(&begin);
    new_content.push('\n');
    new_content.push_str(&line);
    new_content.push('\n');
    new_content.push_str(&end);
    new_content.push('\n');
    Ok(new_content)
}

fn remove_env_export_block(path: &Path, key: &str) -> Result<(), String> {
    let snapshot = load_profile_snapshot(path)?;
    let Some(existing) = snapshot.previous.as_deref() else {
        return Ok(());
    };
    let body = remove_fixed_blocks_content(existing, key)
        .map_err(|error| format!("{} 中的 CodeCLI {key} 标记块{error}", path.display()))?;
    if body == existing {
        return Ok(());
    }
    durable_write_profile(&snapshot, &body)
}

fn remove_env_export_blocks_from_home(home: &Path, key: &str) -> Result<(), String> {
    let mut errors = Vec::new();
    for name in [".zprofile", ".zshrc", ".bash_profile", ".bashrc"] {
        let profile = home.join(name);
        if let Err(error) = remove_env_export_block(&profile, key) {
            errors.push(error);
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

pub fn set_user_env(key: &str, value: &str) -> Result<(), String> {
    validate_env_key(key)?;
    if is_secret_key(key) {
        super::util::validate_secret_value(key, value)?;
    } else {
        validate_env_value(key, value)?;
    }

    if cfg!(target_os = "windows") {
        set_user_env_windows(key, value)?;
        unsafe { std::env::set_var(key, value) };
        return Ok(());
    }

    if is_secret_key(key) {
        write_secret_to_file(key, value)?;
    } else {
        set_plain_env_unix(key, value)?;
    }
    unsafe { std::env::set_var(key, value) };
    Ok(())
}

pub fn unset_user_env(key: &str) -> Result<(), String> {
    validate_env_key(key)?;
    if cfg!(target_os = "windows") {
        // 只有注册表删除成功（或本来不存在）才清理进程值；
        // 失败时保留进程值，让调用方可安全重试。
        unset_user_env_windows(key)?;
        unsafe { std::env::remove_var(key) };
        return Ok(());
    }

    let mut errors = Vec::new();
    if is_secret_key(key) {
        if let Err(error) = remove_secret_from_file(key) {
            errors.push(error);
        }
    }

    match home_dir() {
        Some(home) => {
            if let Err(error) = remove_env_export_blocks_from_home(&home, key) {
                errors.push(error);
            }
        }
        None => errors.push("找不到 HOME".into()),
    }

    if !errors.is_empty() {
        return Err(format!(
            "移除持久化环境变量 {key} 未完成（可重试）: {}",
            errors.join("; ")
        ));
    }

    unsafe { std::env::remove_var(key) };
    Ok(())
}

fn validate_user_path_segment(segment: &str) -> Result<&str, String> {
    let segment = segment.trim();
    if segment.is_empty() || segment.contains(';') || segment.contains(['\0', '\r', '\n']) {
        return Err("用户 PATH 片段无效".into());
    }
    Ok(segment)
}

pub(crate) fn add_user_path_segment_windows(segment: &str) -> Result<(), String> {
    let segment = validate_user_path_segment(segment)?;
    #[cfg(windows)]
    {
        update_user_path_segment_windows(segment, true)
    }
    #[cfg(not(windows))]
    {
        let _ = segment;
        Ok(())
    }
}

pub(crate) fn remove_user_path_segment_windows(segment: &str) -> Result<(), String> {
    let segment = validate_user_path_segment(segment)?;
    #[cfg(windows)]
    {
        update_user_path_segment_windows(segment, false)
    }
    #[cfg(not(windows))]
    {
        let _ = segment;
        Ok(())
    }
}

#[cfg(any(windows, test))]
fn windows_path_segment_matches(left: &str, right: &str) -> bool {
    let normalize = |value: &str| {
        value
            .trim()
            .trim_matches('"')
            .replace('/', "\\")
            .trim_end_matches('\\')
            .to_lowercase()
    };
    normalize(left) == normalize(right)
}

#[cfg(windows)]
fn decode_windows_registry_string(bytes: &[u8]) -> Result<String, String> {
    if !bytes.len().is_multiple_of(2) {
        return Err("用户 PATH 注册表字节长度不是 UTF-16".into());
    }
    let mut units = bytes
        .chunks_exact(2)
        .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
        .collect::<Vec<_>>();
    while units.last() == Some(&0) {
        units.pop();
    }
    if units.contains(&0) {
        return Err("用户 PATH 注册表值含内嵌 NUL".into());
    }
    String::from_utf16(&units).map_err(|error| format!("用户 PATH 不是合法 UTF-16: {error}"))
}

#[cfg(windows)]
fn encode_windows_registry_string(value: &str) -> Vec<u8> {
    value
        .encode_utf16()
        .chain(std::iter::once(0))
        .flat_map(u16::to_le_bytes)
        .collect()
}

#[cfg(windows)]
fn update_user_path_segment_windows(segment: &str, add: bool) -> Result<(), String> {
    use winreg::enums::*;
    use winreg::{RegKey, RegValue};

    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let env = if add {
        hkcu.create_subkey("Environment")
            .map(|(key, _)| key)
            .map_err(|error| format!("打开用户环境注册表失败: {error}"))?
    } else {
        match hkcu.open_subkey_with_flags("Environment", KEY_ALL_ACCESS) {
            Ok(key) => key,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return broadcast_environment_change_windows();
            }
            Err(error) => return Err(format!("打开用户环境注册表失败: {error}")),
        }
    };
    let existing = match env.get_raw_value("Path") {
        Ok(value) => Some(value),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(format!("读取用户 PATH 失败: {error}")),
    };
    let (current, value_type) = match &existing {
        Some(value) if value.vtype == REG_SZ || value.vtype == REG_EXPAND_SZ => (
            decode_windows_registry_string(&value.bytes)?,
            value.vtype.clone(),
        ),
        Some(value) => {
            return Err(format!(
                "用户 PATH 注册表类型 {:?} 不是 REG_SZ/REG_EXPAND_SZ，已拒绝改写",
                value.vtype
            ));
        }
        None => (String::new(), REG_EXPAND_SZ),
    };
    let parts: Vec<&str> = current.split(';').collect();
    let already_present = parts
        .iter()
        .any(|part| windows_path_segment_matches(part, segment));
    let updated = if add {
        if already_present {
            current.clone()
        } else if current.is_empty() {
            segment.to_owned()
        } else if current.ends_with(';') {
            format!("{current}{segment}")
        } else {
            format!("{current};{segment}")
        }
    } else {
        parts
            .into_iter()
            .filter(|part| !windows_path_segment_matches(part, segment))
            .collect::<Vec<_>>()
            .join(";")
    };
    if updated == current {
        return finalize_windows_env_unset(
            Some(&env),
            flush_registry_key_windows,
            broadcast_environment_change_windows,
        );
    }
    if updated.is_empty() {
        match env.delete_value("Path") {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("删除用户 PATH 失败: {error}")),
        }
    } else {
        env.set_raw_value(
            "Path",
            &RegValue {
                bytes: encode_windows_registry_string(&updated),
                vtype: value_type,
            },
        )
        .map_err(|error| format!("写入用户 PATH 失败: {error}"))?;
    }
    finalize_windows_env_unset(
        Some(&env),
        flush_registry_key_windows,
        broadcast_environment_change_windows,
    )
}

#[cfg(windows)]
fn set_user_env_windows(key: &str, value: &str) -> Result<(), String> {
    use winreg::enums::*;
    use winreg::RegKey;
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (env, _) = hkcu
        .create_subkey("Environment")
        .map_err(|e| format!("打开用户环境注册表失败: {}", e))?;
    env.set_value(key, &value)
        .map_err(|e| format!("写注册表环境变量失败: {}", e))?;
    flush_registry_key_windows(&env)?;
    broadcast_environment_change_windows()
}

#[cfg(windows)]
fn unset_user_env_windows(key: &str) -> Result<(), String> {
    use winreg::enums::*;
    use winreg::RegKey;
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let env = match hkcu.open_subkey_with_flags("Environment", KEY_ALL_ACCESS) {
        Ok(env) => env,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return finalize_windows_env_unset::<RegKey>(
                None,
                |_| Ok(()),
                broadcast_environment_change_windows,
            );
        }
        Err(error) => return Err(format!("打开用户环境注册表失败: {error}")),
    };
    match env.delete_value(key) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("删除注册表环境变量 {key} 失败: {error}")),
    }
    // 值已经不存在也可能是上次“删除成功、flush/广播失败”的重试。
    // 因此 NotFound 不能直接当作事务完成：仍要确认持久化并广播。
    finalize_windows_env_unset(
        Some(&env),
        flush_registry_key_windows,
        broadcast_environment_change_windows,
    )
}

/// Windows 环境删除的事务尾步。`env=None` 表示 Environment 子键本就不存在，
/// 但仍需重试广播；`env=Some` 则必须先 flush 再广播。抽出此函数以便在
/// 非 Windows CI 上覆盖 partial-delete -> retry 的幂等合同。
#[cfg(any(windows, test))]
fn finalize_windows_env_unset<T>(
    env: Option<&T>,
    flush: impl FnOnce(&T) -> Result<(), String>,
    broadcast: impl FnOnce() -> Result<(), String>,
) -> Result<(), String> {
    if let Some(env) = env {
        flush(env)?;
    }
    broadcast()
}

#[cfg(windows)]
fn flush_registry_key_windows(key: &winreg::RegKey) -> Result<(), String> {
    use windows_sys::Win32::System::Registry::RegFlushKey;

    // winreg 0.55 与本 crate 同用 windows-sys 的 opaque pointer HKEY，
    // 句柄类型已一致，无需再做转换。
    // SAFETY: raw_handle 在 key 生命周期内有效，RegFlushKey 不接管句柄。
    let status = unsafe { RegFlushKey(key.raw_handle()) };
    if status == 0 {
        Ok(())
    } else {
        Err(format!(
            "RegFlushKey 持久化用户环境失败: {}",
            std::io::Error::from_raw_os_error(status as i32)
        ))
    }
}

#[cfg(windows)]
fn broadcast_environment_change_windows() -> Result<(), String> {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        SendMessageTimeoutW, HWND_BROADCAST, SMTO_ABORTIFHUNG, WM_SETTINGCHANGE,
    };

    let environment: Vec<u16> = "Environment".encode_utf16().chain(Some(0)).collect();
    let mut message_result = 0usize;
    // SAFETY: Environment buffer 在同步调用期间存活且 NUL 结尾；
    // SendMessageTimeoutW 最多等待 5 秒，不持有该指针。
    let result = unsafe {
        SendMessageTimeoutW(
            HWND_BROADCAST,
            WM_SETTINGCHANGE,
            0,
            environment.as_ptr() as isize,
            SMTO_ABORTIFHUNG,
            5_000,
            &mut message_result,
        )
    };
    if result == 0 {
        Err(format!(
            "注册表已更新，但广播 Windows 环境变化失败: {}（可重试；必要时注销后生效）",
            std::io::Error::last_os_error()
        ))
    } else {
        Ok(())
    }
}

#[cfg(windows)]
fn get_user_env_windows_strict(key: &str) -> Result<Option<String>, String> {
    use winreg::enums::*;
    use winreg::RegKey;
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let env = match hkcu.open_subkey("Environment") {
        Ok(env) => env,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("读取用户环境注册表失败: {error}")),
    };
    match env.get_value::<String, _>(key) {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("读取注册表环境变量 {key} 失败: {error}")),
    }
}

#[cfg(not(windows))]
fn set_user_env_windows(_key: &str, _value: &str) -> Result<(), String> {
    Err("not windows".into())
}
#[cfg(not(windows))]
fn unset_user_env_windows(_key: &str) -> Result<(), String> {
    Ok(())
}
#[cfg(not(windows))]
fn get_user_env_windows_strict(_key: &str) -> Result<Option<String>, String> {
    Err("not windows".into())
}

fn is_generated_scheme_secret_key(key: &str) -> bool {
    let Some(body) = key
        .strip_prefix("SCHEME_SCH_")
        .and_then(|value| value.strip_suffix("_KEY"))
    else {
        return false;
    };
    let Some((seconds_hex, nanos)) = body.rsplit_once('_') else {
        return false;
    };
    !seconds_hex.is_empty()
        && seconds_hex.chars().all(|ch| ch.is_ascii_hexdigit())
        && nanos.len() == 9
        && nanos.chars().all(|ch| ch.is_ascii_digit())
}

/// 枚举本工具生成但可能因异常中断未进入 schemes.json 的动态 Key。
/// 只接受 `new_id()` 的精确命名形式，不会泛化删除用户的其它环境变量。
pub(crate) fn generated_scheme_secret_keys() -> Result<Vec<String>, String> {
    let mut keys = std::collections::BTreeSet::new();

    #[cfg(windows)]
    {
        use winreg::enums::*;
        use winreg::RegKey;
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let env = match hkcu.open_subkey("Environment") {
            Ok(env) => Some(env),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(format!("打开用户环境注册表失败: {error}")),
        };
        if let Some(env) = env {
            for value in env.enum_values() {
                let (name, _) = value.map_err(|error| format!("枚举用户环境变量失败: {error}"))?;
                if is_generated_scheme_secret_key(&name) {
                    keys.insert(name);
                }
            }
        }
    }

    #[cfg(not(windows))]
    {
        let Some(path) = secrets_env_path() else {
            return Ok(Vec::new());
        };
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                return Err("secrets.env 不是可信普通文件，已拒绝枚举".into())
            }
            Ok(_) => {
                let raw = std::fs::read_to_string(&path)
                    .map_err(|error| format!("读取 secrets.env 失败: {error}"))?;
                for line in raw.lines() {
                    if let Some((key, _)) = super::util::parse_secret_line(line) {
                        if is_generated_scheme_secret_key(&key) {
                            keys.insert(key);
                        }
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("检查 secrets.env 失败: {error}")),
        }
    }

    Ok(keys.into_iter().collect())
}

#[cfg(unix)]
fn parse_strict_node_version_directory(name: &std::ffi::OsStr) -> Option<(u64, u64, u64)> {
    let raw = name.to_str()?;
    let raw = raw.strip_prefix('v').unwrap_or(raw);
    let mut parts = raw.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

#[cfg(unix)]
fn trusted_nvm_node_bin(versions_root: &Path) -> Option<PathBuf> {
    let root_metadata = std::fs::symlink_metadata(versions_root).ok()?;
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        return None;
    }
    let mut candidates = Vec::new();
    for entry in std::fs::read_dir(versions_root).ok()? {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        let version = match parse_strict_node_version_directory(&entry.file_name()) {
            Some(version) => version,
            None => continue,
        };
        let version_dir = entry.path();
        let version_metadata = std::fs::symlink_metadata(&version_dir).ok()?;
        if version_metadata.file_type().is_symlink() || !version_metadata.is_dir() {
            continue;
        }
        let bin = version_dir.join("bin");
        let bin_metadata = match std::fs::symlink_metadata(&bin) {
            Ok(metadata) => metadata,
            Err(_) => continue,
        };
        if bin_metadata.file_type().is_symlink() || !bin_metadata.is_dir() {
            continue;
        }
        let node = bin.join("node");
        let node_metadata = match std::fs::symlink_metadata(&node) {
            Ok(metadata) => metadata,
            Err(_) => continue,
        };
        if node_metadata.file_type().is_symlink() || !node_metadata.is_file() {
            continue;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if node_metadata.permissions().mode() & 0o111 == 0 {
                continue;
            }
        }
        candidates.push((version, bin));
    }
    candidates
        .into_iter()
        .max_by_key(|(version, _)| *version)
        .map(|(_, bin)| bin)
}

pub fn refresh_path_from_system() {
    #[cfg(windows)]
    {
        use winreg::enums::*;
        use winreg::RegKey;
        let mut parts: Vec<String> = Vec::new();
        if let Ok(cur) = std::env::var("PATH") {
            parts.push(cur);
        }
        let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
        if let Ok(key) =
            hklm.open_subkey(r"SYSTEM\CurrentControlSet\Control\Session Manager\Environment")
        {
            if let Ok(v) = key.get_value::<String, _>("Path") {
                parts.push(v);
            }
        }
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        if let Ok(key) = hkcu.open_subkey("Environment") {
            if let Ok(v) = key.get_value::<String, _>("Path") {
                parts.push(v);
            }
        }
        // 合并去重
        let mut seen = std::collections::HashSet::new();
        let mut merged = Vec::new();
        for part in parts {
            for p in part.split(';') {
                let p = p.trim();
                if !p.is_empty() && seen.insert(p.to_string()) {
                    merged.push(p.to_string());
                }
            }
        }
        if !merged.is_empty() {
            unsafe { std::env::set_var("PATH", merged.join(";")) };
        }
    }
    #[cfg(not(windows))]
    {
        let mut path = std::env::var("PATH").unwrap_or_default();
        let extras = [
            "/opt/homebrew/bin",
            "/usr/local/bin",
            "/opt/homebrew/opt/node@22/bin",
            "/usr/local/opt/node@22/bin",
        ];
        for p in extras {
            if Path::new(p).exists() && !path.split(':').any(|x| x == p) {
                path = format!("{}:{}", p, path);
            }
        }
        // nvm / fnm / volta 常见路径
        if let Some(home) = home_dir() {
            for rel in [
                ".nvm/versions/node",
                ".local/share/fnm/aliases/default/bin",
                ".volta/bin",
                ".npm-global/bin",
            ] {
                let p = home.join(rel);
                if rel.contains("versions/node") {
                    if let Some(bin) = trusted_nvm_node_bin(&p) {
                        let bs = bin.display().to_string();
                        if !path.split(':').any(|x| x == bs) {
                            path = format!("{}:{}", bs, path);
                        }
                    }
                } else if p.exists() {
                    let s = p.display().to_string();
                    if !path.split(':').any(|x| x == s) {
                        path = format!("{}:{}", s, path);
                    }
                }
            }
        }
        // 这个函数会在每个 CLI 工作流的受控子进程之前运行，
        // 不得为探测 PATH 再无超时启动 `npm config get prefix`。
        // CodeCLI 自己的 npm prefix 会由 ensure_user_npm_prefix 精确添加；
        // 用户的 npm/node 则依靠已有 PATH 与上面的常见运行时目录，
        // 避免恶意或卡死的 npm shim 让整个安装器无限挂起。
        unsafe { std::env::set_var("PATH", path) };
    }
}

#[cfg(test)]
mod tests {
    use super::{
        durable_write_profile, env_export_content_from_snapshot, finalize_windows_env_unset,
        is_generated_scheme_secret_key, load_profile_snapshot, remove_env_export_block,
        remove_fixed_blocks_content, rollback_profile_if_ours, upsert_env_export_block,
        windows_path_segment_matches,
    };
    #[cfg(not(windows))]
    use super::{
        get_persistent_env_unix_from_home_strict, remove_env_export_blocks_from_home,
        remove_source_blocks_from_home,
    };
    #[cfg(unix)]
    use super::{parse_strict_node_version_directory, trusted_nvm_node_bin};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn fixed_block_removal_preserves_user_whitespace_and_removes_duplicates() {
        let raw = "  \nuser-before\n# >>> codecli-installer demo >>>\nfirst\n# <<< codecli-installer demo <<<\n\nuser-middle\r\n# >>> codecli-installer demo >>>\r\nsecond\r\n# <<< codecli-installer demo <<<\r\nuser-after\n\n";
        let cleaned = remove_fixed_blocks_content(raw, "demo").unwrap();
        assert_eq!(cleaned, "  \nuser-before\n\nuser-middle\r\nuser-after\n\n");
    }

    #[test]
    fn fixed_block_removal_rejects_orphan_or_reversed_markers() {
        assert!(remove_fixed_blocks_content(
            "# >>> codecli-installer demo >>>\nmissing end\n",
            "demo"
        )
        .is_err());
        assert!(remove_fixed_blocks_content(
            "# <<< codecli-installer demo <<<\n# >>> codecli-installer demo >>>\n",
            "demo"
        )
        .is_err());
    }

    #[test]
    fn marker_text_embedded_in_user_command_is_never_treated_as_a_block() {
        let raw = "echo '# >>> codecli-installer demo >>>'\nprintf '%s' '# <<< codecli-installer demo <<<'\n";
        assert_eq!(remove_fixed_blocks_content(raw, "demo").unwrap(), raw);
    }

    #[cfg(unix)]
    #[test]
    fn nvm_node_bin_uses_numeric_semver_and_rejects_linked_or_malformed_entries() {
        use std::os::unix::fs::{symlink, PermissionsExt};

        let home = TempHome::new();
        let root = home.path().join("versions/node");
        std::fs::create_dir_all(&root).unwrap();
        let create_version = |name: &str| {
            let bin = root.join(name).join("bin");
            std::fs::create_dir_all(&bin).unwrap();
            let node = bin.join("node");
            std::fs::write(&node, b"test node").unwrap();
            std::fs::set_permissions(&node, std::fs::Permissions::from_mode(0o700)).unwrap();
            bin
        };
        create_version("v22.9.0");
        let expected = create_version("v22.19.0");
        create_version("v9.99.99");
        create_version("v100"); // 非严格 major.minor.patch，必须忽略

        symlink(root.join("v22.19.0"), root.join("v99.0.0")).unwrap();
        std::fs::create_dir_all(root.join("v23.0.0")).unwrap();
        symlink(root.join("v22.19.0/bin"), root.join("v23.0.0/bin")).unwrap();

        assert_eq!(
            parse_strict_node_version_directory(std::ffi::OsStr::new("v22.19.0")),
            Some((22, 19, 0))
        );
        assert!(parse_strict_node_version_directory(std::ffi::OsStr::new("v22.9")).is_none());
        assert_eq!(trusted_nvm_node_bin(&root), Some(expected));
    }

    #[test]
    fn env_upsert_removes_all_complete_duplicates_without_trimming_user_content() {
        let home = TempHome::new();
        let profile = home.path().join(".zprofile");
        std::fs::write(
            &profile,
            "  \n# >>> codecli-installer ANTHROPIC_MODEL >>>\nexport ANTHROPIC_MODEL='old-one'\n# <<< codecli-installer ANTHROPIC_MODEL <<<\nuser\n# >>> codecli-installer ANTHROPIC_MODEL >>>\nexport ANTHROPIC_MODEL='old-two'\n# <<< codecli-installer ANTHROPIC_MODEL <<<\n\n",
        )
        .unwrap();
        upsert_env_export_block(&profile, "ANTHROPIC_MODEL", "new-model").unwrap();
        let body = std::fs::read_to_string(&profile).unwrap();
        assert!(body.starts_with("  \nuser\n\n"), "{body:?}");
        assert_eq!(
            body.matches("# >>> codecli-installer ANTHROPIC_MODEL >>>")
                .count(),
            1
        );
        assert!(body.contains("export ANTHROPIC_MODEL='new-model'"));
        assert!(!body.contains("old-one"));
        assert!(!body.contains("old-two"));
    }

    #[test]
    fn profile_write_cas_preserves_edit_made_after_snapshot() {
        let home = TempHome::new();
        let profile = home.path().join(".zprofile");
        std::fs::write(&profile, "before\n").unwrap();
        let snapshot = load_profile_snapshot(&profile).unwrap();
        let planned =
            env_export_content_from_snapshot(&snapshot, "ANTHROPIC_MODEL", "model").unwrap();

        std::fs::write(&profile, "user editor saved\n").unwrap();
        let error = durable_write_profile(&snapshot, &planned)
            .expect_err("stale snapshot must never overwrite a later editor save");
        assert!(error.contains("其它程序修改"), "{error}");
        assert_eq!(
            std::fs::read_to_string(&profile).unwrap(),
            "user editor saved\n"
        );
    }

    #[test]
    fn profile_rollback_cas_preserves_edit_made_after_our_write() {
        let home = TempHome::new();
        let profile = home.path().join(".zprofile");
        std::fs::write(&profile, "before\n").unwrap();
        let snapshot = load_profile_snapshot(&profile).unwrap();
        let planned =
            env_export_content_from_snapshot(&snapshot, "ANTHROPIC_MODEL", "model").unwrap();
        durable_write_profile(&snapshot, &planned).unwrap();

        std::fs::write(&profile, "user editor saved after tool\n").unwrap();
        let error = rollback_profile_if_ours(&snapshot, &planned)
            .expect_err("rollback must not overwrite a later editor save");
        assert!(error.contains("已被其它程序修改"), "{error}");
        assert_eq!(
            std::fs::read_to_string(&profile).unwrap(),
            "user editor saved after tool\n"
        );
    }

    #[test]
    fn profile_rollback_never_deletes_concurrently_rewritten_new_file() {
        let home = TempHome::new();
        let profile = home.path().join(".zprofile");
        let snapshot = load_profile_snapshot(&profile).unwrap();
        let planned =
            env_export_content_from_snapshot(&snapshot, "ANTHROPIC_MODEL", "model").unwrap();
        durable_write_profile(&snapshot, &planned).unwrap();

        std::fs::write(&profile, "user now owns this file\n").unwrap();
        rollback_profile_if_ours(&snapshot, &planned)
            .expect_err("rollback must not delete a concurrently rewritten new profile");
        assert_eq!(
            std::fs::read_to_string(&profile).unwrap(),
            "user now owns this file\n"
        );
    }

    #[test]
    fn env_remove_preserves_all_non_managed_whitespace_and_removes_duplicates() {
        let home = TempHome::new();
        let profile = home.path().join(".zprofile");
        let raw = " \n# >>> codecli-installer OPENAI_MODEL >>>\nexport OPENAI_MODEL='one'\n# <<< codecli-installer OPENAI_MODEL <<<\nkeep\n# >>> codecli-installer OPENAI_MODEL >>>\nexport OPENAI_MODEL='two'\n# <<< codecli-installer OPENAI_MODEL <<<\n\n";
        std::fs::write(&profile, raw).unwrap();
        remove_env_export_block(&profile, "OPENAI_MODEL").unwrap();
        assert_eq!(std::fs::read_to_string(&profile).unwrap(), " \nkeep\n\n");
    }

    #[test]
    fn windows_path_match_normalizes_case_quotes_slashes_and_trailing_separator() {
        assert!(windows_path_segment_matches(
            r#""C:/Users/Ahai/.claude/codecli-installer/npm-global/""#,
            r"c:\users\ahai\.claude\codecli-installer\npm-global"
        ));
        assert!(!windows_path_segment_matches(
            r"C:\Users\Ahai\bin",
            r"C:\Users\Ahai\bin-old"
        ));
    }

    #[test]
    fn windows_env_unset_retry_finishes_flush_before_broadcast() {
        use std::cell::RefCell;

        let calls = RefCell::new(Vec::new());
        finalize_windows_env_unset(
            Some(&()),
            |_| {
                calls.borrow_mut().push("flush");
                Ok(())
            },
            || {
                calls.borrow_mut().push("broadcast");
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(*calls.borrow(), ["flush", "broadcast"]);

        let calls = RefCell::new(Vec::new());
        let error = finalize_windows_env_unset(
            Some(&()),
            |_| {
                calls.borrow_mut().push("flush");
                Err("flush failed".into())
            },
            || {
                calls.borrow_mut().push("broadcast");
                Ok(())
            },
        )
        .unwrap_err();
        assert_eq!(error, "flush failed");
        assert_eq!(*calls.borrow(), ["flush"]);
    }

    #[test]
    fn windows_env_unset_missing_key_still_retries_broadcast() {
        use std::cell::Cell;

        let broadcasted = Cell::new(false);
        let error = finalize_windows_env_unset::<()>(
            None,
            |_| panic!("missing Environment key must not flush"),
            || {
                broadcasted.set(true);
                Err("broadcast failed".into())
            },
        )
        .unwrap_err();
        assert!(broadcasted.get());
        assert_eq!(error, "broadcast failed");
    }

    struct TempHome(PathBuf);

    impl TempHome {
        fn new() -> Self {
            let nonce = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "codecli-platform-test-{}-{}-{nonce}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("clock")
                    .as_nanos()
            ));
            std::fs::create_dir_all(&path).expect("create temp home");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempHome {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn generated_scheme_key_filter_is_strict() {
        assert!(is_generated_scheme_secret_key(
            "SCHEME_SCH_65ABCDEF_123456789_KEY"
        ));
        assert!(!is_generated_scheme_secret_key("SCHEME_USER_KEY"));
        assert!(!is_generated_scheme_secret_key(
            "SCHEME_SCH_NOTHEX_123456789_KEY"
        ));
        assert!(!is_generated_scheme_secret_key(
            "SCHEME_SCH_65ABCDEF_123_KEY"
        ));
    }

    #[cfg(not(windows))]
    #[test]
    fn process_only_value_is_not_treated_as_persistent() {
        let home = TempHome::new();
        let key = format!(
            "CODECLI_PLATFORM_TEMP_ONLY_{}",
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let previous = std::env::var_os(&key);
        unsafe { std::env::set_var(&key, "temporary-process-value") };

        assert_eq!(
            get_persistent_env_unix_from_home_strict(home.path(), &key).unwrap(),
            None
        );

        match previous {
            Some(value) => unsafe { std::env::set_var(&key, value) },
            None => unsafe { std::env::remove_var(&key) },
        }
    }

    #[cfg(not(windows))]
    #[test]
    fn persistent_plain_value_must_come_from_managed_block() {
        let home = TempHome::new();
        let profile = home.path().join(".zprofile");
        std::fs::write(&profile, "export ANTHROPIC_MODEL='manual-value'\n").unwrap();
        assert_eq!(
            get_persistent_env_unix_from_home_strict(home.path(), "ANTHROPIC_MODEL").unwrap(),
            None,
            "不应把未纳入工具事务的任意 shell 代码当作可恢复状态"
        );

        upsert_env_export_block(&profile, "ANTHROPIC_MODEL", "managed 'model'").unwrap();
        assert_eq!(
            get_persistent_env_unix_from_home_strict(home.path(), "ANTHROPIC_MODEL")
                .unwrap()
                .as_deref(),
            Some("managed 'model'")
        );
    }

    #[cfg(unix)]
    #[test]
    fn strict_secret_snapshot_distinguishes_absent_from_corrupt() {
        use std::os::unix::fs::{symlink, PermissionsExt};

        let home = TempHome::new();
        let key = "SCHEME_SCH_65ABCDEF_123456789_KEY";
        assert_eq!(
            get_persistent_env_unix_from_home_strict(home.path(), key).unwrap(),
            None
        );

        let dir = home.path().join(".claude/codecli-installer");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("secrets.env");
        std::fs::write(&path, format!("{key}='sk-strict-snapshot-123456'\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            get_persistent_env_unix_from_home_strict(home.path(), key)
                .unwrap()
                .as_deref(),
            Some("sk-strict-snapshot-123456")
        );

        std::fs::write(&path, "this is not an env assignment\n").unwrap();
        assert!(get_persistent_env_unix_from_home_strict(home.path(), key).is_err());

        std::fs::remove_file(&path).unwrap();
        let outside = home.path().join("outside-secrets");
        std::fs::write(&outside, format!("{key}='sk-outside-secret-123456'\n")).unwrap();
        symlink(&outside, &path).unwrap();
        assert!(get_persistent_env_unix_from_home_strict(home.path(), key).is_err());
    }

    #[cfg(not(windows))]
    #[test]
    fn strict_plain_snapshot_rejects_conflicting_managed_profiles() {
        let home = TempHome::new();
        upsert_env_export_block(
            &home.path().join(".zprofile"),
            "ANTHROPIC_MODEL",
            "model-one",
        )
        .unwrap();
        upsert_env_export_block(&home.path().join(".zshrc"), "ANTHROPIC_MODEL", "model-two")
            .unwrap();
        assert!(
            get_persistent_env_unix_from_home_strict(home.path(), "ANTHROPIC_MODEL")
                .unwrap_err()
                .contains("值不一致")
        );
    }

    #[cfg(unix)]
    #[test]
    fn profile_symlink_is_preserved_target_is_updated_and_mode_is_retained() {
        use std::os::unix::fs::{symlink, PermissionsExt};

        let home = TempHome::new();
        let dotfiles = home.path().join("dotfiles");
        std::fs::create_dir(&dotfiles).unwrap();
        let target = dotfiles.join("zprofile");
        std::fs::write(&target, "# user content\n").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o640)).unwrap();
        let logical = home.path().join(".zprofile");
        symlink("dotfiles/zprofile", &logical).unwrap();

        upsert_env_export_block(&logical, "ANTHROPIC_MODEL", "managed-model").unwrap();

        assert!(
            std::fs::symlink_metadata(&logical)
                .unwrap()
                .file_type()
                .is_symlink(),
            "原子替换不得用普通文件覆盖顶层链接"
        );
        let content = std::fs::read_to_string(&target).unwrap();
        assert!(content.contains("# user content"));
        assert!(content.contains("export ANTHROPIC_MODEL='managed-model'"));
        assert_eq!(
            std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o640,
            "改写既有 profile 时应保留 mode"
        );
    }

    #[cfg(unix)]
    #[test]
    fn newly_created_profile_uses_private_mode() {
        use std::os::unix::fs::PermissionsExt;

        let home = TempHome::new();
        let profile = home.path().join(".zprofile");
        upsert_env_export_block(&profile, "ANTHROPIC_MODEL", "managed-model").unwrap();

        assert_eq!(
            std::fs::metadata(&profile).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[cfg(unix)]
    #[test]
    fn dangling_profile_symlink_is_rejected_without_replacement() {
        use std::os::unix::fs::symlink;

        let home = TempHome::new();
        let profile = home.path().join(".zprofile");
        symlink("missing-target", &profile).unwrap();

        let error = upsert_env_export_block(&profile, "ANTHROPIC_MODEL", "managed-model")
            .expect_err("断链不得被原子 rename 替换成普通文件");
        assert!(error.contains("无效或已断开"), "{error}");
        assert!(std::fs::symlink_metadata(&profile)
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[cfg(not(windows))]
    #[test]
    fn env_block_removal_aggregates_errors_and_is_retryable() {
        let home = TempHome::new();
        std::fs::create_dir(home.path().join(".zprofile")).unwrap();
        upsert_env_export_block(
            &home.path().join(".zshrc"),
            "ANTHROPIC_MODEL",
            "managed-model",
        )
        .unwrap();
        std::fs::write(
            home.path().join(".bashrc"),
            "# >>> codecli-installer ANTHROPIC_MODEL >>>\n",
        )
        .unwrap();

        let error = remove_env_export_blocks_from_home(home.path(), "ANTHROPIC_MODEL")
            .expect_err("两个 profile 错误必须上报");
        assert!(error.contains(".zprofile"), "{error}");
        assert!(error.contains(".bashrc"), "{error}");
        assert_eq!(
            std::fs::read_to_string(home.path().join(".zshrc")).unwrap(),
            ""
        );

        std::fs::remove_dir(home.path().join(".zprofile")).unwrap();
        std::fs::remove_file(home.path().join(".bashrc")).unwrap();
        upsert_env_export_block(
            &home.path().join(".zprofile"),
            "ANTHROPIC_MODEL",
            "retry-model",
        )
        .unwrap();
        remove_env_export_blocks_from_home(home.path(), "ANTHROPIC_MODEL")
            .expect("修复文件后应可安全重试");
        assert_eq!(
            std::fs::read_to_string(home.path().join(".zprofile")).unwrap(),
            ""
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn source_block_removal_continues_after_one_profile_error() {
        let home = TempHome::new();
        std::fs::create_dir(home.path().join(".zprofile")).unwrap();
        let zshrc = home.path().join(".zshrc");
        std::fs::write(
            &zshrc,
            "before\n# >>> codecli-installer secrets >>>\n. '/tmp/secrets.env'\n# <<< codecli-installer secrets <<<\nafter\n",
        )
        .unwrap();

        let error = remove_source_blocks_from_home(home.path())
            .expect_err("单个 profile 不可读时必须返回错误");
        assert!(error.contains(".zprofile"), "{error}");
        let remaining = std::fs::read_to_string(&zshrc).unwrap();
        assert!(remaining.contains("before"));
        assert!(remaining.contains("after"));
        assert!(!remaining.contains("codecli-installer secrets"));

        std::fs::remove_dir(home.path().join(".zprofile")).unwrap();
        remove_source_blocks_from_home(home.path()).expect("剩余状态应可重试");
    }
}
