// SPDX-License-Identifier: MPL-2.0
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::process::Command;

use super::cmd::{check_cancelled, humanize_npm_err, run_timed};
use super::op_lock::with_new_operation;
use super::platform::{
    add_user_path_segment_windows, ensure_tool_path_block, os_kind, refresh_path_from_system,
    remove_tool_runtime_path_blocks, remove_user_path_segment_windows, which_cmd,
    which_cmd_candidates, OsKind,
};
use super::util::{
    atomic_write_mode, chrono_like_now, powershell_single_quote, remove_file_durable,
};

const MAX_CLI_OWNERSHIP_BYTES: u64 = 64 * 1024;
const CLI_OWNERSHIP_SCHEMA_VERSION: u8 = 2;

/// npm 安装归属不能只记一个 bool。`pending` 在 npm 副作用前持久化，
/// 即使进程在 npm 返回后、提交 `installed` 前崩溃，下次也只会检查/清理
/// CodeCLI 专属 prefix 里的精确包。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum CliInstallState {
    Pending,
    Installed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct CliInstallRecord {
    pub schema_version: u8,
    pub package: String,
    pub prefix: std::path::PathBuf,
    pub state: CliInstallState,
    pub version: Option<String>,
    pub receipt: Option<super::pinned_npm::PinnedBundleReceipt>,
    pub updated_at: String,
}

impl CliInstallRecord {
    pub(crate) fn pending(package: &str, prefix: std::path::PathBuf) -> Self {
        Self {
            schema_version: CLI_OWNERSHIP_SCHEMA_VERSION,
            package: package.to_string(),
            prefix,
            state: CliInstallState::Pending,
            version: None,
            receipt: None,
            updated_at: chrono_like_now(),
        }
    }

    pub(crate) fn installed_from(previous: &Self, version: String) -> Self {
        Self {
            schema_version: previous.schema_version,
            package: previous.package.clone(),
            prefix: previous.prefix.clone(),
            state: CliInstallState::Installed,
            version: Some(version),
            receipt: previous.receipt.clone(),
            updated_at: chrono_like_now(),
        }
    }

    pub(crate) fn pending_from(previous: &Self) -> Self {
        Self {
            schema_version: previous.schema_version,
            package: previous.package.clone(),
            prefix: previous.prefix.clone(),
            state: CliInstallState::Pending,
            version: previous.version.clone(),
            receipt: previous.receipt.clone(),
            updated_at: chrono_like_now(),
        }
    }

    pub(crate) fn with_receipt(&self, receipt: super::pinned_npm::PinnedBundleReceipt) -> Self {
        Self {
            schema_version: self.schema_version,
            package: self.package.clone(),
            prefix: self.prefix.clone(),
            state: CliInstallState::Pending,
            version: self.version.clone(),
            receipt: Some(receipt),
            updated_at: chrono_like_now(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeState {
    pub ok: bool,
    pub skipped: bool,
    pub node_version: Option<String>,
    pub message: String,
    pub requires_restart: bool,
}

/// Node v22.19.0 官方 `SHASUMS256.txt` 的精确 archive pin。
/// 升级版本时必须同时从 nodejs.org 的 signed SHASUMS 更新这些值。
fn expected_node_archive_sha256(file_name: &str) -> Option<&'static str> {
    match file_name {
        "node-v22.19.0-darwin-arm64.tar.gz" => {
            Some("c59006db713c770d6ec63ae16cb3edc11f49ee093b5c415d667bb4f436c6526d")
        }
        "node-v22.19.0-darwin-x64.tar.gz" => {
            Some("3cfed4795cd97277559763c5f56e711852d2cc2420bda1cea30c8aa9ac77ce0c")
        }
        "node-v22.19.0-win-arm64.zip" => {
            Some("e4a7336010d58ff35b53d9dd5869095c56089c70913cf22508cf8183593e56b2")
        }
        "node-v22.19.0-win-x64.zip" => {
            Some("ea3fad0e67a991d8477d8c01344b56e69c676ccb733f065b22436994b1253f86")
        }
        _ => None,
    }
}

fn verify_node_archive_sha256(path: &std::path::Path, file_name: &str) -> Result<(), String> {
    let expected = expected_node_archive_sha256(file_name)
        .ok_or_else(|| format!("Node archive {file_name} 没有内嵌 SHA-256 pin"))?;
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("读取 Node archive 元数据失败: {error}"))?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.len() < 1_000_000
        || metadata.len() > 256 * 1024 * 1024
    {
        return Err("下载的 Node archive 类型或大小异常".into());
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
        .map_err(|error| format!("安全打开 Node archive 失败: {error}"))?;
    let opened = file
        .metadata()
        .map_err(|error| format!("复查 Node archive 失败: {error}"))?;
    if !opened.is_file() || opened.len() != metadata.len() {
        return Err("Node archive 在校验前发生替换".into());
    }
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| format!("读取 Node archive 失败: {error}"))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let actual = hex::encode(hasher.finalize());
    if actual != expected {
        let _ = std::fs::remove_file(path);
        return Err(format!(
            "Node archive SHA-256 不匹配（期望 {}…，实际 {}…），已删除并拒绝解压",
            &expected[..12],
            &actual[..12]
        ));
    }
    Ok(())
}

fn prepare_archive_download_path(path: &std::path::Path) -> Result<(), String> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err(format!("下载目标 {} 不是可信普通文件", path.display()))
        }
        Ok(_) => std::fs::remove_file(path)
            .map_err(|error| format!("删除旧下载文件失败 {}: {error}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("检查下载目标失败 {}: {error}", path.display())),
    }
}

fn download_node_archive(url: &str, destination: &std::path::Path) -> Result<(), String> {
    let parsed = url::Url::parse(url).map_err(|error| format!("Node 下载 URL 无效: {error}"))?;
    if parsed.scheme() != "https"
        || parsed.host_str() != Some("nodejs.org")
        || parsed.port().is_some()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err("Node 下载 URL 不在固定 nodejs.org HTTPS origin".into());
    }
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(20))
        .timeout(std::time::Duration::from_secs(320))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| format!("创建 Node 下载客户端失败: {error}"))?;
    let mut response = client
        .get(parsed)
        .send()
        .map_err(|error| format!("下载 Node 失败: {error}"))?;
    let status = response.status();
    if status.is_redirection() {
        return Err(format!(
            "Node 下载返回重定向 HTTP {}，已拒绝",
            status.as_u16()
        ));
    }
    if !status.is_success() {
        return Err(format!("Node 下载失败 HTTP {}", status.as_u16()));
    }
    const MAX_ARCHIVE_BYTES: u64 = 256 * 1024 * 1024;
    if response
        .content_length()
        .is_some_and(|length| length > MAX_ARCHIVE_BYTES)
    {
        return Err("Node archive Content-Length 超出 256 MiB".into());
    }

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options
        .open(destination)
        .map_err(|error| format!("创建 Node 下载文件失败: {error}"))?;
    let result = (|| -> Result<(), String> {
        let mut total = 0_u64;
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            check_cancelled()?;
            let read = response
                .read(&mut buffer)
                .map_err(|error| format!("读取 Node 下载响应失败: {error}"))?;
            if read == 0 {
                break;
            }
            total = total.saturating_add(read as u64);
            if total > MAX_ARCHIVE_BYTES {
                return Err("Node archive 下载超过 256 MiB，已中止".into());
            }
            file.write_all(&buffer[..read])
                .map_err(|error| format!("写 Node archive 失败: {error}"))?;
        }
        file.sync_all()
            .map_err(|error| format!("同步 Node archive 失败: {error}"))?;
        Ok(())
    })();
    drop(file);
    if let Err(error) = result {
        let _ = std::fs::remove_file(destination);
        return Err(error);
    }
    Ok(())
}

fn reset_runtime_destination(path: &std::path::Path, recreate: bool) -> Result<(), String> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(format!("运行时目标 {} 不是可信真实目录", path.display()));
        }
        Ok(_) => std::fs::remove_dir_all(path)
            .map_err(|error| format!("删除旧运行时目录失败 {}: {error}", path.display()))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("检查旧运行时目录失败: {error}")),
    }
    if recreate {
        std::fs::create_dir_all(path).map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn node_major(ver: &str) -> Option<u32> {
    let v = ver.trim().trim_start_matches('v');
    v.split('.').next()?.parse().ok()
}

fn node_version_from_command(cmd: Command, timeout_secs: u64) -> Option<String> {
    let out = run_timed(cmd, timeout_secs).ok()?;
    // 非 0 退出即为验证失败。不能因为某个 shim/脚本向 stdout
    // 打印了错误文字，就把它当成可用的 Node。
    if !out.status_ok {
        return None;
    }
    out.stdout
        .lines()
        .find(|l| !l.trim().is_empty())
        .map(|l| l.trim().to_string())
        .or_else(|| {
            out.stderr
                .lines()
                .find(|l| !l.trim().is_empty())
                .map(|l| l.trim().to_string())
        })
}

fn current_node_version() -> Option<String> {
    refresh_path_from_system();
    let mut cmd = Command::new("node");
    cmd.arg("-v");
    // PATH 中的 node 可能是损坏或恶意 shim，不得在 ensure_node
    // 入口用 raw output 无限等待。run_timed 同时会去除密钥环境。
    node_version_from_command(cmd, 15)
}

fn owned_node_executable_for_state(state: &std::path::Path) -> std::path::PathBuf {
    let node_root = state.join("runtime").join("node");
    if cfg!(windows) {
        node_root.join("node.exe")
    } else {
        node_root.join("bin").join("node")
    }
}

fn platform_paths_equal(left: &std::path::Path, right: &std::path::Path) -> bool {
    if cfg!(windows) {
        left.to_string_lossy()
            .replace('/', "\\")
            .trim_end_matches('\\')
            .eq_ignore_ascii_case(
                right
                    .to_string_lossy()
                    .replace('/', "\\")
                    .trim_end_matches('\\'),
            )
    } else {
        left == right
    }
}

/// 只有 PATH 正在使用 CodeCLI 专属目录中的真实可执行文件时，
/// 才允许“升级 Node”覆盖该目录。链接/重解析点不能作为归属证明。
fn node_path_is_owned_by_codecli(active: &std::path::Path, state: &std::path::Path) -> bool {
    let expected = owned_node_executable_for_state(state);
    if !active.is_absolute() || !platform_paths_equal(active, &expected) {
        return false;
    }

    let runtime = state.join("runtime");
    let node_root = runtime.join("node");
    for directory in [runtime.as_path(), node_root.as_path()] {
        let Ok(metadata) = std::fs::symlink_metadata(directory) else {
            return false;
        };
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return false;
        }
    }
    if !cfg!(windows) {
        let bin = node_root.join("bin");
        let Ok(metadata) = std::fs::symlink_metadata(&bin) else {
            return false;
        };
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return false;
        }
    }
    let Ok(metadata) = std::fs::symlink_metadata(&expected) else {
        return false;
    };
    metadata.is_file() && !metadata.file_type().is_symlink()
}

pub(crate) fn codecli_node_runtime_is_active() -> bool {
    refresh_path_from_system();
    let Some(active) = which_cmd("node") else {
        return false;
    };
    let Some(state) = super::platform::codecli_state_dir() else {
        return false;
    };
    node_path_is_owned_by_codecli(std::path::Path::new(&active), &state)
}

#[tauri::command]
pub async fn ensure_node(min_major: Option<u32>) -> Result<RuntimeState, String> {
    super::util::spawn_blocking_result(move || with_new_operation(|| ensure_node_sync(min_major)))
        .await
}

pub fn ensure_node_sync(min_major: Option<u32>) -> Result<RuntimeState, String> {
    ensure_node_sync_with_force(min_major, false)
}

/// `force=true` 用于明确的“升级”动作：即使已满足最低版本也会
/// 重装 CodeCLI 专属目录中的固定 Node archive。绝不调用 brew/winget/
/// Chocolatey 的 mutable latest，也不修改无法证明归属的用户 Node。
pub fn ensure_node_sync_with_force(
    min_major: Option<u32>,
    force: bool,
) -> Result<RuntimeState, String> {
    check_cancelled()?;
    let min = min_major.unwrap_or(18);
    let current = current_node_version();
    if let Some(ver) = current.as_deref() {
        if let Some(major) = node_major(ver) {
            if major >= min && !force {
                if which_cmd("npm").is_none() {
                    return Err(format!(
                        "检测到 Node {} 但找不到 npm。请重装 Node（勾选 npm）后重试。",
                        ver
                    ));
                }
                return Ok(RuntimeState {
                    ok: true,
                    skipped: true,
                    node_version: Some(ver.to_string()),
                    message: format!("Node.js 已安装 {}，跳过安装", ver),
                    requires_restart: false,
                });
            }
            // 过旧，或 force 明确要求升级。
        }
    }

    if force && current.is_some() && !codecli_node_runtime_is_active() {
        return Err(
            "检测到非 CodeCLI 专属目录中的 Node。为避免改变你的 nvm/volta/Homebrew/winget/Chocolatey 环境，本工具拒绝自动升级；请用原管理器升级。".into(),
        );
    }

    match os_kind() {
        OsKind::Macos => install_node_macos(min),
        OsKind::Windows => install_node_windows(min),
        OsKind::Linux => Err("暂不支持 Linux GUI 安装 Node，请手动安装 Node 18+".into()),
        OsKind::Unknown => Err("未知操作系统，无法安装 Node".into()),
    }
}

