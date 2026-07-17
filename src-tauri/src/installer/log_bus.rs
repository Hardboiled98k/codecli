// SPDX-License-Identifier: MPL-2.0
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use super::util::mask_secrets_with;

const MAX_CURRENT_LOG_BYTES: u64 = 2 * 1024 * 1024;
const MAX_READ_LOG_BYTES: u64 = MAX_CURRENT_LOG_BYTES + 64 * 1024;
const MAX_EXPORT_LINES: usize = 1_500;
const MAX_LINE_CHARS: usize = 4_000;
static FILE_LOCK: Mutex<()> = Mutex::new(());
static LOG_WRITES_ENABLED: AtomicBool = AtomicBool::new(true);

/// 与日志写入/导出使用同一把锁；成功 purge 后保持禁用，
/// 避免已排队的 IPC append 在删除完成后又重建状态目录。
pub(crate) fn suspend_diagnostic_writes_for<T>(
    action: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    let _guard = FILE_LOCK
        .lock()
        .map_err(|_| "日志文件锁已损坏".to_string())?;
    LOG_WRITES_ENABLED.store(false, Ordering::SeqCst);
    let result = action();
    if result.is_err() {
        LOG_WRITES_ENABLED.store(true, Ordering::SeqCst);
    }
    result
}

#[tauri::command]
pub async fn resume_diagnostic_log() -> Result<(), String> {
    super::util::spawn_blocking_result(|| {
        let _guard = FILE_LOCK
            .lock()
            .map_err(|_| "日志文件锁已损坏".to_string())?;
        LOG_WRITES_ENABLED.store(true, Ordering::SeqCst);
        Ok(())
    })
    .await
}

fn logs_dir() -> Result<PathBuf, String> {
    let state =
        super::platform::codecli_state_dir().ok_or_else(|| "找不到用户配置目录".to_string())?;
    match std::fs::symlink_metadata(&state) {
        Ok(meta) if meta.file_type().is_symlink() || !meta.is_dir() => {
            return Err("本工具状态路径不是可信目录".into())
        }
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(&state).map_err(|e| format!("创建状态目录失败: {e}"))?;
        }
        Err(e) => return Err(format!("检查状态目录失败: {e}")),
    }
    let dir = state.join("logs");
    match std::fs::symlink_metadata(&dir) {
        Ok(meta) if meta.file_type().is_symlink() || !meta.is_dir() => {
            return Err("日志路径不是可信目录".into())
        }
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir(&dir).map_err(|e| format!("创建日志目录失败: {e}"))?;
        }
        Err(e) => return Err(format!("检查日志目录失败: {e}")),
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| format!("设置日志目录权限失败: {e}"))?;
    }
    Ok(dir)
}

fn reject_non_regular_or_symlink(path: &std::path::Path) -> Result<(), String> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() || !meta.is_file() => {
            Err(format!("拒绝写入非普通日志文件: {}", path.display()))
        }
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("检查日志文件失败 {}: {e}", path.display())),
    }
}

fn open_private_append(path: &std::path::Path) -> Result<std::fs::File, String> {
    reject_non_regular_or_symlink(path)?;
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW).mode(0o600);
    }
    let file = options
        .open(path)
        .map_err(|e| format!("安全打开诊断日志失败: {e}"))?;
    if !file
        .metadata()
        .map_err(|e| format!("读取日志句柄元数据失败: {e}"))?
        .is_file()
    {
        return Err("诊断日志句柄不是普通文件".into());
    }
    Ok(file)
}

fn read_private_log_lines(path: &std::path::Path) -> Result<Vec<String>, String> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(format!("检查诊断日志失败 {}: {error}", path.display())),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!("拒绝读取非普通诊断日志: {}", path.display()));
    }
    if metadata.len() > MAX_READ_LOG_BYTES {
        return Err(format!("诊断日志过大，已拒绝导出: {}", path.display()));
    }

    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = options
        .open(path)
        .map_err(|error| format!("安全打开诊断日志失败 {}: {error}", path.display()))?;
    let opened = file
        .metadata()
        .map_err(|error| format!("复查诊断日志失败 {}: {error}", path.display()))?;
    if !opened.is_file() || opened.len() > MAX_READ_LOG_BYTES {
        return Err(format!(
            "诊断日志打开后不再是可信小文件: {}",
            path.display()
        ));
    }
    let mut bytes = Vec::with_capacity(opened.len() as usize);
    file.take(MAX_READ_LOG_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("读取诊断日志失败 {}: {error}", path.display()))?;
    if bytes.len() as u64 > MAX_READ_LOG_BYTES {
        return Err(format!("诊断日志读取期间变大，已拒绝: {}", path.display()));
    }
    let raw = String::from_utf8(bytes)
        .map_err(|error| format!("诊断日志不是 UTF-8 {}: {error}", path.display()))?;
    Ok(raw.lines().map(sanitize_line).collect())
}

