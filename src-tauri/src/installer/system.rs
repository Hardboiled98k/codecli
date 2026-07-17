// SPDX-License-Identifier: MPL-2.0
use serde::Serialize;
use std::process::Command;

use super::platform::{os_display_name, os_kind, which_cmd, OsKind};

fn probe_command(program: &str) -> Command {
    let mut command = Command::new(program);
    super::util::strip_secret_env_from_command(&mut command);
    command
}

/// 公开支持矩阵（与 README 一致）
const MIN_MACOS: (u32, u32) = (12, 0); // macOS 12 Monterey+
const MIN_WIN_MAJOR: u32 = 10;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NetworkProbe {
    pub npm_official_ok: bool,
    pub nodejs_org_ok: bool,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemReport {
    pub os: String,
    pub os_kind: String,
    pub os_version: Option<String>,
    pub arch: String,
    pub os_supported: bool,
    pub support_message: String,
    pub network_ok: bool,
    pub network: NetworkProbe,
    pub disk_free_gb: f64,
    pub disk_known: bool,
    pub node_installed: bool,
    pub node_version: Option<String>,
    pub npm_installed: bool,
    pub npm_version: Option<String>,
    pub claude_installed: bool,
    pub claude_version: Option<String>,
    pub codex_installed: bool,
    pub codex_version: Option<String>,
    pub home: Option<String>,
    pub has_admin_hint: bool,
}

fn parse_version_line(stdout: &str) -> Option<String> {
    stdout
        .lines()
        .find(|l| !l.trim().is_empty())
        .map(|l| l.trim().to_string())
}

fn probe_url(client: &reqwest::blocking::Client, url: &str) -> bool {
    if let Ok(resp) = client.head(url).send() {
        if resp.status().is_success()
            || resp.status().is_redirection()
            || resp.status().as_u16() == 405
        {
            return true;
        }
    }
    if let Ok(resp) = client.get(url).send() {
        return resp.status().is_success() || resp.status().is_redirection();
    }
    false
}

fn check_network_detailed() -> NetworkProbe {
    let client = match reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(6))
        .redirect(reqwest::redirect::Policy::limited(2))
        .build()
    {
        Ok(c) => c,
        Err(_) => {
            return NetworkProbe {
                npm_official_ok: false,
                nodejs_org_ok: false,
                detail: "无法创建 HTTP 客户端".into(),
            };
        }
    };

    let npm_official_ok = probe_url(&client, "https://registry.npmjs.org");
    let nodejs_org_ok = probe_url(&client, "https://nodejs.org/dist/");
    // 启动自检只访问实际安装会用到的官方端点。不为“通用联网”
    // 额外请求搜索引擎、CDN 或第三方镜像，减少未必要的 IP 暴露。
    let install_network_ok = npm_official_ok || nodejs_org_ok;

    let detail = format!(
        "官方npm={} nodejs.org={} 安装网络={}",
        if npm_official_ok { "OK" } else { "NG" },
        if nodejs_org_ok { "OK" } else { "NG" },
        if install_network_ok { "OK" } else { "NG" }
    );

    NetworkProbe {
        npm_official_ok,
        nodejs_org_ok,
        detail,
    }
}

fn disk_free_gb() -> (f64, bool) {
    // 优先测 HOME 所在卷（实际写入目录）
    if cfg!(target_os = "windows") {
        let home = super::platform::home_dir()
            .and_then(|p| p.to_str().map(|s| s.to_string()))
            .unwrap_or_else(|| "C:\\".into());
        let drive = home.chars().take(2).collect::<String>();
        let letter = drive.trim_end_matches(':').chars().next().unwrap_or('C');
        let ps = format!("(Get-PSDrive -Name '{}').Free", letter);
        if let Ok(out) = probe_command("powershell")
            .args(["-NoProfile", "-Command", &ps])
            .output()
        {
            if out.status.success() {
                let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if let Ok(bytes) = s.parse::<f64>() {
                    return (
                        (bytes / 1024.0 / 1024.0 / 1024.0 * 10.0).round() / 10.0,
                        true,
                    );
                }
            }
        }
        (0.0, false)
    } else {
        let target = super::platform::home_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "/".into());
        if let Ok(out) = probe_command("df").args(["-k", &target]).output() {
            let s = String::from_utf8_lossy(&out.stdout);
            if let Some(line) = s.lines().nth(1) {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 4 {
                    if let Ok(kb) = parts[3].parse::<f64>() {
                        return ((kb / 1024.0 / 1024.0 * 10.0).round() / 10.0, true);
                    }
                }
            }
        }
        (0.0, false)
    }
}