fn verify_node_after_install(min: u32) -> Result<RuntimeState, String> {
    refresh_path_from_system();
    if let Some(ver) = current_node_version() {
        let major = node_major(&ver);
        if major.map(|v| v >= min).unwrap_or(false) && which_cmd("npm").is_some() {
            return Ok(RuntimeState {
                ok: true,
                skipped: false,
                node_version: Some(ver),
                message: "Node.js 安装成功，已验证 node/npm".into(),
                requires_restart: false,
            });
        }
        let message = if major.map(|v| v < min).unwrap_or(false) {
            format!(
                "安装/升级命令已结束，但当前 PATH 仍解析到过旧 Node {}（需要 {}+）。请重开安装器后重试。",
                ver, min
            )
        } else if which_cmd("npm").is_none() {
            format!(
                "检测到 Node {}，但 npm 仍不可用。请重开安装器或重装 Node。",
                ver
            )
        } else {
            format!("Node 版本输出无法解析（{}），未能验证安装成功。", ver)
        };
        return Ok(RuntimeState {
            ok: false,
            skipped: false,
            node_version: Some(ver),
            message,
            requires_restart: true,
        });
    }
    Ok(RuntimeState {
        // 安装命令“看起来成功”不等于 runtime 可用。返回 false
        // 阻止后续 npm 步骤被误报为成功。
        ok: false,
        skipped: false,
        node_version: None,
        message: "Node 已安装但当前进程检测不到。请关闭安装器重新打开后再继续。".into(),
        requires_restart: true,
    })
}

fn install_node_macos(min: u32) -> Result<RuntimeState, String> {
    install_node_official_unix_tarball(min)
}

fn install_node_official_unix_tarball(min: u32) -> Result<RuntimeState, String> {
    check_cancelled()?;
    let arch = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "x64",
        other => return Err(format!("不支持的架构: {}", other)),
    };
    let ver = "v22.19.0";
    let name = format!("node-{}-darwin-{}", ver, arch);
    let url = format!("https://nodejs.org/dist/{}/{}.tar.gz", ver, name);
    let base = super::platform::codecli_state_dir()
        .ok_or("找不到安装目录")?
        .join("runtime");
    std::fs::create_dir_all(&base).map_err(|e| e.to_string())?;
    let base_metadata =
        std::fs::symlink_metadata(&base).map_err(|error| format!("检查运行时目录失败: {error}"))?;
    if base_metadata.file_type().is_symlink() || !base_metadata.is_dir() {
        return Err("运行时目录不是可信真实目录".into());
    }
    let archive_name = format!("{}.tar.gz", name);
    let tgz = base.join(&archive_name);
    let dest = base.join("node");
    prepare_archive_download_path(&tgz)?;

    download_node_archive(&url, &tgz).map_err(|error| {
        format!("下载 Node 失败。请检查网络或手动安装: https://nodejs.org/\n{error}")
    })?;
    if !tgz.exists() || tgz.metadata().map(|m| m.len()).unwrap_or(0) < 1_000_000 {
        return Err("下载的 Node 包过小或不存在，请重试。".into());
    }
    verify_node_archive_sha256(&tgz, &archive_name)?;

    reset_runtime_destination(&dest, true)?;
    let mut tar = Command::new("tar");
    tar.args(["-xzf"])
        .arg(&tgz)
        .arg("-C")
        .arg(&dest)
        .arg("--strip-components=1");
    let tar_out = run_timed(tar, 120).map_err(|e| format!("解压 Node 失败: {}", e))?;
    if !tar_out.status_ok {
        return Err(format!(
            "解压 Node 失败: {}",
            tar_out.stderr.chars().take(200).collect::<String>()
        ));
    }
    let _ = std::fs::remove_file(&tgz);

    inject_node_bin_path(&dest.join("bin"))?;
    let mut st = verify_node_after_install(min)?;
    if st.ok {
        st.message = format!("已安装官方 Node {} 到用户目录，已验证 node/npm", ver);
    }
    Ok(st)
}

fn inject_node_bin_path(bin: &std::path::Path) -> Result<(), String> {
    if !bin.exists() {
        return Ok(());
    }
    let b = bin.display().to_string();
    let mut path = std::env::var("PATH").unwrap_or_default();
    if !path.split(':').any(|x| x == b) {
        path = format!("{}:{}", b, path);
        unsafe { std::env::set_var("PATH", path) };
    }
    ensure_tool_path_block("node-path", bin)
}

fn install_node_windows(min: u32) -> Result<RuntimeState, String> {
    install_node_windows_zip(min)
}

fn install_node_windows_zip(min: u32) -> Result<RuntimeState, String> {
    check_cancelled()?;
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        other => return Err(format!("不支持的 Windows 架构: {}", other)),
    };
    let ver = "v22.19.0";
    let name = format!("node-{}-win-{}", ver, arch);
    let url = format!("https://nodejs.org/dist/{}/{}.zip", ver, name);
    let base = super::platform::codecli_state_dir()
        .ok_or("找不到安装目录")?
        .join("runtime");
    std::fs::create_dir_all(&base).map_err(|e| e.to_string())?;
    let base_metadata =
        std::fs::symlink_metadata(&base).map_err(|error| format!("检查运行时目录失败: {error}"))?;
    if base_metadata.file_type().is_symlink() || !base_metadata.is_dir() {
        return Err("运行时目录不是可信真实目录".into());
    }
    let archive_name = format!("{}.zip", name);
    let zip_path = base.join(&archive_name);
    let dest = base.join("node");
    prepare_archive_download_path(&zip_path)?;

    download_node_archive(&url, &zip_path)
        .map_err(|error| format!("下载 Node 失败。请手动安装: https://nodejs.org/\n{error}"))?;
    if !zip_path.exists() || zip_path.metadata().map(|m| m.len()).unwrap_or(0) < 1_000_000 {
        return Err("下载的 Node zip 过小或不存在，请重试。".into());
    }
    verify_node_archive_sha256(&zip_path, &archive_name)?;

    // 先删旧 dest，解压到 base，再把 node-*-win-* 目录 rename 成 dest（Win 不能 rename 到已存在目录）
    reset_runtime_destination(&dest, false)?;
    let extracted = base.join(&name);
    reset_runtime_destination(&extracted, false)?;
    let expand = format!(
        "$ErrorActionPreference = 'Stop'; Expand-Archive -Path {} -DestinationPath {} -Force",
        powershell_single_quote(&zip_path.display().to_string()),
        powershell_single_quote(&base.display().to_string())
    );
    let mut un = Command::new("powershell");
    un.args(["-NoProfile", "-Command", &expand]);
    let un_out = run_timed(un, 120).map_err(|e| format!("解压 Node 失败: {}", e))?;
    if !un_out.status_ok {
        return Err(format!(
            "解压 Node 失败: {}",
            un_out.stderr.chars().take(200).collect::<String>()
        ));
    }
    let extracted_metadata = std::fs::symlink_metadata(&extracted).map_err(|_| {
        format!(
            "解压后未找到目录 {}，请手动安装 Node: https://nodejs.org/",
            extracted.display()
        )
    })?;
    if extracted_metadata.file_type().is_symlink() || !extracted_metadata.is_dir() {
        return Err("解压后的 Node 目录类型异常，已拒绝使用".into());
    }
    std::fs::rename(&extracted, &dest).map_err(|e| {
        format!(
            "移动 Node 目录失败: {}（{} -> {}）",
            e,
            extracted.display(),
            dest.display()
        )
    })?;
    let _ = std::fs::remove_file(&zip_path);

    // PATH: 当前进程 + 用户 PATH 持久化
    let bin = dest.display().to_string();
    append_user_path_windows(&bin)?;

    let mut st = verify_node_after_install(min)?;
    if st.ok {
        st.message = format!("已安装用户级 Node {}（无需 winget），已验证 node/npm", ver);
    }
    Ok(st)
}

fn append_user_path_windows(dir: &str) -> Result<(), String> {
    // 当前进程
    if let Ok(path) = std::env::var("PATH") {
        if !path.split(';').any(|x| x.eq_ignore_ascii_case(dir)) {
            unsafe { std::env::set_var("PATH", format!("{};{}", dir, path)) };
        }
    }
    // 直接使用注册表安全更新，并执行 RegFlushKey + WM_SETTINGCHANGE。
    add_user_path_segment_windows(dir)
}

pub(crate) fn owned_npm_prefix() -> Result<std::path::PathBuf, String> {
    let prefix = super::platform::codecli_state_dir()
        .ok_or("找不到 CodeCLI 状态目录")?
        .join("npm-global");
    if !prefix.is_absolute() {
        return Err("CodeCLI npm prefix 不是绝对路径，已拒绝 npm 操作".into());
    }
    Ok(prefix)
}

fn ensure_real_directory(path: &std::path::Path, label: &str) -> Result<(), String> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(format!("{label} 不是可信真实目录"));
        }
        Ok(_) => return Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("检查 {label} 失败: {error}")),
    }
    std::fs::create_dir_all(path).map_err(|error| format!("创建 {label} 失败: {error}"))?;
    let metadata =
        std::fs::symlink_metadata(path).map_err(|error| format!("复查 {label} 失败: {error}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(format!("{label} 创建后不是可信真实目录"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("设置 {label} 0700 权限失败: {error}"))?;
    }
    Ok(())
}

pub(crate) fn ensure_owned_npm_prefix() -> Result<std::path::PathBuf, String> {
    let prefix = owned_npm_prefix()?;
    let state = prefix.parent().ok_or("CodeCLI npm prefix 没有父目录")?;
    ensure_real_directory(state, "CodeCLI 状态目录")?;
    ensure_real_directory(&prefix, "CodeCLI npm prefix")?;
    // Windows: 全局 bin 在 prefix 本身；Unix: prefix/bin
    let path_dir = if cfg!(windows) {
        prefix.clone()
    } else {
        let b = prefix.join("bin");
        ensure_real_directory(&b, "CodeCLI npm bin 目录")?;
        b
    };

    if cfg!(windows) {
        append_user_path_windows(&path_dir.display().to_string())?;
    } else {
        let b = path_dir.display().to_string();
        let mut path = std::env::var("PATH").unwrap_or_default();
        if !path.split(':').any(|x| x == b) {
            path = format!("{}:{}", b, path);
            unsafe { std::env::set_var("PATH", path) };
        }
        ensure_tool_path_block("npm-prefix", &path_dir)?;
    }
    Ok(prefix)
}

fn metadata_is_link_or_reparse(metadata: &std::fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        // FILE_ATTRIBUTE_REPARSE_POINT. npm/node 执行入口必须是普通文件，
        // 不跟随可在校验后切换目标的 reparse point。
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        false
    }
}

fn validate_absolute_regular_file(
    path: &std::path::Path,
    label: &str,
    max_bytes: u64,
) -> Result<std::path::PathBuf, String> {
    if !path.is_absolute() {
        return Err(format!("{label} 不是绝对路径"));
    }
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("检查 {label} {} 失败: {error}", path.display()))?;
    if metadata_is_link_or_reparse(&metadata) || !metadata.is_file() {
        return Err(format!("{label} 不是可信普通文件: {}", path.display()));
    }
    if metadata.len() == 0 || metadata.len() > max_bytes {
        return Err(format!("{label} 大小异常: {}", path.display()));
    }
    Ok(path.to_path_buf())
}

