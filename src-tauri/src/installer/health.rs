// SPDX-License-Identifier: MPL-2.0
//! 环境体检 + 保守一键修复

use serde::Serialize;
use std::process::Command;

use super::op_lock::with_op_lock;
use super::platform::{
    claude_config_dir, codecli_state_dir, codex_config_toml, home_dir, refresh_path_from_system,
    which_cmd,
};
use super::system::probe_system_sync;
use super::util::{chrono_like_now, strip_secret_env_from_command};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HealthItem {
    pub id: String,
    pub title: String,
    /// ok | warn | bad | info
    pub level: String,
    pub message: String,
    pub detail: Option<String>,
    pub fixable: bool,
    pub fix_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HealthReport {
    pub ok: bool,
    pub summary: String,
    pub checked_at: String,
    pub items: Vec<HealthItem>,
    pub auto_fixable: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HealthFixResult {
    pub ok: bool,
    pub message: String,
    pub fixed: Vec<String>,
    pub skipped: Vec<String>,
}

fn item(
    id: &str,
    title: &str,
    level: &str,
    message: impl Into<String>,
    detail: Option<String>,
    fixable: bool,
    fix_id: Option<&str>,
) -> HealthItem {
    HealthItem {
        id: id.into(),
        title: title.into(),
        level: level.into(),
        message: message.into(),
        detail,
        fixable,
        fix_id: fix_id.map(|s| s.into()),
    }
}

fn bin_path_detail(bin: &str) -> (Option<String>, Option<String>) {
    let path = which_cmd(bin);
    let ver = path.as_ref().and_then(|_| {
        let mut version = Command::new(bin);
        version.arg("--version");
        strip_secret_env_from_command(&mut version);
        let out = version.output().ok()?;
        if !out.status.success() {
            let mut short_version = Command::new(bin);
            short_version.arg("-v");
            strip_secret_env_from_command(&mut short_version);
            let out2 = short_version.output().ok()?;
            if !out2.status.success() {
                return None;
            }
            return Some(
                String::from_utf8_lossy(&out2.stdout)
                    .lines()
                    .next()
                    .unwrap_or("")
                    .trim()
                    .to_string(),
            );
        }
        Some(
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .next()
                .unwrap_or("")
                .trim()
                .to_string(),
        )
    });
    (path, ver)
}

fn path_has_dir(dir: &str) -> bool {
    let path = std::env::var("PATH").unwrap_or_default();
    let sep = if cfg!(target_os = "windows") {
        ';'
    } else {
        ':'
    };
    path.split(sep).any(|p| p == dir || p.ends_with(dir))
}

pub fn health_check_sync() -> Result<HealthReport, String> {
    refresh_path_from_system();
    let mut items = Vec::new();
    let mut auto_fixable = Vec::new();

    // 系统探针
    let probe = probe_system_sync()?;
    if !probe.os_supported {
        items.push(item(
            "os",
            "系统支持",
            "bad",
            probe.support_message.clone(),
            probe.os_version.clone(),
            false,
            None,
        ));
    } else {
        items.push(item(
            "os",
            "系统支持",
            "ok",
            format!("{} / {}", probe.os, probe.arch),
            Some(probe.support_message.clone()),
            false,
            None,
        ));
    }

    if probe.disk_known && probe.disk_free_gb < 1.0 {
        items.push(item(
            "disk",
            "磁盘空间",
            "bad",
            format!("剩余 {:.1} GB，不足 1GB", probe.disk_free_gb),
            None,
            false,
            None,
        ));
    } else if probe.disk_known {
        items.push(item(
            "disk",
            "磁盘空间",
            "ok",
            format!("剩余 {:.1} GB", probe.disk_free_gb),
            None,
            false,
            None,
        ));
    } else {
        items.push(item(
            "disk",
            "磁盘空间",
            "warn",
            "无法检测磁盘剩余空间",
            None,
            false,
            None,
        ));
    }

    if !probe.network_ok {
        items.push(item(
            "network",
            "网络",
            "bad",
            "通用网络不可用",
            Some(probe.network.detail.clone()),
            false,
            None,
        ));
    } else {
        // 安装工件只从官方 registry + 固定 SRI 获取；第三方镜像可达
        // 只能作为网络诊断，不能显示成供应链安装链路“正常”。
        let level = if probe.network.npm_official_ok {
            "ok"
        } else {
            "warn"
        };
        items.push(item(
            "network",
            "网络",
            level,
            probe.network.detail.clone(),
            None,
            false,
            None,
        ));
    }

    // 命令路径
    for (bin, title) in [
        ("node", "Node.js"),
        ("npm", "npm"),
        ("claude", "Claude Code"),
        ("codex", "Codex CLI"),
    ] {
        let (path, ver) = bin_path_detail(bin);
        match (path, ver) {
            (Some(p), Some(v)) => items.push(item(
                &format!("bin_{}", bin),
                title,
                "ok",
                format!("{} · {}", v, p),
                None,
                false,
                None,
            )),
            (Some(p), None) => items.push(item(
                &format!("bin_{}", bin),
                title,
                "warn",
                format!("找到命令但无法读版本 · {}", p),
                None,
                false,
                None,
            )),
            (None, _) => {
                let level = if bin == "node" || bin == "npm" {
                    "bad"
                } else {
                    "warn"
                };
                items.push(item(
                    &format!("bin_{}", bin),
                    title,
                    level,
                    format!("当前 PATH 找不到 `{}`（新开终端或重新安装）", bin),
                    None,
                    bin == "claude" || bin == "codex" || bin == "node",
                    Some("refresh_path"),
                ));
                if !auto_fixable.contains(&"refresh_path".to_string()) {
                    auto_fixable.push("refresh_path".into());
                }
            }
        }
    }

    // 用户级 bin 是否在 PATH
    if let Some(home) = home_dir() {
        let candidates = if cfg!(target_os = "windows") {
            vec![home
                .join("AppData")
                .join("Roaming")
                .join("npm")
                .display()
                .to_string()]
        } else {
            vec![
                home.join(".local").join("bin").display().to_string(),
                home.join(".npm-global").join("bin").display().to_string(),
            ]
        };
        for c in candidates {
            if std::path::Path::new(&c).exists() && !path_has_dir(&c) {
                items.push(item(
                    "path_user_bin",
                    "用户 bin PATH",
                    "warn",
                    format!("{} 存在但不在 PATH", c),
                    Some("修复：刷新 PATH / 新开终端".into()),
                    true,
                    Some("refresh_path"),
                ));
                if !auto_fixable.contains(&"refresh_path".to_string()) {
                    auto_fixable.push("refresh_path".into());
                }
            }
        }
    }

    // Claude settings 解析
    if let Some(dir) = claude_config_dir() {
        let settings = dir.join("settings.json");
        if settings.exists() {
            match std::fs::read_to_string(&settings) {
                Ok(raw) => match serde_json::from_str::<serde_json::Value>(&raw) {
                    Ok(v) if v.is_object() => items.push(item(
                        "claude_settings",
                        "Claude settings.json",
                        "ok",
                        "可解析",
                        Some(settings.display().to_string()),
                        false,
                        None,
                    )),
                    Ok(_) => items.push(item(
                        "claude_settings",
                        "Claude settings.json",
                        "bad",
                        "根节点不是对象，写入会中止",
                        Some(settings.display().to_string()),
                        false,
                        None,
                    )),
                    Err(e) => items.push(item(
                        "claude_settings",
                        "Claude settings.json",
                        "bad",
                        format!("JSON 解析失败: {}", e),
                        Some(settings.display().to_string()),
                        false,
                        None,
                    )),
                },
                Err(e) => items.push(item(
                    "claude_settings",
                    "Claude settings.json",
                    "bad",
                    format!("无法读取: {}", e),
                    None,
                    false,
                    None,
                )),
            }
        } else {
            items.push(item(
                "claude_settings",
                "Claude settings.json",
                "info",
                "尚未创建（首次配置 API 时会写）",
                None,
                false,
                None,
            ));
        }
    }

    // Codex config
    if let Some(path) = codex_config_toml() {
        if path.exists() {
            match std::fs::read_to_string(&path) {
                Ok(raw) => {
                    if raw.trim().is_empty() {
                        items.push(item(
                            "codex_toml",
                            "Codex config.toml",
                            "warn",
                            "文件为空",
                            Some(path.display().to_string()),
                            false,
                            None,
                        ));
                    } else {
                        match raw.parse::<toml_edit::DocumentMut>() {
                            Ok(_) => items.push(item(
                                "codex_toml",
                                "Codex config.toml",
                                "ok",
                                "可解析",
                                Some(path.display().to_string()),
                                false,
                                None,
                            )),
                            Err(e) => items.push(item(
                                "codex_toml",
                                "Codex config.toml",
                                "bad",
                                format!("TOML 解析失败: {}", e),
                                Some(path.display().to_string()),
                                false,
                                None,
                            )),
                        }
                    }
                }
                Err(e) => items.push(item(
                    "codex_toml",
                    "Codex config.toml",
                    "bad",
                    format!("无法读取: {}", e),
                    None,
                    false,
                    None,
                )),
            }
        } else {
            items.push(item(
                "codex_toml",
                "Codex config.toml",
                "info",
                "尚未创建",
                None,
                false,
                None,
            ));
        }
    }

    // ownership / secrets
    if let Some(dir) = codecli_state_dir() {
        let own = dir.join("ownership.json");
        if own.exists() {
            let parse_ok = std::fs::read_to_string(&own)
                .ok()
                .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
                .map(|v| v.is_object())
                .unwrap_or(false);
            if parse_ok {
                items.push(item(
                    "ownership",
                    "本工具 ownership",
                    "ok",
                    "记录可解析（清除仅按 ownership 字段级回滚）",
                    Some(own.display().to_string()),
                    false,
                    None,
                ));
            } else {
                items.push(item(
                    "ownership",
                    "本工具 ownership",
                    "warn",
                    "文件存在但无法解析，清除可能不可用",
                    Some(own.display().to_string()),
                    false,
                    None,
                ));
            }
        } else {
            items.push(item(
                "ownership",
                "本工具 ownership",
                "info",
                "尚无写入记录",
                None,
                false,
                None,
            ));
        }
        let secrets = dir.join("secrets.env");
        if secrets.exists() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let meta = std::fs::symlink_metadata(&secrets);
                match meta {
                    Ok(m) if m.file_type().is_symlink() => {
                        items.push(item(
                            "secrets_perm",
                            "secrets.env 权限",
                            "bad",
                            "secrets.env 是符号链接，拒绝视为安全",
                            Some(secrets.display().to_string()),
                            false,
                            None,
                        ));
                    }
                    Ok(m) if m.is_file() => {
                        let mode = m.permissions().mode() & 0o777;
                        if mode != 0o600 {
                            items.push(item(
                                "secrets_perm",
                                "secrets.env 权限",
                                "bad",
                                format!("权限 {:o}，要求精确 0600", mode),
                                Some(secrets.display().to_string()),
                                true,
                                Some("fix_secrets_perm"),
                            ));
                            auto_fixable.push("fix_secrets_perm".into());
                        } else {
                            items.push(item(
                                "secrets_perm",
                                "secrets.env 权限",
                                "ok",
                                "0600 普通文件",
                                Some(secrets.display().to_string()),
                                false,
                                None,
                            ));
                        }
                    }
                    Ok(_) => {
                        items.push(item(
                            "secrets_perm",
                            "secrets.env 权限",
                            "bad",
                            "不是普通文件",
                            Some(secrets.display().to_string()),
                            false,
                            None,
                        ));
                    }
                    Err(e) => {
                        items.push(item(
                            "secrets_perm",
                            "secrets.env 权限",
                            "warn",
                            format!("无法 stat: {}", e),
                            None,
                            false,
                            None,
                        ));
                    }
                }
            }
            #[cfg(not(unix))]
            {
                items.push(item(
                    "secrets_perm",
                    "secrets.env",
                    "ok",
                    "存在",
                    Some(secrets.display().to_string()),
                    false,
                    None,
                ));
            }
        } else {
            items.push(item(
                "secrets_perm",
                "secrets.env",
                "info",
                "尚未创建（配置 API 后生成）",
                None,
                false,
                None,
            ));
        }
    }

    let bad = items.iter().filter(|i| i.level == "bad").count();
    let warn = items.iter().filter(|i| i.level == "warn").count();
    let summary = if bad > 0 {
        format!("{} 项异常 · {} 项警告", bad, warn)
    } else if warn > 0 {
        format!("基本可用 · {} 项警告", warn)
    } else {
        "环境正常".into()
    };

    Ok(HealthReport {
        ok: bad == 0,
        summary,
        checked_at: chrono_like_now(),
        items,
        auto_fixable,
    })
}