fn tool_version(bin: &str, version_flag: &str) -> Option<String> {
    which_cmd(bin)?;
    let out = probe_command(bin).arg(version_flag).output().ok()?;
    // 必须成功退出，避免把错误输出当版本
    if !out.status.success() {
        return None;
    }
    parse_version_line(&String::from_utf8_lossy(&out.stdout))
        .or_else(|| parse_version_line(&String::from_utf8_lossy(&out.stderr)))
}

fn macos_version() -> Option<String> {
    let out = probe_command("sw_vers")
        .arg("-productVersion")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_version_line(&String::from_utf8_lossy(&out.stdout))
}

fn windows_version() -> Option<String> {
    // CurrentMajorVersionNumber 更可靠（Win10=10, Win11 仍为 10；ProductName 区分）
    let out = probe_command("powershell")
        .args([
            "-NoProfile",
            "-Command",
            "$p=Get-ItemProperty 'HKLM:\\SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion'; '{0} | {1} | {2}' -f $p.CurrentMajorVersionNumber,$p.DisplayVersion,$p.ProductName",
        ])
        .output()
        .ok()?;
    if out.status.success() {
        if let Some(v) = parse_version_line(&String::from_utf8_lossy(&out.stdout)) {
            if !v.is_empty() {
                return Some(v);
            }
        }
    }
    let out2 = probe_command("cmd").args(["/C", "ver"]).output().ok()?;
    parse_version_line(&String::from_utf8_lossy(&out2.stdout))
}

fn parse_semver_major_minor(v: &str) -> Option<(u32, u32)> {
    let mut parts = v.trim().trim_start_matches('v').split(['.', ' ']);
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().and_then(|x| x.parse().ok()).unwrap_or(0);
    Some((major, minor))
}

fn check_os_support() -> (bool, Option<String>, String) {
    match os_kind() {
        OsKind::Macos => {
            let ver = macos_version();
            let arch_ok = matches!(std::env::consts::ARCH, "aarch64" | "x86_64");
            if !arch_ok {
                return (
                    false,
                    ver,
                    format!(
                        "不支持的 Mac 架构 {}（需要 Apple Silicon 或 Intel x64）",
                        std::env::consts::ARCH
                    ),
                );
            }
            let Some(v) = ver.as_deref() else {
                return (
                    false,
                    None,
                    "无法读取 macOS 版本，已中止安装（安全策略：版本未知不放行）。".into(),
                );
            };
            let Some((maj, min)) = parse_semver_major_minor(v) else {
                return (
                    false,
                    ver.clone(),
                    format!("无法解析 macOS 版本「{}」，已中止。", v),
                );
            };
            if maj < MIN_MACOS.0 || (maj == MIN_MACOS.0 && min < MIN_MACOS.1) {
                return (
                    false,
                    ver.clone(),
                    format!(
                        "macOS {} 过旧。需要 macOS {}+。请升级系统后再装。",
                        v, MIN_MACOS.0
                    ),
                );
            }
            (
                true,
                ver,
                format!("支持 macOS {}+ / arm64+x64", MIN_MACOS.0),
            )
        }
        OsKind::Windows => {
            let ver = windows_version();
            let arch_ok = matches!(std::env::consts::ARCH, "x86_64" | "aarch64");
            if !arch_ok {
                return (
                    false,
                    ver,
                    format!("不支持的 Windows 架构 {}", std::env::consts::ARCH),
                );
            }
            let Some(v) = ver.as_deref() else {
                return (
                    false,
                    None,
                    "无法读取 Windows 版本，已中止安装（安全策略：版本未知不放行）。".into(),
                );
            };
            let low = v.to_lowercase();
            // 拒绝明确的旧系统
            if low.contains("windows 7")
                || low.contains("windows 8")
                || low.contains(" 6.1")
                || low.contains(" 6.2")
                || low.contains(" 6.3")
            {
                return (
                    false,
                    ver.clone(),
                    "Windows 7/8 不受支持。请使用 Windows 10/11。".into(),
                );
            }
            // CurrentMajorVersionNumber 在输出开头：如 "10 | 22H2 | Windows 10/11"
            let major_ok = low
                .split('|')
                .next()
                .and_then(|s| s.trim().parse::<u32>().ok())
                .map(|m| m >= MIN_WIN_MAJOR)
                .unwrap_or_else(|| {
                    // cmd ver 文本：Microsoft Windows [Version 10.0.xxxxx]
                    low.contains("version 10.")
                        || low.contains("windows 10")
                        || low.contains("windows 11")
                });
            if !major_ok {
                return (
                    false,
                    ver.clone(),
                    format!("当前 Windows「{}」不受支持。需要 Windows 10/11。", v),
                );
            }
            (true, ver, "支持 Windows 10/11".into())
        }
        OsKind::Linux => (
            false,
            None,
            "暂不支持 Linux GUI 安装器。请手动安装 CLI。".into(),
        ),
        OsKind::Unknown => (false, None, "未知操作系统，无法继续。".into()),
    }
}