/// Windows 上 `npm.cmd` 必须通过 cmd.exe 解释；但把包名、registry
/// URL 或 `--prefix <PathBuf>` 追加到 `cmd /C npm` 后，`&|<>^%`
/// 等元字符会再被 cmd 解析。这里只用 npm.cmd 的受信位置定位
/// 同目录的 node.exe 与 npm-cli.js，随后通过 CreateProcess 直接启动
/// node.exe；所有动态参数因而保持独立 argv，不经任何 shell。
fn windows_npm_command_from_shim(npm_shim: &std::path::Path) -> Result<Command, String> {
    let npm_shim = validate_absolute_regular_file(npm_shim, "Windows npm.cmd", 4 * 1024 * 1024)?;
    let is_npm_cmd = npm_shim
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case("npm.cmd"));
    if !is_npm_cmd {
        return Err("Windows npm 入口不是 npm.cmd，已拒绝经 shell 执行".into());
    }
    let directory = npm_shim.parent().ok_or("Windows npm.cmd 没有父目录")?;
    let node = validate_absolute_regular_file(
        &directory.join("node.exe"),
        "Windows node.exe",
        512 * 1024 * 1024,
    )?;
    let npm_cli = validate_absolute_regular_file(
        &directory.join("node_modules/npm/bin/npm-cli.js"),
        "Windows npm-cli.js",
        32 * 1024 * 1024,
    )?;
    let mut command = Command::new(node);
    command.arg(npm_cli);
    Ok(command)
}

fn windows_npm_command_from_executable(npm_exe: &std::path::Path) -> Result<Command, String> {
    let npm_exe = validate_absolute_regular_file(npm_exe, "Windows npm.exe", 512 * 1024 * 1024)?;
    let is_npm_exe = npm_exe
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case("npm.exe"));
    if !is_npm_exe {
        return Err("Windows npm 可执行入口不是 npm.exe".into());
    }
    Ok(Command::new(npm_exe))
}

fn windows_npm_command_from_candidates<I, P>(candidates: I) -> Result<Command, String>
where
    I: IntoIterator<Item = P>,
    P: AsRef<std::path::Path>,
{
    // `where npm` 会按 Windows PATH/PATHEXT 顺序返回多个入口，
    // 官方 Node 还可能先返回不能被 CreateProcess 直接执行的
    // 无扩展名 `npm`。跳过这类非 Windows 执行入口，但一旦
    // 遇到第一个 npm.exe/npm.cmd，其验证失败就 fail closed，
    // 不降级到 PATH 后面的另一个 npm。
    for candidate in candidates {
        let path = candidate.as_ref();
        let extension = path
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or_default();
        if extension.eq_ignore_ascii_case("exe") {
            return windows_npm_command_from_executable(path);
        }
        if extension.eq_ignore_ascii_case("cmd") {
            return windows_npm_command_from_shim(path);
        }
    }
    Err("PATH 中找不到可信 npm.exe/npm.cmd，请重装 Node.js（包含 npm）".into())
}

pub(crate) fn npm_command() -> Result<Command, String> {
    if cfg!(windows) {
        windows_npm_command_from_candidates(which_cmd_candidates("npm"))
    } else {
        Ok(Command::new("npm"))
    }
}

fn safe_npm_package_components(package: &str) -> Result<Vec<&str>, String> {
    let components: Vec<&str> = package.split('/').collect();
    let valid_atom = |part: &str| {
        !part.is_empty()
            && part.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
            })
    };
    match components.as_slice() {
        [plain] if valid_atom(plain) && !plain.starts_with('@') => Ok(components),
        [scope, name]
            if scope.starts_with('@')
                && valid_atom(scope.trim_start_matches('@'))
                && valid_atom(name) =>
        {
            Ok(components)
        }
        _ => Err("非法 npm 包名".into()),
    }
}

fn npm_modules_root(prefix: &std::path::Path) -> std::path::PathBuf {
    if cfg!(windows) {
        prefix.join("node_modules")
    } else {
        prefix.join("lib").join("node_modules")
    }
}

fn npm_package_dir(prefix: &std::path::Path, package: &str) -> Result<std::path::PathBuf, String> {
    let mut path = npm_modules_root(prefix);
    for component in safe_npm_package_components(package)? {
        path.push(component);
    }
    Ok(path)
}

fn read_small_regular_file(
    path: &std::path::Path,
    label: &str,
    max_bytes: u64,
) -> Result<Option<Vec<u8>>, String> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("读取 {label} 元数据失败: {error}")),
    };
    if metadata_is_link_or_reparse(&metadata) || !metadata.is_file() {
        return Err(format!("{label} 不是可信普通文件"));
    }
    if metadata.len() > max_bytes {
        return Err(format!("{label} 超过大小上限"));
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
    if !opened.is_file() || opened.len() != metadata.len() || opened.len() > max_bytes {
        return Err(format!("{label} 打开后发生替换或变大"));
    }
    let mut bytes = Vec::new();
    file.take(max_bytes + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("读取 {label} 失败: {error}"))?;
    if bytes.len() as u64 > max_bytes {
        return Err(format!("{label} 读取期间变大"));
    }
    Ok(Some(bytes))
}

fn validate_existing_directory_chain(
    prefix: &std::path::Path,
    target: &std::path::Path,
) -> Result<bool, String> {
    if !target.starts_with(prefix) {
        return Err("npm 包路径越界".into());
    }
    let mut current = prefix.to_path_buf();
    match std::fs::symlink_metadata(&current) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(format!(
                "npm prefix 不是可信真实目录: {}",
                current.display()
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(format!("检查 npm prefix 失败: {error}")),
    }
    let relative = target.strip_prefix(prefix).map_err(|_| "npm 包路径越界")?;
    for component in relative.components() {
        current.push(component.as_os_str());
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(format!("npm 包路径包含非真实目录: {}", current.display()));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(format!("检查 npm 包路径失败: {error}")),
        }
    }
    Ok(true)
}

pub(crate) fn npm_package_artifacts_present_at(
    prefix: &std::path::Path,
    package: &str,
) -> Result<bool, String> {
    let expected = owned_npm_prefix()?;
    npm_package_artifacts_present_at_expected(prefix, package, &expected)
}

fn npm_package_artifacts_present_at_expected(
    prefix: &std::path::Path,
    package: &str,
    expected: &std::path::Path,
) -> Result<bool, String> {
    if !configured_path_matches(&prefix.display().to_string(), expected) {
        return Err("npm prefix 不是 CodeCLI 专属目录".into());
    }
    let package_dir = npm_package_dir(prefix, package)?;
    let parent = package_dir.parent().ok_or("npm 包路径没有父目录")?;
    if !validate_existing_directory_chain(prefix, parent)? {
        return Ok(false);
    }
    match std::fs::symlink_metadata(&package_dir) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => Err(format!(
            "npm 包副作用路径不是可信真实目录: {}",
            package_dir.display()
        )),
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!("检查 npm 包副作用失败: {error}")),
    }
}

pub(crate) fn npm_package_installed_at(
    prefix: &std::path::Path,
    package: &str,
) -> Result<bool, String> {
    let expected = owned_npm_prefix()?;
    npm_package_installed_at_expected(prefix, package, &expected)
}

fn npm_package_installed_at_expected(
    prefix: &std::path::Path,
    package: &str,
    expected: &std::path::Path,
) -> Result<bool, String> {
    if !configured_path_matches(&prefix.display().to_string(), expected) {
        return Err("npm prefix 不是 CodeCLI 专属目录".into());
    }
    let package_dir = npm_package_dir(prefix, package)?;
    if !validate_existing_directory_chain(prefix, &package_dir)? {
        return Ok(false);
    }
    let manifest_path = package_dir.join("package.json");
    let Some(bytes) = read_small_regular_file(&manifest_path, "npm package.json", 1024 * 1024)?
    else {
        return Err(format!(
            "npm 包目录存在但缺少可信 package.json: {}",
            package_dir.display()
        ));
    };
    let manifest: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|error| format!("npm package.json 损坏: {error}"))?;
    if manifest.get("name").and_then(|value| value.as_str()) != Some(package) {
        return Err(format!("npm package.json 包名不匹配，期望 {package}"));
    }
    Ok(true)
}

pub(crate) fn cli_launcher_paths(
    prefix: &std::path::Path,
    command: &str,
) -> Result<Vec<std::path::PathBuf>, String> {
    if !command
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        return Err("非法 CLI 命令名".into());
    }
    if cfg!(windows) {
        Ok(vec![
            prefix.join(command),
            prefix.join(format!("{command}.cmd")),
            prefix.join(format!("{command}.ps1")),
        ])
    } else {
        Ok(vec![prefix.join("bin").join(command)])
    }
}

pub(crate) fn npm_cli_artifacts_present(
    prefix: &std::path::Path,
    command: &str,
) -> Result<bool, String> {
    let expected = owned_npm_prefix()?;
    if !configured_path_matches(&prefix.display().to_string(), &expected) {
        return Err("CLI launcher prefix 不是 CodeCLI 专属目录".into());
    }
    for path in cli_launcher_paths(prefix, command)? {
        match std::fs::symlink_metadata(&path) {
            Ok(_) => return Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "检查 CLI launcher 失败 {}: {error}",
                    path.display()
                ))
            }
        }
    }
    Ok(false)
}

fn package_bin_script(
    prefix: &std::path::Path,
    package: &str,
    command: &str,
) -> Result<std::path::PathBuf, String> {
    if !npm_package_installed_at(prefix, package)? {
        return Err(format!("CodeCLI 专属 prefix 中未找到 {package}"));
    }
    let package_dir = npm_package_dir(prefix, package)?;
    let manifest_path = package_dir.join("package.json");
    let bytes = read_small_regular_file(&manifest_path, "npm package.json", 1024 * 1024)?
        .ok_or("npm package.json 不存在")?;
    let manifest: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|error| format!("npm package.json 损坏: {error}"))?;
    let relative = match manifest.get("bin") {
        Some(serde_json::Value::String(value)) => value.as_str(),
        Some(serde_json::Value::Object(map)) => {
            map.get(command)
                .and_then(|value| value.as_str())
                .ok_or_else(|| format!("npm 包未声明 {command} bin"))?
        }
        _ => return Err("npm package.json 未声明可用 bin".into()),
    };
    let relative_path = std::path::Path::new(relative);
    if relative_path.is_absolute()
        || relative_path.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir
                    | std::path::Component::RootDir
                    | std::path::Component::Prefix(_)
            )
        })
    {
        return Err("npm bin 路径越界".into());
    }
    let script = package_dir.join(relative_path);
    let metadata = std::fs::symlink_metadata(&script)
        .map_err(|error| format!("检查 npm bin 失败: {error}"))?;
    if metadata_is_link_or_reparse(&metadata) || !metadata.is_file() {
        return Err("npm bin 不是可信普通文件".into());
    }
    let canonical_package = std::fs::canonicalize(&package_dir)
        .map_err(|error| format!("解析 npm 包目录失败: {error}"))?;
    let canonical_script =
        std::fs::canonicalize(&script).map_err(|error| format!("解析 npm bin 失败: {error}"))?;
    if !canonical_script.starts_with(&canonical_package) {
        return Err("npm bin 解析后越界".into());
    }
    Ok(script)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NpmBinKind {
    NodeJs,
    Native,
}

fn classify_npm_bin_prefix(prefix: &[u8]) -> Option<NpmBinKind> {
    let first_line_end = prefix
        .iter()
        .position(|byte| *byte == b'\n')
        .unwrap_or(prefix.len());
    let first_line = prefix[..first_line_end]
        .strip_suffix(b"\r")
        .unwrap_or(&prefix[..first_line_end]);
    if first_line == b"#!/usr/bin/env node" {
        return Some(NpmBinKind::NodeJs);
    }

    // 不只看 2-4 字节魔数：同时核对 PE signature/machine、
    // Mach-O CPU/filetype 或 ELF type/machine，避免把 `MZ hello`
    // 之类普通文本误当成 native。
    if prefix.len() >= 0x40 && prefix.starts_with(b"MZ") {
        let pe_offset = u32::from_le_bytes(prefix[0x3c..0x40].try_into().ok()?) as usize;
        if pe_offset.checked_add(6)? <= prefix.len()
            && &prefix[pe_offset..pe_offset + 4] == b"PE\0\0"
        {
            let machine = u16::from_le_bytes(prefix[pe_offset + 4..pe_offset + 6].try_into().ok()?);
            if matches!(machine, 0x8664 | 0xaa64) {
                return Some(NpmBinKind::Native);
            }
        }
    }

    if prefix.len() >= 16 {
        let (little_endian, is_mach) = match &prefix[..4] {
            [0xce, 0xfa, 0xed, 0xfe] | [0xcf, 0xfa, 0xed, 0xfe] => (true, true),
            [0xfe, 0xed, 0xfa, 0xce] | [0xfe, 0xed, 0xfa, 0xcf] => (false, true),
            _ => (true, false),
        };
        if is_mach {
            let read_u32 = |offset: usize| {
                let bytes: [u8; 4] = prefix[offset..offset + 4].try_into().ok()?;
                Some(if little_endian {
                    u32::from_le_bytes(bytes)
                } else {
                    u32::from_be_bytes(bytes)
                })
            };
            let cpu_type = read_u32(4)?;
            let file_type = read_u32(12)?;
            if matches!(cpu_type, 0x0100_0007 | 0x0100_000c) && file_type == 2 {
                return Some(NpmBinKind::Native);
            }
        }
    }

    if prefix.len() >= 20 && prefix.starts_with(b"\x7fELF") {
        let little_endian = match prefix[5] {
            1 => true,
            2 => false,
            _ => return None,
        };
        let read_u16 = |offset: usize| {
            let bytes: [u8; 2] = prefix[offset..offset + 2].try_into().ok()?;
            Some(if little_endian {
                u16::from_le_bytes(bytes)
            } else {
                u16::from_be_bytes(bytes)
            })
        };
        let file_type = read_u16(16)?;
        let machine = read_u16(18)?;
        if matches!(file_type, 2 | 3) && matches!(machine, 62 | 183) {
            return Some(NpmBinKind::Native);
        }
    }
    None
}

