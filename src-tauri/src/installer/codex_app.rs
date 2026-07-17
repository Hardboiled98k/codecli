// SPDX-License-Identifier: MPL-2.0
use serde::Serialize;
use std::process::Command;

use super::platform::{os_kind, OsKind};
use super::util::strip_secret_env_from_command;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexAppStatus {
    pub available: bool,
    pub installed: bool,
    pub message: String,
    pub download_url: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallResult {
    pub ok: bool,
    pub skipped: bool,
    /// success | skipped | action_required | failed
    pub status: String,
    pub message: String,
}

fn download_url() -> Option<String> {
    // 2026-07 官方桌面端已合并为 ChatGPT（内含 Codex）。
    Some("https://chatgpt.com/download".into())
}

fn desktop_app_installed() -> bool {
    match os_kind() {
        OsKind::Macos => {
            std::path::Path::new("/Applications/Codex.app").exists()
                || std::path::Path::new("/Applications/ChatGPT Codex.app").exists()
                || std::path::Path::new("/Applications/ChatGPT.app").exists()
                || std::env::var_os("HOME").is_some_and(|home| {
                    let applications = std::path::PathBuf::from(home).join("Applications");
                    applications.join("Codex.app").exists()
                        || applications.join("ChatGPT.app").exists()
                })
        }
        OsKind::Windows => {
            // 不用 `where Codex`（会撞 CLI）。先查普通安装路径，
            // 再查 Microsoft Store/MSIX 包；新版应用名为 ChatGPT。
            if let Some(local) = std::env::var_os("LOCALAPPDATA") {
                let local = std::path::PathBuf::from(local);
                let candidates = [
                    local.join("Programs/Codex/Codex.exe"),
                    local.join("Codex/Codex.exe"),
                    local.join("Programs/ChatGPT/ChatGPT.exe"),
                    local.join("ChatGPT/ChatGPT.exe"),
                    local.join("Microsoft/WindowsApps/ChatGPT.exe"),
                ];
                if candidates.iter().any(|candidate| candidate.exists()) {
                    return true;
                }
            }

            let mut powershell = Command::new("powershell.exe");
            strip_secret_env_from_command(&mut powershell);
            powershell
                .args([
                    "-NoProfile",
                    "-NonInteractive",
                    "-Command",
                    "$p = Get-AppxPackage -ErrorAction SilentlyContinue | Where-Object { $_.Name -match '(?i)(OpenAI.*ChatGPT|ChatGPT.*OpenAI|Codex)' }; if ($p) { exit 0 } else { exit 1 }",
                ])
                .status()
                .is_ok_and(|status| status.success())
        }
        _ => false,
    }
}

#[tauri::command]
pub async fn codex_app_available() -> CodexAppStatus {
    super::util::spawn_blocking_ok(codex_app_available_sync)
        .await
        .unwrap_or(CodexAppStatus {
            available: false,
            installed: false,
            message: "探测失败".into(),
            download_url: download_url(),
        })
}

pub fn codex_app_available_sync() -> CodexAppStatus {
    match os_kind() {
        OsKind::Macos | OsKind::Windows => {
            let installed = desktop_app_installed();
            CodexAppStatus {
                available: true,
                installed,
                message: if installed {
                    "已检测到 ChatGPT/Codex 桌面 App".into()
                } else {
                    "可安装 ChatGPT 桌面 App（内含 Codex；将打开官方下载页）".into()
                },
                download_url: download_url(),
            }
        }
        _ => CodexAppStatus {
            available: false,
            installed: false,
            message: "当前平台暂无 ChatGPT/Codex 桌面 App 安装指引".into(),
            download_url: download_url(),
        },
    }
}

#[tauri::command]
pub async fn install_codex_app() -> Result<InstallResult, String> {
    super::util::spawn_blocking_result(install_codex_app_sync).await
}

pub fn install_codex_app_sync() -> Result<InstallResult, String> {
    let status = codex_app_available_sync();
    if status.installed {
        return Ok(InstallResult {
            ok: true,
            skipped: true,
            status: "skipped".into(),
            message: "ChatGPT/Codex 桌面 App 已安装，跳过".into(),
        });
    }
    let url = status
        .download_url
        .ok_or_else(|| "无可用下载地址".to_string())?;

    let result = if cfg!(target_os = "windows") {
        let mut command = Command::new("cmd");
        strip_secret_env_from_command(&mut command);
        command.args(["/C", "start", "", &url]).output()
    } else if cfg!(target_os = "macos") {
        let mut command = Command::new("open");
        strip_secret_env_from_command(&mut command);
        command.arg(&url).output()
    } else {
        let mut command = Command::new("xdg-open");
        strip_secret_env_from_command(&mut command);
        command.arg(&url).output()
    };

    match result {
        Ok(out) if out.status.success() => Ok(InstallResult {
            ok: true,
            skipped: false,
            status: "action_required".into(),
            message: format!(
                "已打开官方下载页，请手动安装 ChatGPT 桌面 App（内含 Codex）后点「重试此步」验收。\n{}",
                url
            ),
        }),
        Ok(out) => Err(format!(
            "打开下载页失败（exit {:?}）。请手动访问: {}",
            out.status.code(),
            url
        )),
        Err(e) => Err(format!("无法打开浏览器: {}。请手动访问: {}", e, url)),
    }
}
