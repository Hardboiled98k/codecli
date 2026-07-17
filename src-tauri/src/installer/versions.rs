// SPDX-License-Identifier: MPL-2.0
//! 版本查看 + 安全升级（复用现有 install 路径）

use serde::Serialize;
use std::process::Command;

use super::claude_code::{claude_code_version_sync, install_claude_code_sync_with_force};
use super::codex_cli::{codex_cli_version_sync, install_codex_cli_sync_with_force};
use super::op_lock::with_new_operation;
use super::platform::{refresh_path_from_system, which_cmd};
use super::runtime::{codecli_node_runtime_is_active, ensure_node_sync_with_force};
use super::util::{chrono_like_now, strip_secret_env_from_command};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ComponentVersion {
    pub id: String,
    pub name: String,
    pub installed: bool,
    pub version: Option<String>,
    pub path: Option<String>,
    pub upgradable: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VersionsReport {
    pub ok: bool,
    pub checked_at: String,
    pub components: Vec<ComponentVersion>,
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpgradeResult {
    pub ok: bool,
    pub message: String,
    pub details: Vec<String>,
}

fn tool_version(bin: &str) -> Option<String> {
    which_cmd(bin)?;
    for flag in ["--version", "-v"] {
        let mut cmd = Command::new(bin);
        cmd.arg(flag);
        strip_secret_env_from_command(&mut cmd);
        if let Ok(out) = cmd.output() {
            if out.status.success() {
                let s = String::from_utf8_lossy(&out.stdout);
                if let Some(line) = s.lines().find(|l| !l.trim().is_empty()) {
                    return Some(line.trim().to_string());
                }
                let e = String::from_utf8_lossy(&out.stderr);
                if let Some(line) = e.lines().find(|l| !l.trim().is_empty()) {
                    return Some(line.trim().to_string());
                }
            }
        }
    }
    None
}

pub fn versions_report_sync() -> Result<VersionsReport, String> {
    refresh_path_from_system();
    let mut components = vec![ComponentVersion {
        id: "codecli".into(),
        name: "CodeCLI 安装器".into(),
        installed: true,
        version: Some(super::APP_VERSION.into()),
        path: std::env::current_exe()
            .ok()
            .map(|path| path.to_string_lossy().into_owned()),
        // 应用自身只通过对外签名下载页更新，不复用 CLI 安装通道。
        upgradable: false,
    }];

    let node_v = tool_version("node");
    let node_upgradable = node_v.is_none() || codecli_node_runtime_is_active();
    components.push(ComponentVersion {
        id: "node".into(),
        name: "Node.js".into(),
        installed: node_v.is_some(),
        version: node_v,
        path: which_cmd("node"),
        // 只能覆盖工具自己的固定 runtime。外部 Node 由它的
        // 原管理器升级，避免安装器跨边界改动用户环境。
        upgradable: node_upgradable,
    });

    let npm_v = tool_version("npm");
    components.push(ComponentVersion {
        id: "npm".into(),
        name: "npm".into(),
        installed: npm_v.is_some(),
        version: npm_v,
        path: which_cmd("npm"),
        upgradable: false,
    });

    let claude_v = claude_code_version_sync().or_else(|| tool_version("claude"));
    components.push(ComponentVersion {
        id: "claude".into(),
        name: "Claude Code".into(),
        installed: claude_v.is_some(),
        version: claude_v,
        path: which_cmd("claude"),
        upgradable: true,
    });

    let codex_v = codex_cli_version_sync().or_else(|| tool_version("codex"));
    components.push(ComponentVersion {
        id: "codex".into(),
        name: "Codex CLI".into(),
        installed: codex_v.is_some(),
        version: codex_v,
        path: which_cmd("codex"),
        upgradable: true,
    });

    let missing = components.iter().filter(|c| !c.installed).count();
    Ok(VersionsReport {
        ok: true,
        checked_at: chrono_like_now(),
        components,
        message: if missing == 0 {
            "组件齐全".into()
        } else {
            format!("{} 项未安装", missing)
        },
    })
}

pub fn upgrade_component_sync(
    id: String,
    prefer_mirror: Option<bool>,
) -> Result<UpgradeResult, String> {
    refresh_path_from_system();
    let mut details = Vec::new();
    match id.as_str() {
        "node" => {
            // 当前受支持的 Claude Code npm wrapper 要求 Node >=22。
            let r = ensure_node_sync_with_force(Some(22), true)?;
            details.push(r.message.clone());
            Ok(UpgradeResult {
                ok: r.ok,
                message: r.message,
                details,
            })
        }
        "claude" => {
            let node = ensure_node_sync_with_force(Some(22), false)?;
            details.push(format!("Node: {}", node.message));
            let r = install_claude_code_sync_with_force(prefer_mirror, true)?;
            details.push(r.message.clone());
            if let Some(v) = claude_code_version_sync() {
                details.push(format!("当前版本: {}", v));
            }
            Ok(UpgradeResult {
                ok: r.ok,
                message: r.message,
                details,
            })
        }
        "codex" => {
            let r = install_codex_cli_sync_with_force(prefer_mirror, true)?;
            details.push(r.message.clone());
            if let Some(v) = codex_cli_version_sync() {
                details.push(format!("当前版本: {}", v));
            }
            Ok(UpgradeResult {
                ok: r.ok,
                message: r.message,
                details,
            })
        }
        "all" => {
            let mut msgs = Vec::new();
            let mut all_ok = true;
            match ensure_node_sync_with_force(Some(22), true) {
                Ok(r) => {
                    all_ok &= r.ok;
                    msgs.push(format!("Node: {}", r.message));
                }
                Err(e) => {
                    all_ok = false;
                    msgs.push(format!("Node: {}", e));
                }
            }
            match install_claude_code_sync_with_force(prefer_mirror, true) {
                Ok(r) => {
                    all_ok &= r.ok;
                    msgs.push(format!("Claude: {}", r.message));
                }
                Err(e) => {
                    all_ok = false;
                    msgs.push(format!("Claude: {}", e));
                }
            }
            match install_codex_cli_sync_with_force(prefer_mirror, true) {
                Ok(r) => {
                    all_ok &= r.ok;
                    msgs.push(format!("Codex: {}", r.message));
                }
                Err(e) => {
                    all_ok = false;
                    msgs.push(format!("Codex: {}", e));
                }
            }
            Ok(UpgradeResult {
                ok: all_ok,
                message: if all_ok {
                    "批量升级结束".into()
                } else {
                    "批量升级部分失败（见详情）".into()
                },
                details: msgs,
            })
        }
        other => Err(format!("未知组件: {}（node|claude|codex|all）", other)),
    }
}

#[tauri::command]
pub async fn versions_report() -> Result<VersionsReport, String> {
    super::util::spawn_blocking_result(versions_report_sync).await
}

#[tauri::command]
pub async fn upgrade_component(
    id: String,
    prefer_mirror: Option<bool>,
) -> Result<UpgradeResult, String> {
    super::util::spawn_blocking_result(move || {
        with_new_operation(|| upgrade_component_sync(id, prefer_mirror))
    })
    .await
}