fn npm_bin_kind(script: &std::path::Path) -> Result<NpmBinKind, String> {
    let before = std::fs::symlink_metadata(script)
        .map_err(|error| format!("检查 npm bin 类型失败: {error}"))?;
    if metadata_is_link_or_reparse(&before) || !before.is_file() || before.len() < 2 {
        return Err("npm bin 不是可识别的可信普通文件".into());
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
        .open(script)
        .map_err(|error| format!("安全打开 npm bin 失败: {error}"))?;
    let opened = file
        .metadata()
        .map_err(|error| format!("复查 npm bin 失败: {error}"))?;
    if metadata_is_link_or_reparse(&opened) || !opened.is_file() || opened.len() != before.len() {
        return Err("npm bin 在类型识别前发生替换".into());
    }
    let mut prefix = [0_u8; 4096];
    let read = file
        .read(&mut prefix)
        .map_err(|error| format!("读取 npm bin 文件头失败: {error}"))?;
    classify_npm_bin_prefix(&prefix[..read])
        .ok_or_else(|| "npm bin 既不是已审计 Node shebang，也不是可识别原生可执行文件".into())
}

fn native_cli_target_is_audited(
    prefix: &std::path::Path,
    command: &str,
    script: &std::path::Path,
) -> bool {
    if command != "claude" {
        return false;
    }
    let Ok(relative) = script.strip_prefix(prefix) else {
        return false;
    };
    matches!(
        relative
            .to_string_lossy()
            .replace('\\', "/")
            .to_ascii_lowercase()
            .as_str(),
        "node_modules/@anthropic-ai/claude-code/bin/claude.exe"
            | "lib/node_modules/@anthropic-ai/claude-code/bin/claude.exe"
    )
}

fn npm_bin_version_command(script: &std::path::Path, kind: NpmBinKind) -> Command {
    let mut command = match kind {
        NpmBinKind::NodeJs => {
            let mut process = Command::new("node");
            process.arg(script);
            process
        }
        NpmBinKind::Native => Command::new(script),
    };
    command.arg("--version");
    command
}

fn normalized_windows_launcher_target(
    prefix: &std::path::Path,
    script: &std::path::Path,
) -> Result<String, String> {
    let relative = script
        .strip_prefix(prefix)
        .map_err(|_| "Windows npm launcher 的 bin script 不在 prefix 内")?;
    let normalized_relative = relative
        .to_string_lossy()
        .replace('\\', "/")
        .trim_start_matches("./")
        .to_ascii_lowercase();
    if normalized_relative.is_empty() {
        return Err("Windows npm launcher 的 bin script 相对路径为空".into());
    }

    // npm 包名/bin 目录只需要这些安全字符。同时拒绝 batch、
    // PowerShell 和 POSIX shell 的元字符，使下面的整文件模板可证明。
    if !normalized_relative.chars().all(|character| {
        character.is_ascii_alphanumeric() || matches!(character, '/' | '.' | '@' | '-' | '_')
    }) {
        return Err("Windows npm launcher 的 bin script 路径含 shell 危险字符".into());
    }
    Ok(normalized_relative)
}

fn normalized_windows_launcher_body(bytes: &[u8], label: &str) -> Result<String, String> {
    if bytes.is_empty() || bytes.contains(&0) {
        return Err(format!("{label} 为空或包含 NUL"));
    }
    let body =
        std::str::from_utf8(bytes).map_err(|_| format!("{label} 不是可验证的 UTF-8/ASCII 文本"))?;
    if body.trim().is_empty() {
        return Err(format!("{label} 只包含空白"));
    }
    // Node writeFile 在 Windows 生成 .cmd 时是 CRLF，sh/ps1 是 LF。
    // 同时容忍文件经过无语义换行转换，但不容忍独立 CR。
    let normalized = body.replace("\r\n", "\n");
    if normalized.contains('\r') {
        return Err(format!("{label} 包含异常回车符"));
    }
    Ok(normalized
        .strip_suffix('\n')
        .unwrap_or(&normalized)
        .to_string())
}

fn windows_cmd_shim_v4_to_v8(target: &str) -> String {
    const TOKEN: &str = "__CODECLI_NPM_BIN_TARGET__";
    concat!(
        "@ECHO off\n",
        "GOTO start\n",
        ":find_dp0\n",
        "SET dp0=%~dp0\n",
        "EXIT /b\n",
        ":start\n",
        "SETLOCAL\n",
        "CALL :find_dp0\n",
        "\n",
        "IF EXIST \"%dp0%\\node.exe\" (\n",
        "  SET \"_prog=%dp0%\\node.exe\"\n",
        ") ELSE (\n",
        "  SET \"_prog=node\"\n",
        "  SET PATHEXT=%PATHEXT:;.JS;=;%\n",
        ")\n",
        "\n",
        "endLocal & goto #_undefined_# 2>NUL || title %COMSPEC% & ",
        "\"%_prog%\"  \"%dp0%\\__CODECLI_NPM_BIN_TARGET__\" %*"
    )
    .replace(TOKEN, target)
}

fn windows_cmd_shim_v9(target: &str) -> String {
    const TOKEN: &str = "__CODECLI_NPM_BIN_TARGET__";
    concat!(
        "@ECHO off\n",
        "GOTO start\n",
        ":find_dp0\n",
        "SET dp0=%~dp0\n",
        "EXIT /b\n",
        ":start\n",
        "SETLOCAL\n",
        "CALL :find_dp0\n",
        "\n",
        "IF EXIST \"%dp0%\\node.exe\" (\n",
        "  SET \"_prog=%dp0%\\node.exe\"\n",
        ") ELSE (\n",
        "  SET \"_prog=node\"\n",
        ")\n",
        "\n",
        "endLocal & goto #_undefined_# 2>NUL || title %COMSPEC% & ",
        "set PATHEXT=%PATHEXT:;.JS;=;% & ",
        "\"%_prog%\"  \"%dp0%\\__CODECLI_NPM_BIN_TARGET__\" %*"
    )
    .replace(TOKEN, target)
}

fn windows_cmd_shim_native(target: &str) -> String {
    const TOKEN: &str = "__CODECLI_NPM_BIN_TARGET__";
    // cmd-shim@4.1.0 到 @9.0.2 的 no-shebang/direct-exec .cmd 相同。
    concat!(
        "@ECHO off\n",
        "GOTO start\n",
        ":find_dp0\n",
        "SET dp0=%~dp0\n",
        "EXIT /b\n",
        ":start\n",
        "SETLOCAL\n",
        "CALL :find_dp0\n",
        "\"%dp0%\\__CODECLI_NPM_BIN_TARGET__\"   %*"
    )
    .replace(TOKEN, target)
}

pub(crate) fn validate_windows_cmd_launcher_content(
    prefix: &std::path::Path,
    script: &std::path::Path,
    bytes: &[u8],
) -> Result<(), String> {
    let normalized_relative = normalized_windows_launcher_target(prefix, script)?;
    let body = normalized_windows_launcher_body(bytes, "Windows npm .cmd launcher")?;
    let target = normalized_relative.replace('/', "\\");
    // 仅白名单从 cmd-shim@4.1.0/5.0.0/6.0.3/7.0.0/8.0.0/9.0.2
    // 实际源码与生成物提取的完整模板。v4-v8 的 .cmd 相同，
    // v9 另一套；它们覆盖项目允许的 Node 18+ 主线 npm。不做
    // contains 或命令行宽松解析，额外命令/管道/链式操作都 fail closed。
    let kind = npm_bin_kind(script)?;
    let matches = match kind {
        NpmBinKind::NodeJs => {
            body == windows_cmd_shim_v4_to_v8(&target) || body == windows_cmd_shim_v9(&target)
        }
        NpmBinKind::Native => body == windows_cmd_shim_native(&target),
    };
    if matches {
        Ok(())
    } else {
        Err(format!(
            "Windows npm .cmd launcher 不符合已审计 cmd-shim v4-v9 模板，未安全绑定已验证 bin: {normalized_relative}"
        ))
    }
}

fn windows_sh_shim_v4_to_v5(target: &str) -> String {
    const TOKEN: &str = "__CODECLI_NPM_BIN_TARGET__";
    concat!(
        "#!/bin/sh\n",
        "basedir=$(dirname \"$(echo \"$0\" | sed -e 's,\\\\,/,g')\")\n",
        "\n",
        "case `uname` in\n",
        "    *CYGWIN*|*MINGW*|*MSYS*) basedir=`cygpath -w \"$basedir\"`;;\n",
        "esac\n",
        "\n",
        "if [ -x \"$basedir/node\" ]; then\n",
        "  exec \"$basedir/node\"  \"$basedir/__CODECLI_NPM_BIN_TARGET__\" \"$@\"\n",
        "else \n",
        "  exec node  \"$basedir/__CODECLI_NPM_BIN_TARGET__\" \"$@\"\n",
        "fi"
    )
    .replace(TOKEN, target)
}

fn windows_sh_shim_v6_to_v8(target: &str) -> String {
    const TOKEN: &str = "__CODECLI_NPM_BIN_TARGET__";
    concat!(
        "#!/bin/sh\n",
        "basedir=$(dirname \"$(echo \"$0\" | sed -e 's,\\\\,/,g')\")\n",
        "\n",
        "case `uname` in\n",
        "    *CYGWIN*|*MINGW*|*MSYS*)\n",
        "        if command -v cygpath > /dev/null 2>&1; then\n",
        "            basedir=`cygpath -w \"$basedir\"`\n",
        "        fi\n",
        "    ;;\n",
        "esac\n",
        "\n",
        "if [ -x \"$basedir/node\" ]; then\n",
        "  exec \"$basedir/node\"  \"$basedir/__CODECLI_NPM_BIN_TARGET__\" \"$@\"\n",
        "else \n",
        "  exec node  \"$basedir/__CODECLI_NPM_BIN_TARGET__\" \"$@\"\n",
        "fi"
    )
    .replace(TOKEN, target)
}

fn windows_sh_shim_v9(target: &str) -> String {
    const TOKEN: &str = "__CODECLI_NPM_BIN_TARGET__";
    concat!(
        "#!/bin/sh\n",
        "basedir=$(dirname \"$(echo \"$0\" | sed -e 's,\\\\,/,g')\")\n",
        "basedir_win=\"$basedir\"\n",
        "\n",
        "case `uname -a` in\n",
        "  *CYGWIN*|*MINGW*|*MSYS*)\n",
        "    if command -v cygpath > /dev/null 2>&1; then\n",
        "      basedir_win=`cygpath -w \"$basedir\"`\n",
        "    fi\n",
        "  ;;\n",
        "  *WSL2*)\n",
        "    if command -v wslpath > /dev/null 2>&1; then\n",
        "      basedir_win=\"$(wslpath -w \"$basedir\" 2> /dev/null)\"\n",
        "      if [ $? -ne 0 ] || [ -z \"$basedir_win\" ]; then\n",
        "        echo \"Error: wslpath failed to convert path. WSL environment may be misconfigured.\" >&2\n",
        "        exit 1\n",
        "      fi\n",
        "    fi\n",
        "  ;;\n",
        "esac\n",
        "\n",
        "PROG_EXE=\"$basedir/node.exe\"\n",
        "if ! [ -x \"$PROG_EXE\" ]; then\n",
        "  PROG_EXE=\"$basedir/node\"\n",
        "  if ! [ -x \"$PROG_EXE\" ]; then\n",
        "    PROG_EXE=node\n",
        "    if ! [ -x \"$PROG_EXE\" ]; then\n",
        "      PROG_EXE=node.exe\n",
        "    fi\n",
        "  fi\n",
        "fi\n",
        "\n",
        "exec \"$PROG_EXE\"  \"$basedir_win/__CODECLI_NPM_BIN_TARGET__\" \"$@\""
    )
    .replace(TOKEN, target)
}

fn windows_sh_shim_native_v4_to_v5(target: &str) -> String {
    const TOKEN: &str = "__CODECLI_NPM_BIN_TARGET__";
    concat!(
        "#!/bin/sh\n",
        "basedir=$(dirname \"$(echo \"$0\" | sed -e 's,\\\\,/,g')\")\n",
        "\n",
        "case `uname` in\n",
        "    *CYGWIN*|*MINGW*|*MSYS*) basedir=`cygpath -w \"$basedir\"`;;\n",
        "esac\n",
        "\n",
        "exec \"$basedir/__CODECLI_NPM_BIN_TARGET__\"   \"$@\""
    )
    .replace(TOKEN, target)
}

fn windows_sh_shim_native_v6_to_v8(target: &str) -> String {
    const TOKEN: &str = "__CODECLI_NPM_BIN_TARGET__";
    concat!(
        "#!/bin/sh\n",
        "basedir=$(dirname \"$(echo \"$0\" | sed -e 's,\\\\,/,g')\")\n",
        "\n",
        "case `uname` in\n",
        "    *CYGWIN*|*MINGW*|*MSYS*)\n",
        "        if command -v cygpath > /dev/null 2>&1; then\n",
        "            basedir=`cygpath -w \"$basedir\"`\n",
        "        fi\n",
        "    ;;\n",
        "esac\n",
        "\n",
        "exec \"$basedir/__CODECLI_NPM_BIN_TARGET__\"   \"$@\""
    )
    .replace(TOKEN, target)
}

fn windows_sh_shim_native_v9(target: &str) -> String {
    const TOKEN: &str = "__CODECLI_NPM_BIN_TARGET__";
    concat!(
        "#!/bin/sh\n",
        "basedir=$(dirname \"$(echo \"$0\" | sed -e 's,\\\\,/,g')\")\n",
        "basedir_win=\"$basedir\"\n",
        "\n",
        "case `uname -a` in\n",
        "  *CYGWIN*|*MINGW*|*MSYS*)\n",
        "    if command -v cygpath > /dev/null 2>&1; then\n",
        "      basedir_win=`cygpath -w \"$basedir\"`\n",
        "    fi\n",
        "  ;;\n",
        "  *WSL2*)\n",
        "    if command -v wslpath > /dev/null 2>&1; then\n",
        "      basedir_win=\"$(wslpath -w \"$basedir\" 2> /dev/null)\"\n",
        "      if [ $? -ne 0 ] || [ -z \"$basedir_win\" ]; then\n",
        "        echo \"Error: wslpath failed to convert path. WSL environment may be misconfigured.\" >&2\n",
        "        exit 1\n",
        "      fi\n",
        "    fi\n",
        "  ;;\n",
        "esac\n",
        "\n",
        // 实查 v9 direct-exec 仍使用 basedir，而不是 basedir_win。
        "exec \"$basedir/__CODECLI_NPM_BIN_TARGET__\"   \"$@\""
    )
    .replace(TOKEN, target)
}

fn validate_windows_sh_launcher_content(
    prefix: &std::path::Path,
    script: &std::path::Path,
    bytes: &[u8],
) -> Result<(), String> {
    let target = normalized_windows_launcher_target(prefix, script)?;
    let body = normalized_windows_launcher_body(bytes, "Windows npm extensionless launcher")?;
    let kind = npm_bin_kind(script)?;
    let matches = match kind {
        NpmBinKind::NodeJs => {
            body == windows_sh_shim_v4_to_v5(&target)
                || body == windows_sh_shim_v6_to_v8(&target)
                || body == windows_sh_shim_v9(&target)
        }
        NpmBinKind::Native => {
            body == windows_sh_shim_native_v4_to_v5(&target)
                || body == windows_sh_shim_native_v6_to_v8(&target)
                || body == windows_sh_shim_native_v9(&target)
        }
    };
    if matches {
        Ok(())
    } else {
        Err(format!(
            "Windows npm extensionless launcher 不符合已审计 cmd-shim v4-v9 模板，未安全绑定已验证 bin: {target}"
        ))
    }
}

fn windows_ps1_shim_v4_to_v9(target: &str) -> String {
    const TOKEN: &str = "__CODECLI_NPM_BIN_TARGET__";
    // 实查 cmd-shim@4.1.0 到 @9.0.2 的 Node PowerShell 模板相同。
    concat!(
        "#!/usr/bin/env pwsh\n",
        "$basedir=Split-Path $MyInvocation.MyCommand.Definition -Parent\n",
        "\n",
        "$exe=\"\"\n",
        "if ($PSVersionTable.PSVersion -lt \"6.0\" -or $IsWindows) {\n",
        "  # Fix case when both the Windows and Linux builds of Node\n",
        "  # are installed in the same directory\n",
        "  $exe=\".exe\"\n",
        "}\n",
        "$ret=0\n",
        "if (Test-Path \"$basedir/node$exe\") {\n",
        "  # Support pipeline input\n",
        "  if ($MyInvocation.ExpectingInput) {\n",
        "    $input | & \"$basedir/node$exe\"  \"$basedir/__CODECLI_NPM_BIN_TARGET__\" $args\n",
        "  } else {\n",
        "    & \"$basedir/node$exe\"  \"$basedir/__CODECLI_NPM_BIN_TARGET__\" $args\n",
        "  }\n",
        "  $ret=$LASTEXITCODE\n",
        "} else {\n",
        "  # Support pipeline input\n",
        "  if ($MyInvocation.ExpectingInput) {\n",
        "    $input | & \"node$exe\"  \"$basedir/__CODECLI_NPM_BIN_TARGET__\" $args\n",
        "  } else {\n",
        "    & \"node$exe\"  \"$basedir/__CODECLI_NPM_BIN_TARGET__\" $args\n",
        "  }\n",
        "  $ret=$LASTEXITCODE\n",
        "}\n",
        "exit $ret"
    )
    .replace(TOKEN, target)
}

fn windows_ps1_shim_native_v4_to_v9(target: &str) -> String {
    const TOKEN: &str = "__CODECLI_NPM_BIN_TARGET__";
    concat!(
        "#!/usr/bin/env pwsh\n",
        "$basedir=Split-Path $MyInvocation.MyCommand.Definition -Parent\n",
        "\n",
        "$exe=\"\"\n",
        "if ($PSVersionTable.PSVersion -lt \"6.0\" -or $IsWindows) {\n",
        "  # Fix case when both the Windows and Linux builds of Node\n",
        "  # are installed in the same directory\n",
        "  $exe=\".exe\"\n",
        "}\n",
        "# Support pipeline input\n",
        "if ($MyInvocation.ExpectingInput) {\n",
        "  $input | & \"$basedir/__CODECLI_NPM_BIN_TARGET__\"   $args\n",
        "} else {\n",
        "  & \"$basedir/__CODECLI_NPM_BIN_TARGET__\"   $args\n",
        "}\n",
        "exit $LASTEXITCODE"
    )
    .replace(TOKEN, target)
}

/// 从已审计的 cmd-shim v9 模板生成一组 launcher，并立即用
/// `validate_cli_launchers` 整文件复验。调用者必须传入 fresh staging
/// prefix；本函数从不覆盖现有路径。
pub(crate) fn create_and_validate_cli_launchers(
    prefix: &std::path::Path,
    command: &str,
    script: &std::path::Path,
) -> Result<Vec<std::path::PathBuf>, String> {
    let paths = cli_launcher_paths(prefix, command)?;
    let kind = npm_bin_kind(script)?;
    if kind == NpmBinKind::Native && !native_cli_target_is_audited(prefix, command, script) {
        return Err("仅允许已审计 Claude Code package bin 使用原生直接执行模式".into());
    }

    #[cfg(windows)]
    {
        let target = normalized_windows_launcher_target(prefix, script)?;
        let bodies = match kind {
            NpmBinKind::NodeJs => vec![
                windows_sh_shim_v9(&target),
                windows_cmd_shim_v9(&target.replace('/', "\\")),
                windows_ps1_shim_v4_to_v9(&target),
            ],
            NpmBinKind::Native => vec![
                windows_sh_shim_native_v9(&target),
                windows_cmd_shim_native(&target.replace('/', "\\")),
                windows_ps1_shim_native_v4_to_v9(&target),
            ],
        };
        for (path, body) in paths.iter().zip(bodies) {
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            let mut file = options.open(path).map_err(|error| {
                format!("创建 Windows CLI launcher 失败 {}: {error}", path.display())
            })?;
            use std::io::Write as _;
            file.write_all(body.as_bytes())
                .map_err(|error| format!("写入 Windows CLI launcher 失败: {error}"))?;
            file.sync_all()
                .map_err(|error| format!("持久化 Windows CLI launcher 失败: {error}"))?;
        }
    }
    #[cfg(not(windows))]
    {
        let bin = prefix.join("bin");
        std::fs::create_dir_all(&bin)
            .map_err(|error| format!("创建 npm bin staging 失败: {error}"))?;
        let relative = script
            .strip_prefix(prefix)
            .map_err(|_| "Unix npm bin 不在 staging prefix 内")?;
        let target = std::path::Path::new("..").join(relative);
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &paths[0])
            .map_err(|error| format!("创建 Unix CLI launcher 失败: {error}"))?;
        #[cfg(not(unix))]
        return Err("当前平台无法创建 Unix CLI launcher".into());
    }
    validate_cli_launchers(prefix, command, script)?;
    Ok(paths)
}