fn merge_persistent_and_ui_lines(mut persistent: Vec<String>, ui_lines: &[String]) -> Vec<String> {
    // UI 的大多数行已异步追加到持久日志。用计数多重集只补进
    // “尚未落盘的额外行”，避免导出时把整个 React 日志重复一遍。
    let mut persisted_counts = std::collections::HashMap::<String, usize>::new();
    for line in &persistent {
        *persisted_counts.entry(line.clone()).or_default() += 1;
    }
    for line in ui_lines {
        let line = sanitize_line(line);
        let remaining = persisted_counts.entry(line.clone()).or_default();
        if *remaining > 0 {
            *remaining -= 1;
        } else {
            persistent.push(line);
        }
    }
    let start = persistent.len().saturating_sub(MAX_EXPORT_LINES);
    persistent.drain(..start);
    persistent
}

fn sanitize_line(line: &str) -> String {
    let masked = mask_secrets_with(line, &[]);
    let mut out: String = masked.chars().take(MAX_LINE_CHARS).collect();
    // 单条 UI 日志不得伪造多行诊断记录。
    out = out.replace(['\r', '\n'], " ");
    out
}

fn ensure_private_file(path: &std::path::Path) -> Result<(), String> {
    #[cfg(not(unix))]
    let _ = path;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("设置日志文件权限失败: {e}"))?;
    }
    Ok(())
}

/// 将前端可见活动日志同步追加到本机私有滚动文件。
#[tauri::command]
pub async fn append_diagnostic_log(line: String) -> Result<(), String> {
    super::util::spawn_blocking_result(move || {
        if !LOG_WRITES_ENABLED.load(Ordering::SeqCst) {
            return Ok(());
        }
        let _guard = FILE_LOCK
            .lock()
            .map_err(|_| "日志文件锁已损坏".to_string())?;
        if !LOG_WRITES_ENABLED.load(Ordering::SeqCst) {
            return Ok(());
        }
        let path = logs_dir()?.join("codecli-current.log");
        reject_non_regular_or_symlink(&path)?;
        if path.metadata().map(|m| m.len()).unwrap_or(0) >= MAX_CURRENT_LOG_BYTES {
            let rotated = logs_dir()?.join("codecli-previous.log");
            reject_non_regular_or_symlink(&rotated)?;
            let _ = std::fs::remove_file(&rotated);
            std::fs::rename(&path, &rotated).map_err(|e| format!("轮转诊断日志失败: {e}"))?;
            ensure_private_file(&rotated)?;
        }
        let mut file = open_private_append(&path)?;
        ensure_private_file(&path)?;
        writeln!(file, "{}", sanitize_line(&line)).map_err(|e| format!("写入诊断日志失败: {e}"))?;
        file.flush().map_err(|e| format!("刷新诊断日志失败: {e}"))
    })
    .await
}

/// 导出一份脱敏、权限收紧的日志文件，返回可直接展示给用户的绝对路径。
#[tauri::command]
pub async fn export_diagnostic_log(lines: Vec<String>) -> Result<String, String> {
    super::util::spawn_blocking_result(move || {
        let _guard = FILE_LOCK
            .lock()
            .map_err(|_| "日志文件锁已损坏".to_string())?;
        let dir = logs_dir()?;
        let path = dir.join(format!(
            "codecli-diagnostic-{}.log",
            super::util::chrono_like_now()
        ));
        let mut persistent = read_private_log_lines(&dir.join("codecli-previous.log"))?;
        persistent.extend(read_private_log_lines(&dir.join("codecli-current.log"))?);
        let body = merge_persistent_and_ui_lines(persistent, &lines).join("\n");
        super::util::atomic_write_mode(&path, &(body + "\n"), true)?;
        path.to_str()
            .map(str::to_owned)
            .ok_or_else(|| "日志路径不是有效 UTF-8".to_string())
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostic_line_masks_secret_and_flattens_newlines() {
        let line = sanitize_line("OPENAI_API_KEY=sk-abcdefghijklmnopqrstuvwxyz\nforged");
        assert!(!line.contains("abcdefghijklmnopqrstuvwxyz"));
        assert!(!line.contains('\n'));
        assert!(line.contains("[redacted]"));
    }

    #[test]
    fn diagnostic_line_is_bounded() {
        let line = sanitize_line(&"x".repeat(MAX_LINE_CHARS + 100));
        assert_eq!(line.chars().count(), MAX_LINE_CHARS);
    }

    #[test]
    fn export_merge_prefers_persistent_log_and_only_adds_missing_ui_rows() {
        let persistent = vec!["one".into(), "same".into(), "same".into()];
        let ui = vec!["same".into(), "same".into(), "new".into()];
        assert_eq!(
            merge_persistent_and_ui_lines(persistent, &ui),
            ["one", "same", "same", "new"]
        );
    }

    #[cfg(unix)]
    #[test]
    fn private_append_rejects_symlink_and_creates_mode_0600() {
        use std::os::unix::fs::{symlink, PermissionsExt};

        let dir = std::env::temp_dir().join(format!(
            "codecli-log-test-{}-{}",
            std::process::id(),
            super::super::util::chrono_like_now()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("target.log");
        std::fs::write(&target, "do-not-touch").unwrap();
        let linked = dir.join("linked.log");
        symlink(&target, &linked).unwrap();
        assert!(open_private_append(&linked).is_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "do-not-touch");
        assert!(read_private_log_lines(&linked).is_err());

        let private = dir.join("private.log");
        drop(open_private_append(&private).unwrap());
        ensure_private_file(&private).unwrap();
        let mode = std::fs::metadata(&private).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let _ = std::fs::remove_dir_all(dir);
    }
}