pub fn health_fix_sync(fix_ids: Vec<String>) -> Result<HealthFixResult, String> {
    let mut fixed = Vec::new();
    let mut skipped = Vec::new();
    let ids: Vec<String> = if fix_ids.is_empty() {
        // 默认只跑安全修复
        vec!["refresh_path".into(), "fix_secrets_perm".into()]
    } else {
        fix_ids
    };

    for id in ids {
        match id.as_str() {
            "refresh_path" => {
                refresh_path_from_system();
                fixed.push("已从系统刷新 PATH（当前进程）".into());
            }
            "fix_secrets_perm" => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    use std::os::unix::fs::PermissionsExt;
                    if let Some(dir) = codecli_state_dir() {
                        let secrets = dir.join("secrets.env");
                        match std::fs::symlink_metadata(&secrets) {
                            Ok(meta) if meta.file_type().is_symlink() || !meta.is_file() => {
                                return Err(
                                    "secrets.env 不是可安全修复的普通文件，已拒绝 chmod".into()
                                );
                            }
                            Ok(_) => {}
                            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                                skipped.push("secrets.env 不存在".into());
                                continue;
                            }
                            Err(error) => {
                                return Err(format!("secrets.env 检查失败: {error}"));
                            }
                        }
                        let mut options = std::fs::OpenOptions::new();
                        options.read(true).custom_flags(libc::O_NOFOLLOW);
                        let file = options
                            .open(&secrets)
                            .map_err(|error| format!("安全打开 secrets.env 失败: {error}"))?;
                        let metadata = file
                            .metadata()
                            .map_err(|error| format!("复查 secrets.env 失败: {error}"))?;
                        if !metadata.is_file() {
                            return Err("secrets.env 打开后不是普通文件".into());
                        }
                        let mut perms = metadata.permissions();
                        perms.set_mode(0o600);
                        file.set_permissions(perms)
                            .map_err(|error| format!("修复 secrets.env 权限失败: {error}"))?;
                        fixed.push("secrets.env → 0600".into());
                    } else {
                        skipped.push("无状态目录".into());
                    }
                }
                #[cfg(not(unix))]
                {
                    skipped.push("Windows 无需 chmod secrets".into());
                }
            }
            other => skipped.push(format!("未知修复: {}", other)),
        }
    }

    Ok(HealthFixResult {
        ok: !fixed.is_empty() || skipped.is_empty(),
        message: if fixed.is_empty() {
            "无修复执行".into()
        } else {
            format!(
                "已执行 {} 项修复；新开终端后 PATH 对 shell 完全生效",
                fixed.len()
            )
        },
        fixed,
        skipped,
    })
}

#[tauri::command]
pub async fn health_check() -> Result<HealthReport, String> {
    super::util::spawn_blocking_result(health_check_sync).await
}

#[tauri::command]
pub async fn health_fix(fix_ids: Option<Vec<String>>) -> Result<HealthFixResult, String> {
    super::util::spawn_blocking_result(move || {
        // 体检始终可读；修复仅修改本工具明确管理的本机状态。
        with_op_lock(|| health_fix_sync(fix_ids.unwrap_or_default()))
    })
    .await
}