fn validate_windows_ps1_launcher_content(
    prefix: &std::path::Path,
    script: &std::path::Path,
    bytes: &[u8],
) -> Result<(), String> {
    let target = normalized_windows_launcher_target(prefix, script)?;
    let body = normalized_windows_launcher_body(bytes, "Windows npm .ps1 launcher")?;
    let kind = npm_bin_kind(script)?;
    let matches = match kind {
        NpmBinKind::NodeJs => body == windows_ps1_shim_v4_to_v9(&target),
        NpmBinKind::Native => body == windows_ps1_shim_native_v4_to_v9(&target),
    };
    if matches {
        Ok(())
    } else {
        Err(format!(
            "Windows npm .ps1 launcher 不符合已审计 cmd-shim v4-v9 模板，未安全绑定已验证 bin: {target}"
        ))
    }
}

pub(crate) fn validate_cli_launchers(
    prefix: &std::path::Path,
    command: &str,
    script: &std::path::Path,
) -> Result<(), String> {
    let paths = cli_launcher_paths(prefix, command)?;
    let bin_kind = npm_bin_kind(script)?;
    if bin_kind == NpmBinKind::Native && !native_cli_target_is_audited(prefix, command, script) {
        return Err("仅允许已审计 Claude Code package bin 使用原生直接执行模式".into());
    }
    if cfg!(windows) {
        // cmd-shim@4-@9 都会一起生成 extensionless sh、.cmd 与
        // .ps1 三个入口。PowerShell 默认可优先命中 .ps1，因此不能
        // 只验 .cmd 再直接运行 package bin 就宣称 Installed。
        for (index, path) in paths.iter().enumerate() {
            let metadata = match std::fs::symlink_metadata(path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    return Err(format!(
                        "npm 包存在但缺少必需 Windows launcher: {}",
                        path.display()
                    ));
                }
                Err(error) => {
                    return Err(format!(
                        "检查 CLI launcher 失败 {}: {error}",
                        path.display()
                    ));
                }
            };
            if metadata_is_link_or_reparse(&metadata)
                || !metadata.is_file()
                || metadata.len() == 0
                || metadata.len() > 64 * 1024
            {
                return Err(format!("CLI launcher 类型/大小异常: {}", path.display()));
            }
            let bytes = read_small_regular_file(path, "Windows npm launcher", 64 * 1024)?
                .ok_or_else(|| format!("npm 包存在但 launcher 消失: {}", path.display()))?;
            match index {
                0 => validate_windows_sh_launcher_content(prefix, script, &bytes)?,
                1 => validate_windows_cmd_launcher_content(prefix, script, &bytes)?,
                2 => validate_windows_ps1_launcher_content(prefix, script, &bytes)?,
                _ => return Err("内部错误：Windows npm launcher 集合超出审计范围".into()),
            }
        }
        if paths.len() != 3 {
            return Err("内部错误：Windows npm launcher 集合不完整".into());
        }
        return Ok(());
    }

    let Some(path) = paths.first() else {
        return Err(format!("npm 包存在但缺少 {command} launcher"));
    };
    {
        let metadata = match std::fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(format!("npm 包存在但缺少 {command} launcher"));
            }
            Err(error) => {
                return Err(format!(
                    "检查 CLI launcher 失败 {}: {error}",
                    path.display()
                ))
            }
        };
        if !metadata.file_type().is_symlink() {
            return Err(format!(
                "npm Unix launcher 不是符号链接: {}",
                path.display()
            ));
        }
        let resolved = std::fs::canonicalize(path)
            .map_err(|error| format!("解析 npm launcher 失败: {error}"))?;
        let expected =
            std::fs::canonicalize(script).map_err(|error| format!("解析 npm bin 失败: {error}"))?;
        if resolved != expected {
            return Err(format!(
                "npm launcher 未指向归属包的 bin: {}",
                path.display()
            ));
        }
    }
    Ok(())
}