/// 安装前网络是否足够。CLI 安装包仅允许固定的
/// registry.npmjs.org HTTPS tarball，镜像可达不能代替官方源。
pub fn network_sufficient_for_install(has_node: bool, net: &NetworkProbe) -> Result<(), String> {
    if !net.npm_official_ok {
        return Err(format!(
            "无法访问官方 npm registry.npmjs.org；为确保安装包 SRI 身份闭环，不会降级到第三方镜像。请检查网络/代理/DNS。\n{}",
            net.detail
        ));
    }
    if !has_node && !net.nodejs_org_ok {
        return Err(format!(
            "未安装 Node，且无法访问 nodejs.org。为保证安装字节与本版验收过的固定 SHA-256 一致，不会降级调用 brew/winget/Chocolatey 的可变最新版。请先联网或自行安装 Node 22+。\n{}",
            net.detail
        ));
    }
    Ok(())
}

#[tauri::command]
pub async fn probe_system() -> Result<SystemReport, String> {
    super::util::spawn_blocking_result(probe_system_sync).await
}

pub fn probe_system_sync() -> Result<SystemReport, String> {
    super::platform::refresh_path_from_system();
    let node_version = tool_version("node", "-v");
    let npm_version = tool_version("npm", "-v");
    let claude_version =
        tool_version("claude", "--version").or_else(|| tool_version("claude", "-v"));
    let codex_version = tool_version("codex", "--version").or_else(|| tool_version("codex", "-v"));

    let kind = match os_kind() {
        OsKind::Windows => "windows",
        OsKind::Macos => "macos",
        OsKind::Linux => "linux",
        OsKind::Unknown => "unknown",
    };

    let (os_supported, os_version, support_message) = check_os_support();
    let network = check_network_detailed();
    let network_ok = network.npm_official_ok || network.nodejs_org_ok;
    let (disk_free_gb, disk_known) = disk_free_gb();

    Ok(SystemReport {
        os: os_display_name(),
        os_kind: kind.to_string(),
        os_version,
        arch: std::env::consts::ARCH.to_string(),
        os_supported,
        support_message,
        network_ok,
        network,
        disk_free_gb,
        disk_known,
        node_installed: node_version.is_some(),
        node_version,
        npm_installed: npm_version.is_some(),
        npm_version,
        claude_installed: claude_version.is_some(),
        claude_version,
        codex_installed: codex_version.is_some(),
        codex_version,
        home: super::platform::home_dir().map(|p| p.display().to_string()),
        has_admin_hint: false,
    })
}

#[cfg(test)]
mod tests {
    use super::{network_sufficient_for_install, NetworkProbe};

    fn network(official: bool) -> NetworkProbe {
        NetworkProbe {
            npm_official_ok: official,
            nodejs_org_ok: false,
            detail: "test".into(),
        }
    }

    #[test]
    fn unavailable_official_registry_blocks_pinned_artifact_preflight() {
        let error = network_sufficient_for_install(true, &network(false))
            .expect_err("unavailable official registry must block pinned downloads");
        assert!(error.contains("registry.npmjs.org"), "{error}");
    }

    #[test]
    fn official_registry_satisfies_cli_download_when_node_exists() {
        network_sufficient_for_install(true, &network(true)).unwrap();
    }
}