pub(crate) fn verify_owned_npm_cli(
    prefix: &std::path::Path,
    package: &str,
    command: &str,
) -> Result<String, String> {
    let script = package_bin_script(prefix, package, command)?;
    validate_cli_launchers(prefix, command, &script)?;
    check_cancelled()?;
    let bin_kind = npm_bin_kind(&script)?;
    if bin_kind == NpmBinKind::Native && package != "@anthropic-ai/claude-code" {
        return Err("未审计的 npm 包不得直接执行原生 package bin".into());
    }
    if bin_kind == NpmBinKind::NodeJs && which_cmd("node").is_none() {
        return Err("无法验证 CLI：找不到 node".into());
    }
    let command_process = npm_bin_version_command(&script, bin_kind);
    let output = run_timed(command_process, 30)?;
    if !output.status_ok {
        return Err(format!(
            "{command} --version 退出非 0: {}",
            output
                .stderr
                .chars()
                .chain(output.stdout.chars())
                .take(160)
                .collect::<String>()
        ));
    }
    output
        .stdout
        .lines()
        .chain(output.stderr.lines())
        .find(|line| !line.trim().is_empty())
        .map(|line| line.trim().to_string())
        .ok_or_else(|| format!("{command} --version 无输出"))
}

fn validate_cli_record_at(
    record: &CliInstallRecord,
    expected_package: &str,
    expected_prefix: &std::path::Path,
) -> Result<(), String> {
    if record.schema_version != CLI_OWNERSHIP_SCHEMA_VERSION {
        return Err(format!(
            "CLI ownership schema 版本不受支持: {}",
            record.schema_version
        ));
    }
    if record.package != expected_package {
        return Err("CLI ownership 包名不匹配".into());
    }
    safe_npm_package_components(&record.package)?;
    if record
        .version
        .as_deref()
        .is_some_and(|version| version.is_empty() || version.len() > 256)
        || record.updated_at.is_empty()
        || record.updated_at.len() > 64
        || !record
            .updated_at
            .chars()
            .all(|character| character.is_ascii_digit())
    {
        return Err("CLI ownership 字段长度或格式异常".into());
    }
    if !record.prefix.is_absolute()
        || !configured_path_matches(&record.prefix.display().to_string(), expected_prefix)
    {
        return Err("CLI ownership prefix 不是 CodeCLI 专属 npm prefix".into());
    }
    if record.state == CliInstallState::Installed
        && (record.version.is_none() || record.receipt.is_none())
    {
        return Err("Installed CLI ownership 缺少版本或发布前内容收据".into());
    }
    if let Some(receipt) = &record.receipt {
        let command = match record.package.as_str() {
            "@anthropic-ai/claude-code" => "claude",
            "@openai/codex" => "codex",
            _ => return Err("CLI ownership 包不在内嵌收据白名单".into()),
        };
        super::pinned_npm::validate_receipt_shape(
            &record.prefix,
            &record.package,
            command,
            receipt,
        )?;
    }
    Ok(())
}

fn parse_cli_install_record(
    bytes: &[u8],
    expected_package: &str,
    expected_prefix: &std::path::Path,
) -> Result<CliInstallRecord, String> {
    let record: CliInstallRecord = serde_json::from_slice(bytes)
        .map_err(|error| format!("CLI ownership 损坏或为旧版不安全格式: {error}"))?;
    validate_cli_record_at(&record, expected_package, expected_prefix)?;
    Ok(record)
}

pub(crate) fn load_cli_install_record(
    path: &std::path::Path,
    expected_package: &str,
) -> Result<Option<CliInstallRecord>, String> {
    let Some(bytes) = read_small_regular_file(path, "CLI ownership", MAX_CLI_OWNERSHIP_BYTES)?
    else {
        return Ok(None);
    };
    let expected_prefix = owned_npm_prefix()?;
    parse_cli_install_record(&bytes, expected_package, &expected_prefix).map(Some)
}

pub(crate) fn persist_cli_install_record(
    path: &std::path::Path,
    record: &CliInstallRecord,
    expected_previous: Option<&CliInstallRecord>,
) -> Result<(), String> {
    let expected_prefix = owned_npm_prefix()?;
    validate_cli_record_at(record, &record.package, &expected_prefix)?;
    let current = load_cli_install_record(path, &record.package)?;
    if current.as_ref() != expected_previous {
        return Err("CLI ownership 在操作期间被外部修改，已拒绝覆盖".into());
    }
    let parent = path.parent().ok_or("CLI ownership 路径没有父目录")?;
    ensure_real_directory(parent, "CodeCLI 状态目录")?;
    let body = serde_json::to_string_pretty(record)
        .map_err(|error| format!("序列化 CLI ownership 失败: {error}"))?;
    atomic_write_mode(path, &body, true)
}

pub(crate) fn remove_cli_install_record(
    path: &std::path::Path,
    expected: &CliInstallRecord,
) -> Result<(), String> {
    let current = load_cli_install_record(path, &expected.package)?;
    if current.as_ref() != Some(expected) {
        return Err("CLI ownership 在卸载期间被外部修改，已保留记录".into());
    }
    remove_file_durable(path).map_err(|error| format!("持久删除 CLI ownership 失败: {error}"))
}

fn directory_has_unknown_npm_payload_inner(
    path: &std::path::Path,
    allowed_lock: &std::path::Path,
) -> Result<bool, String> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(format!("检查 npm prefix 失败 {}: {error}", path.display())),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(format!("npm prefix 不是可信目录: {}", path.display()));
    }
    for entry in std::fs::read_dir(path)
        .map_err(|error| format!("读取 npm prefix 失败 {}: {error}", path.display()))?
    {
        let entry = entry.map_err(|error| format!("读取 npm prefix 条目失败: {error}"))?;
        let file_type = entry
            .file_type()
            .map_err(|error| format!("读取 npm prefix 类型失败: {error}"))?;
        if file_type.is_symlink() {
            return Ok(true);
        }
        if file_type.is_dir() {
            if directory_has_unknown_npm_payload_inner(&entry.path(), allowed_lock)? {
                return Ok(true);
            }
            continue;
        }
        // npm 只会在 global node_modules 根中保留这个内部 lock。
        // 嵌套包目录或 prefix 其他位置的同名文件仍是未知负载，
        // 不能因 basename 相同而在 purge 时被误删。
        if !file_type.is_file() || entry.path() != allowed_lock {
            return Ok(true);
        }
    }
    Ok(false)
}

fn directory_has_unknown_npm_payload(prefix: &std::path::Path) -> Result<bool, String> {
    let allowed_lock = npm_modules_root(prefix).join(".package-lock.json");
    directory_has_unknown_npm_payload_inner(prefix, &allowed_lock)
}

fn reset_legacy_global_npm_prefix(prefix: &std::path::Path) -> Result<bool, String> {
    if which_cmd("npm").is_none() {
        return match std::fs::symlink_metadata(prefix) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Ok(_) => Err(
                "检测到旧版 CodeCLI npm prefix，但当前找不到 npm，无法确认并恢复用户 npm 配置；请先恢复 npm 后重试完整卸载"
                    .into(),
            ),
            Err(error) => Err(format!("检查旧版 npm prefix 失败: {error}")),
        };
    }
    let mut get = npm_command()?;
    get.args(["config", "get", "prefix"]);
    let current = run_timed(get, 30)?;
    if !current.status_ok {
        return Err(format!(
            "读取 npm prefix 失败: {}",
            humanize_npm_err(&current.stderr)
        ));
    }
    let configured = current.stdout.trim();
    if !configured_path_matches(configured, prefix) {
        return Ok(false);
    }

    // 兼容修复旧版曾执行过的 `npm config set prefix ...`。
    let mut delete = npm_command()?;
    delete.args(["config", "delete", "prefix"]);
    let deleted = run_timed(delete, 30)?;
    if !deleted.status_ok {
        return Err(format!(
            "恢复旧版 npm prefix 失败: {}",
            humanize_npm_err(&deleted.stderr)
        ));
    }
    let mut verify = npm_command()?;
    verify.args(["config", "get", "prefix"]);
    let verified = run_timed(verify, 30)?;
    if !verified.status_ok || verified.stdout.trim() == configured {
        return Err("旧版 npm prefix 删除后复查失败，已拒绝删除工具数据".into());
    }
    Ok(true)
}

fn configured_path_matches(configured: &str, expected: &std::path::Path) -> bool {
    let configured = configured.trim().trim_matches('"').trim_matches('\'');
    let expected = expected.display().to_string();
    if cfg!(windows) {
        let normalize = |value: &str| {
            value
                .replace('/', "\\")
                .trim_end_matches('\\')
                .to_lowercase()
        };
        normalize(configured) == normalize(&expected)
    } else {
        configured.trim_end_matches('/') == expected.trim_end_matches('/')
    }
}

/// purge 已持有全局操作锁，并应在 CLI/扩展卸载完成后调用。
pub(crate) fn prepare_runtime_state_for_purge() -> Result<Vec<String>, String> {
    let state = super::platform::codecli_state_dir().ok_or("找不到工具状态目录")?;
    for record in ["claude-code.json", "codex-cli.json"] {
        let path = state.join(record);
        match std::fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Ok(metadata) => {
                let kind = if metadata.file_type().is_symlink() {
                    "符号链接/重解析点"
                } else if metadata.is_file() {
                    "文件"
                } else {
                    "异常类型"
                };
                return Err(format!(
                    "仍有 CLI 安装归属记录 {record}（{kind}），请先完成 CLI 卸载再清理工具数据"
                ));
            }
            Err(error) => {
                return Err(format!("检查 CLI 安装归属记录 {record} 失败: {error}"));
            }
        }
    }
    let prefix = state.join("npm-global");
    if directory_has_unknown_npm_payload(&prefix)? {
        return Err(format!(
            "{} 中仍有无法证明归属的 npm 文件/包，为防误删已保留状态目录；请手工备份处理后重试",
            prefix.display()
        ));
    }

    let mut written = Vec::new();
    if reset_legacy_global_npm_prefix(&prefix)? {
        written.push("restored:legacy-npm-prefix".into());
    }
    if cfg!(windows) {
        let node = state.join("runtime/node");
        remove_user_path_segment_windows(&node.display().to_string())?;
        remove_user_path_segment_windows(&prefix.display().to_string())?;
        let mut current = std::env::var("PATH").unwrap_or_default();
        current = current
            .split(';')
            .filter(|part| {
                !part.eq_ignore_ascii_case(&node.display().to_string())
                    && !part.eq_ignore_ascii_case(&prefix.display().to_string())
            })
            .collect::<Vec<_>>()
            .join(";");
        unsafe { std::env::set_var("PATH", current) };
        written.push("removed:windows-runtime-path".into());
    } else {
        written.extend(remove_tool_runtime_path_blocks()?);
    }
    Ok(written)
}

/// 解析版本数字用于比较（粗粒度 major.minor.patch）
pub fn version_tuple(ver: &str) -> Option<(u32, u32, u32)> {
    let v = ver.trim().trim_start_matches('v');
    // 从字符串中抓第一段 x.y.z
    let mut num = String::new();
    for c in v.chars() {
        if c.is_ascii_digit() || c == '.' {
            num.push(c);
        } else if !num.is_empty() {
            break;
        }
    }
    let mut it = num.split('.');
    let a = it.next()?.parse().ok()?;
    let b = it.next().and_then(|x| x.parse().ok()).unwrap_or(0);
    let c = it.next().and_then(|x| x.parse().ok()).unwrap_or(0);
    Some((a, b, c))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_SEQUENCE: AtomicU64 = AtomicU64::new(1);

    fn test_dir(label: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "codecli-runtime-{label}-{}-{}",
            std::process::id(),
            TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).expect("create test directory");
        path
    }

    #[test]
    fn version_tuple_parses_plain_and_prefixed_versions() {
        assert_eq!(version_tuple("1.0.0"), Some((1, 0, 0)));
        assert_eq!(version_tuple("2.1.0"), Some((2, 1, 0)));
        assert_eq!(version_tuple("claude 1.2.3 something"), Some((1, 2, 3)));
        assert_eq!(version_tuple("unknown"), None);
    }

    #[cfg(unix)]
    #[test]
    fn node_version_probe_times_out_instead_of_hanging_on_shim() {
        super::super::cmd::clear_cancel();
        let mut command = Command::new("sh");
        command.args(["-c", "sleep 30"]);
        let started = std::time::Instant::now();
        assert_eq!(node_version_from_command(command, 1), None);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "hung node shim must be terminated by run_timed"
        );
    }

    #[test]
    fn pinned_node_archives_cover_supported_fallbacks() {
        for name in [
            "node-v22.19.0-darwin-arm64.tar.gz",
            "node-v22.19.0-darwin-x64.tar.gz",
            "node-v22.19.0-win-arm64.zip",
            "node-v22.19.0-win-x64.zip",
        ] {
            let sha = expected_node_archive_sha256(name).expect("supported archive pin");
            assert_eq!(sha.len(), 64);
            assert!(sha.chars().all(|character| character.is_ascii_hexdigit()));
        }
        assert!(expected_node_archive_sha256("node-v22.19.0-win-x86.zip").is_none());
    }

    #[test]
    fn node_upgrade_ownership_requires_exact_real_codecli_runtime() {
        let state = test_dir("node-owned");
        let expected = owned_node_executable_for_state(&state);
        std::fs::create_dir_all(expected.parent().expect("node parent"))
            .expect("create owned node tree");
        std::fs::write(&expected, b"test node executable").expect("owned node");
        assert!(node_path_is_owned_by_codecli(&expected, &state));

        let external = state
            .join("external")
            .join(if cfg!(windows) { "node.exe" } else { "node" });
        std::fs::create_dir_all(external.parent().unwrap()).expect("external parent");
        std::fs::write(&external, b"external node").expect("external node");
        assert!(!node_path_is_owned_by_codecli(&external, &state));

        std::fs::remove_file(&expected).expect("remove regular node");
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            symlink(&external, &expected).expect("linked node");
            assert!(!node_path_is_owned_by_codecli(&expected, &state));
        }
        #[cfg(windows)]
        assert!(!node_path_is_owned_by_codecli(&expected, &state));

        std::fs::remove_dir_all(state).expect("cleanup");
    }

    #[test]
    fn windows_npm_command_uses_node_directly_without_shell_parsing() {
        let directory = test_dir("windows-npm-command").join("Node & Tools");
        let npm_shim = directory.join("npm.cmd");
        let node = directory.join("node.exe");
        let npm_cli = directory.join("node_modules/npm/bin/npm-cli.js");
        std::fs::create_dir_all(npm_cli.parent().unwrap()).expect("npm cli directory");
        std::fs::write(&npm_shim, "@echo off\r\n").expect("npm.cmd");
        std::fs::write(&node, b"test node executable").expect("node.exe");
        std::fs::write(&npm_cli, b"// test npm cli").expect("npm-cli.js");

        let prefix = directory.join("prefix & user data");
        let mut command =
            windows_npm_command_from_shim(&npm_shim).expect("ordinary absolute npm entrypoints");
        command
            .args(["install", "-g", "@openai/codex", "--prefix"])
            .arg(&prefix);
        assert_eq!(command.get_program(), node.as_os_str());
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            vec![
                npm_cli.as_os_str(),
                std::ffi::OsStr::new("install"),
                std::ffi::OsStr::new("-g"),
                std::ffi::OsStr::new("@openai/codex"),
                std::ffi::OsStr::new("--prefix"),
                prefix.as_os_str(),
            ]
        );
        assert!(
            command
                .get_args()
                .all(|argument| argument != "/C" && argument != "cmd"),
            "dynamic npm arguments must never pass through cmd.exe"
        );

        std::fs::remove_dir_all(directory.parent().unwrap()).expect("cleanup");
    }

    #[test]
    fn windows_npm_command_supports_direct_exe_shim_without_shell() {
        let directory = test_dir("windows-npm-exe-command").join("Volta & Tools");
        std::fs::create_dir_all(&directory).expect("Volta shim directory");
        let unsupported = directory.join("npm");
        let npm_exe = directory.join("npm.exe");
        std::fs::write(&unsupported, b"unix compatibility shim").expect("extensionless npm");
        std::fs::write(&npm_exe, b"test executable shim").expect("npm.exe");

        let prefix = directory.join("prefix & user data");
        let mut command = windows_npm_command_from_candidates([&unsupported, &npm_exe])
            .expect("direct npm.exe is a safe argv entrypoint");
        command.args(["root", "-g", "--prefix"]).arg(&prefix);
        assert_eq!(command.get_program(), npm_exe.as_os_str());
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            vec![
                std::ffi::OsStr::new("root"),
                std::ffi::OsStr::new("-g"),
                std::ffi::OsStr::new("--prefix"),
                prefix.as_os_str(),
            ]
        );
        assert!(command.get_args().all(|argument| argument != "/C"));

        std::fs::remove_dir_all(directory.parent().unwrap()).expect("cleanup");
    }

    #[test]
    fn windows_npm_command_rejects_non_absolute_shim() {
        let error = windows_npm_command_from_shim(std::path::Path::new("npm.cmd"))
            .expect_err("relative npm.cmd must fail closed");
        assert!(error.contains("不是绝对路径"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn windows_npm_command_rejects_linked_shim() {
        use std::os::unix::fs::symlink;

        let directory = test_dir("windows-npm-linked-command");
        let actual = directory.join("actual-npm.cmd");
        let linked = directory.join("npm.cmd");
        std::fs::write(&actual, "@echo off\r\n").expect("actual npm.cmd");
        symlink(&actual, &linked).expect("linked npm.cmd");

        let error =
            windows_npm_command_from_shim(&linked).expect_err("linked npm.cmd must fail closed");
        assert!(error.contains("不是可信普通文件"), "{error}");
        std::fs::remove_dir_all(directory).expect("cleanup");
    }

    #[test]
    fn windows_cmd_launcher_must_match_audited_cmd_shim_template() {
        let prefix = test_dir("windows-cmd-launcher");
        let script = prefix.join("node_modules/@openai/codex/bin/codex.js");
        std::fs::create_dir_all(script.parent().unwrap()).expect("package bin directory");
        std::fs::write(&script, b"#!/usr/bin/env node\n").expect("Node package bin");
        let valid = br#"@ECHO off
GOTO start
:find_dp0
SET dp0=%~dp0
EXIT /b
:start
SETLOCAL
CALL :find_dp0

IF EXIST "%dp0%\node.exe" (
  SET "_prog=%dp0%\node.exe"
) ELSE (
  SET "_prog=node"
  SET PATHEXT=%PATHEXT:;.JS;=;%
)

endLocal & goto #_undefined_# 2>NUL || title %COMSPEC% & "%_prog%"  "%dp0%\node_modules\@openai\codex\bin\codex.js" %*
"#;
        validate_windows_cmd_launcher_content(&prefix, &script, valid)
            .expect("npm cmd shim binds exact package bin");
        let valid_crlf = String::from_utf8(valid.to_vec())
            .expect("fixture is UTF-8")
            .replace('\n', "\r\n");
        validate_windows_cmd_launcher_content(&prefix, &script, valid_crlf.as_bytes())
            .expect("the exact npm cmd shim may use CRLF");

        let comment_only = br#":: node_modules\@openai\codex\bin\codex.js %*
@"%~dp0\node.exe" "%~dp0\node_modules\evil\bin.js" %*
"#;
        let error = validate_windows_cmd_launcher_content(&prefix, &script, comment_only)
            .expect_err("comment must not prove launcher binding");
        assert!(error.contains("未安全绑定已验证 bin"), "{error}");

        let echo_only = br#"@ECHO off
@echo node_modules\@openai\codex\bin\codex.js %*
"#;
        validate_windows_cmd_launcher_content(&prefix, &script, echo_only)
            .expect_err("echoing the expected path is not an invocation proof");

        let malicious_prefix = String::from_utf8(valid.to_vec())
            .expect("fixture is UTF-8")
            .replace(
                "endLocal & goto #_undefined_# 2>NUL || title %COMSPEC% &",
                "del /Q C:\\important.txt & endLocal & goto #_undefined_# 2>NUL || title %COMSPEC% &",
            );
        validate_windows_cmd_launcher_content(&prefix, &script, malicious_prefix.as_bytes())
            .expect_err("an arbitrary command before the valid invocation must fail closed");

        let malicious_suffix = String::from_utf8(valid.to_vec())
            .expect("fixture is UTF-8")
            .replace(" %*\n", " %* & calc.exe\n");
        validate_windows_cmd_launcher_content(&prefix, &script, malicious_suffix.as_bytes())
            .expect_err("a chained command after the valid invocation must fail closed");

        let extra_line = String::from_utf8(valid.to_vec())
            .expect("fixture is UTF-8")
            .replacen("GOTO start\n", "GOTO start\ndel /Q C:\\important.txt\n", 1);
        validate_windows_cmd_launcher_content(&prefix, &script, extra_line.as_bytes())
            .expect_err("an extra batch command must fail closed");

        assert!(validate_windows_cmd_launcher_content(&prefix, &script, b"").is_err());
        assert!(validate_windows_cmd_launcher_content(&prefix, &script, b"   \r\n").is_err());
        std::fs::remove_dir_all(prefix).expect("cleanup");
    }

    fn fixture_sha256(template_without_final_newline: &str) -> String {
        use sha2::{Digest, Sha256};

        let mut fixture = template_without_final_newline.as_bytes().to_vec();
        fixture.push(b'\n');
        hex::encode(Sha256::digest(&fixture))
    }

    #[test]
    fn audited_cmd_shim_templates_match_real_v4_to_v9_generated_fixtures() {
        let forward = "node_modules/@openai/codex/bin/codex.js";
        let backward = r"node_modules\@openai\codex\bin\codex.js";
        // 指纹来自 cmd-shim 4.1.0/5.0.0/6.0.3/7.0.0/8.0.0/9.0.2
        // 对同一 `#!/usr/bin/env node` fixture 的实际生成物，换行归一为 LF。
        let node_templates = [
            (
                windows_sh_shim_v4_to_v5(forward),
                "948f713a5ac6c0c81ac1fbc134c1094d6c6ff3684b1b518b573f6355652afa84",
            ),
            (
                windows_sh_shim_v6_to_v8(forward),
                "6ee176b43f96b2294c8e2265d599f829e161b2c2ad34cb2b8fbf5f836fd34b8c",
            ),
            (
                windows_sh_shim_v9(forward),
                "508f6f63b9a11fbba698712e331e1667df029eac8f8559d8ef12afe455f2fafc",
            ),
            (
                windows_cmd_shim_v4_to_v8(backward),
                "0fd52614bcc23d6bf2ef74408a948452157d6ab77c02db200a351a499a3df5e6",
            ),
            (
                windows_cmd_shim_v9(backward),
                "68534a5bb4078415c1bc02b5692f17dd5d459336b51f68d01a4d4fa8e1901625",
            ),
            (
                windows_ps1_shim_v4_to_v9(forward),
                "0c149db80ed0bf442c810146b0ad0163b74982fe4542d673f56c354d7b8229cb",
            ),
        ];
        for (template, expected) in node_templates {
            assert_eq!(fixture_sha256(&template), expected);
        }

        // 同样对 no-shebang/native direct-exec 生成物做独立指纹对照。
        let native_templates = [
            (
                windows_sh_shim_native_v4_to_v5(forward),
                "fd1f1a43e0cf59209c6e9bb3dc3abd71ab01e3c65cfcce80e6ef9bead4006064",
            ),
            (
                windows_sh_shim_native_v6_to_v8(forward),
                "a00d8a6686af7021097d56212e4d5c23ef05fcb45555f7716038df287a23cad9",
            ),
            (
                windows_sh_shim_native_v9(forward),
                "b31131e387a90cd90c609c7f2f4c4262c3a12a0c656b992ebdeee0e84fd0f295",
            ),
            (
                windows_cmd_shim_native(backward),
                "644db7d0b778e6b80ba71196562b5663f6584db8d53f64ff55cf72af00451dfa",
            ),
            (
                windows_ps1_shim_native_v4_to_v9(forward),
                "c86fe1769267ed78310dcb1c9b5284b4c6aae9beb227faadd57401d9278013ed",
            ),
        ];
        for (template, expected) in native_templates {
            assert_eq!(fixture_sha256(&template), expected);
        }
    }

    #[test]
    fn all_windows_node_launchers_require_exact_whole_file_templates() {
        let prefix = test_dir("windows-node-launcher-set");
        let script = prefix.join("node_modules/@openai/codex/bin/codex.js");
        std::fs::create_dir_all(script.parent().unwrap()).expect("package bin directory");
        std::fs::write(&script, b"#!/usr/bin/env node\n").expect("Node package bin");
        let forward = "node_modules/@openai/codex/bin/codex.js";
        let backward = r"node_modules\@openai\codex\bin\codex.js";

        for template in [
            windows_cmd_shim_v4_to_v8(backward),
            windows_cmd_shim_v9(backward),
        ] {
            let lf = format!("{template}\n");
            validate_windows_cmd_launcher_content(&prefix, &script, lf.as_bytes())
                .expect("real Node .cmd template");
            let crlf = lf.replace('\n', "\r\n");
            validate_windows_cmd_launcher_content(&prefix, &script, crlf.as_bytes())
                .expect("real Node .cmd template with CRLF");
        }
        for template in [
            windows_sh_shim_v4_to_v5(forward),
            windows_sh_shim_v6_to_v8(forward),
            windows_sh_shim_v9(forward),
        ] {
            let lf = format!("{template}\n");
            validate_windows_sh_launcher_content(&prefix, &script, lf.as_bytes())
                .expect("real Node extensionless template");
            let crlf = lf.replace('\n', "\r\n");
            validate_windows_sh_launcher_content(&prefix, &script, crlf.as_bytes())
                .expect("line-ending conversion is semantically neutral");
        }
        let ps1 = format!("{}\n", windows_ps1_shim_v4_to_v9(forward));
        validate_windows_ps1_launcher_content(&prefix, &script, ps1.as_bytes())
            .expect("real Node PowerShell template");
        validate_windows_ps1_launcher_content(
            &prefix,
            &script,
            ps1.replace('\n', "\r\n").as_bytes(),
        )
        .expect("PowerShell CRLF conversion is semantically neutral");

        let malicious_sh = format!("{}\nrm -rf \"$HOME\"", windows_sh_shim_v9(forward));
        validate_windows_sh_launcher_content(&prefix, &script, malicious_sh.as_bytes())
            .expect_err("extra sh command must fail closed");
        let malicious_ps1 = format!(
            "{}\nRemove-Item -Recurse $HOME",
            windows_ps1_shim_v4_to_v9(forward)
        );
        validate_windows_ps1_launcher_content(&prefix, &script, malicious_ps1.as_bytes())
            .expect_err("extra PowerShell command must fail closed");
        let wrong_ps1 = windows_ps1_shim_v4_to_v9("node_modules/evil/bin.js");
        validate_windows_ps1_launcher_content(&prefix, &script, wrong_ps1.as_bytes())
            .expect_err("wrong PowerShell target must fail closed");

        std::fs::remove_dir_all(prefix).expect("cleanup");
    }

    #[test]
    fn claude_native_bin_uses_only_exact_direct_exec_launchers() {
        let prefix = test_dir("windows-native-launcher-set");
        let script = prefix.join("node_modules/@anthropic-ai/claude-code/bin/claude.exe");
        std::fs::create_dir_all(script.parent().unwrap()).expect("package bin directory");
        // 构造有 DOS header、PE signature 与 x86_64 machine 的最小文件头，
        // 与当前 Claude Windows package bin 类型对齐。
        let mut pe = vec![0_u8; 0x88];
        pe[..2].copy_from_slice(b"MZ");
        pe[0x3c..0x40].copy_from_slice(&0x80_u32.to_le_bytes());
        pe[0x80..0x84].copy_from_slice(b"PE\0\0");
        pe[0x84..0x86].copy_from_slice(&0x8664_u16.to_le_bytes());
        std::fs::write(&script, pe).expect("native package bin");
        assert_eq!(npm_bin_kind(&script).unwrap(), NpmBinKind::Native);
        assert!(native_cli_target_is_audited(&prefix, "claude", &script));
        assert!(!native_cli_target_is_audited(&prefix, "codex", &script));
        let forward = "node_modules/@anthropic-ai/claude-code/bin/claude.exe";
        let backward = r"node_modules\@anthropic-ai\claude-code\bin\claude.exe";

        validate_windows_cmd_launcher_content(
            &prefix,
            &script,
            windows_cmd_shim_native(backward).as_bytes(),
        )
        .expect("native direct .cmd");
        for template in [
            windows_sh_shim_native_v4_to_v5(forward),
            windows_sh_shim_native_v6_to_v8(forward),
            windows_sh_shim_native_v9(forward),
        ] {
            validate_windows_sh_launcher_content(&prefix, &script, template.as_bytes())
                .expect("native direct extensionless launcher");
        }
        validate_windows_ps1_launcher_content(
            &prefix,
            &script,
            windows_ps1_shim_native_v4_to_v9(forward).as_bytes(),
        )
        .expect("native direct PowerShell launcher");

        let malicious = format!(
            "{}\nStart-Process calc.exe",
            windows_ps1_shim_native_v4_to_v9(forward)
        );
        validate_windows_ps1_launcher_content(&prefix, &script, malicious.as_bytes())
            .expect_err("native launcher with extra command must fail closed");

        let native_command = npm_bin_version_command(&script, NpmBinKind::Native);
        assert_eq!(native_command.get_program(), script.as_os_str());
        assert_eq!(
            native_command.get_args().collect::<Vec<_>>(),
            vec![std::ffi::OsStr::new("--version")]
        );
        let node_command = npm_bin_version_command(&script, NpmBinKind::NodeJs);
        assert_eq!(node_command.get_program(), std::ffi::OsStr::new("node"));
        assert_eq!(
            node_command.get_args().collect::<Vec<_>>(),
            vec![script.as_os_str(), std::ffi::OsStr::new("--version")]
        );

        std::fs::write(&script, b"echo \"Error: native install failed\"\n")
            .expect("failed-install placeholder");
        assert!(
            npm_bin_kind(&script).is_err(),
            "ordinary no-shebang text must never be treated as native"
        );
        assert_eq!(classify_npm_bin_prefix(b"MZ hello, not PE"), None);
        let mut mach_o = [0_u8; 16];
        mach_o[..4].copy_from_slice(&[0xcf, 0xfa, 0xed, 0xfe]);
        mach_o[4..8].copy_from_slice(&0x0100_000c_u32.to_le_bytes());
        mach_o[12..16].copy_from_slice(&2_u32.to_le_bytes());
        assert_eq!(classify_npm_bin_prefix(&mach_o), Some(NpmBinKind::Native));
        let mut elf = [0_u8; 20];
        elf[..4].copy_from_slice(b"\x7fELF");
        elf[4] = 2; // ELFCLASS64
        elf[5] = 1; // little endian
        elf[16..18].copy_from_slice(&3_u16.to_le_bytes()); // PIE/ET_DYN
        elf[18..20].copy_from_slice(&183_u16.to_le_bytes()); // AArch64
        assert_eq!(classify_npm_bin_prefix(&elf), Some(NpmBinKind::Native));
        std::fs::remove_dir_all(prefix).expect("cleanup");
    }

    #[test]
    fn npm_payload_allows_only_exact_global_node_modules_lock() {
        let prefix = test_dir("npm-lock-location");
        let modules_root = npm_modules_root(&prefix);
        std::fs::create_dir_all(&modules_root).expect("global node_modules");
        std::fs::write(modules_root.join(".package-lock.json"), b"{}").expect("npm root lock");
        assert!(
            !directory_has_unknown_npm_payload(&prefix).expect("exact npm lock is known"),
            "only the exact npm internal lock should be allowed"
        );

        let nested = modules_root.join("user-package");
        std::fs::create_dir_all(&nested).expect("nested package");
        std::fs::write(nested.join(".package-lock.json"), b"user data")
            .expect("nested same-name file");
        assert!(
            directory_has_unknown_npm_payload(&prefix).expect("inspect nested lock"),
            "a nested same-name file is user/unknown payload"
        );

        std::fs::remove_dir_all(prefix).expect("cleanup");
    }

    #[test]
    fn cli_ownership_crash_window_keeps_pending_then_commits_installed() {
        let directory = test_dir("pending");
        let path = directory.join("claude-code.json");
        let prefix = directory.join("npm-global");
        let pending = CliInstallRecord::pending("@anthropic-ai/claude-code", prefix.clone());
        atomic_write_mode(&path, &serde_json::to_string(&pending).unwrap(), true)
            .expect("durable pending reservation");
        assert_eq!(
            parse_cli_install_record(
                &std::fs::read(&path).unwrap(),
                "@anthropic-ai/claude-code",
                &prefix,
            )
            .expect("load pending"),
            pending
        );

        let mut launcher_sha256 = std::collections::BTreeMap::new();
        for launcher in cli_launcher_paths(&prefix, "claude").expect("launcher paths") {
            let relative = launcher
                .strip_prefix(&prefix)
                .expect("relative launcher")
                .components()
                .map(|component| component.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
            launcher_sha256.insert(relative, "b".repeat(64));
        }
        let receipt_pending = pending.with_receipt(super::super::pinned_npm::PinnedBundleReceipt {
            schema_version: 1,
            package_sha256: "a".repeat(64),
            launcher_sha256,
        });
        let installed = CliInstallRecord::installed_from(&receipt_pending, "2.1.0".into());
        atomic_write_mode(&path, &serde_json::to_string(&installed).unwrap(), true)
            .expect("atomic pending to installed transition");
        assert_eq!(
            parse_cli_install_record(
                &std::fs::read(&path).unwrap(),
                "@anthropic-ai/claude-code",
                &prefix,
            )
            .expect("load installed")
            .state,
            CliInstallState::Installed
        );
        std::fs::remove_dir_all(directory).expect("cleanup");
    }

    #[test]
    fn pending_side_effect_is_only_proven_inside_exact_prefix() {
        let prefix = test_dir("package-proof");
        let package = prefix
            .join(if cfg!(windows) {
                "node_modules"
            } else {
                "lib/node_modules"
            })
            .join("@openai")
            .join("codex");
        std::fs::create_dir_all(&package).expect("package directory");
        assert!(
            npm_package_artifacts_present_at_expected(&prefix, "@openai/codex", &prefix)
                .expect("pending cleanup may detect a partial real directory")
        );
        assert!(npm_package_installed_at_expected(&prefix, "@openai/codex", &prefix).is_err());
        std::fs::write(
            package.join("package.json"),
            r#"{"name":"@openai/codex","bin":{"codex":"bin/codex.js"}}"#,
        )
        .expect("package manifest");
        assert!(
            npm_package_installed_at_expected(&prefix, "@openai/codex", &prefix)
                .expect("exact-prefix proof")
        );

        let other = test_dir("other-prefix");
        let error = npm_package_installed_at_expected(&prefix, "@openai/codex", &other)
            .expect_err("changed prefix must be rejected");
        assert!(error.contains("不是 CodeCLI 专属目录"));
        std::fs::remove_dir_all(prefix).expect("cleanup prefix");
        std::fs::remove_dir_all(other).expect("cleanup other");
    }

    #[test]
    fn ownership_with_changed_prefix_or_legacy_bool_fails_closed() {
        let directory = test_dir("bad-record");
        let path = directory.join("codex-cli.json");
        let expected = directory.join("expected-prefix");
        let changed = CliInstallRecord::pending("@openai/codex", directory.join("not-owned"));
        let changed_bytes = serde_json::to_vec(&changed).unwrap();
        std::fs::write(&path, &changed_bytes).unwrap();
        let error = parse_cli_install_record(&changed_bytes, "@openai/codex", &expected)
            .expect_err("changed prefix must fail closed");
        assert!(error.contains("prefix"));

        let legacy_bytes = br#"{"package":"@openai/codex","installedByUs":true}"#;
        std::fs::write(&path, legacy_bytes).unwrap();
        let legacy = parse_cli_install_record(legacy_bytes, "@openai/codex", &expected)
            .expect_err("legacy boolean ownership cannot prove a prefix");
        assert!(legacy.contains("旧版不安全格式"));
        std::fs::remove_dir_all(directory).expect("cleanup");
    }

    #[cfg(unix)]
    #[test]
    fn dangling_ownership_symlink_fails_closed() {
        use std::os::unix::fs::symlink;

        let directory = test_dir("dangling-record");
        let path = directory.join("claude-code.json");
        symlink(directory.join("missing-target"), &path).expect("dangling symlink");
        let error = load_cli_install_record(&path, "@anthropic-ai/claude-code")
            .expect_err("dangling ownership link must be rejected");
        assert!(error.contains("不是可信普通文件"));
        std::fs::remove_dir_all(directory).expect("cleanup");
    }
}
