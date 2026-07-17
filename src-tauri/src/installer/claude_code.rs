// SPDX-License-Identifier: MPL-2.0
use serde::Serialize;
use std::path::PathBuf;

use super::cmd::{check_cancelled, run_timed};
use super::op_lock::with_new_operation;
use super::pinned_npm::{
    prepare_claude_bundle, remove_pinned_bundle_exact, verify_pinned_bundle_receipt,
};
use super::platform::{refresh_path_from_system, which_cmd};
use super::runtime::{
    ensure_owned_npm_prefix, load_cli_install_record, npm_cli_artifacts_present,
    npm_package_artifacts_present_at, owned_npm_prefix, persist_cli_install_record,
    remove_cli_install_record, verify_owned_npm_cli, version_tuple, CliInstallRecord,
    CliInstallState,
};
use super::util::strip_secret_env_from_command;

const PKG: &str = "@anthropic-ai/claude-code";
const COMMAND: &str = "claude";
/// 发货版只安装已经审计过 bin/postinstall 形态的精确版本。
/// 不使用 `@latest`，避免上游在客户安装时改变执行模型。
const SUPPORTED_PACKAGE_VERSION: &str = "2.1.211";
const SUPPORTED_VER: (u32, u32, u32) = (2, 1, 211);
/// 过旧则提示用户用原安装器升级；本工具只升级自己拥有的副本。
const MIN_VER: (u32, u32, u32) = (1, 0, 0);

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallResult {
    pub ok: bool,
    pub skipped: bool,
    pub version: Option<String>,
    pub message: String,
}

fn install_record_path() -> Result<PathBuf, String> {
    super::platform::codecli_state_dir()
        .map(|directory| directory.join("claude-code.json"))
        .ok_or_else(|| "找不到 Claude Code 安装记录路径".into())
}

fn load_install_record() -> Result<Option<CliInstallRecord>, String> {
    load_cli_install_record(&install_record_path()?, PKG)
}

#[tauri::command]
pub async fn claude_code_version() -> Option<String> {
    super::util::spawn_blocking_ok(claude_code_version_sync)
        .await
        .ok()
        .flatten()
}

pub fn claude_code_version_sync() -> Option<String> {
    // 这是全局探测，只用于识别“用户已经自行安装”的 CLI；ownership 验证会
    // 直接执行 CodeCLI prefix 中 package.json 声明的 bin，不信任 PATH。
    verify_cli_works().ok()
}

fn verify_cli_works() -> Result<String, String> {
    refresh_path_from_system();
    if which_cmd(COMMAND).is_none() {
        return Err("找不到 claude 命令".into());
    }
    let mut command = if cfg!(windows) {
        let mut command = std::process::Command::new("cmd");
        command.args(["/D", "/S", "/C", COMMAND]);
        command
    } else {
        std::process::Command::new(COMMAND)
    };
    command.arg("--version");
    strip_secret_env_from_command(&mut command);
    let output = run_timed(command, 30)?;
    if !output.status_ok {
        return Err(format!(
            "claude --version 退出码非 0: {}",
            output
                .stderr
                .chars()
                .chain(output.stdout.chars())
                .take(120)
                .collect::<String>()
        ));
    }
    output
        .stdout
        .lines()
        .chain(output.stderr.lines())
        .find(|line| !line.trim().is_empty())
        .map(|line| line.trim().to_string())
        .ok_or_else(|| "claude --version 无输出".to_string())
}

fn unowned_cli_result(version: Option<String>, force: bool) -> Result<InstallResult, String> {
    let readable_and_current = version
        .as_deref()
        .and_then(version_tuple)
        .is_some_and(|value| value >= MIN_VER);
    if !force && readable_and_current {
        let version = version.expect("checked Some above");
        return Ok(InstallResult {
            ok: true,
            skipped: true,
            version: Some(version.clone()),
            message: format!(
                "Claude Code 已由其他方式安装 {}，本工具不抢占安装/卸载归属，已跳过",
                version
            ),
        });
    }
    Err(if force {
        "检测到非本工具安装的 Claude Code；为避免改变你原有安装及 Claude CLI 工作流，本工具拒绝强制升级。请使用原安装器（npm/brew/官方安装器）升级".into()
    } else {
        "检测到非本工具安装的 Claude Code，但版本过旧或无法验证；本工具不会覆盖并抢占归属。请使用原安装器升级后重试".into()
    })
}

fn owned_payload_present(prefix: &std::path::Path) -> Result<(bool, bool), String> {
    Ok((
        npm_package_artifacts_present_at(prefix, PKG)?,
        npm_cli_artifacts_present(prefix, COMMAND)?,
    ))
}

fn is_supported_version(version: &str) -> bool {
    version_tuple(version) == Some(SUPPORTED_VER)
}

#[tauri::command]
pub async fn install_claude_code(prefer_mirror: Option<bool>) -> Result<InstallResult, String> {
    super::util::spawn_blocking_result(move || {
        with_new_operation(|| install_claude_code_sync(prefer_mirror))
    })
    .await
}

pub fn install_claude_code_sync(prefer_mirror: Option<bool>) -> Result<InstallResult, String> {
    install_claude_code_sync_with_force(prefer_mirror, false)
}

/// `force=true` 只升级本工具在专属 prefix 中拥有的副本。用户自行安装的
/// Claude Code 即使显式点击升级也不会被覆盖。
pub fn install_claude_code_sync_with_force(
    _prefer_mirror: Option<bool>,
    force: bool,
) -> Result<InstallResult, String> {
    check_cancelled()?;
    refresh_path_from_system();
    let path = install_record_path()?;
    let existing = load_install_record()?;
    let prefix = owned_npm_prefix()?;

    if existing.is_none() {
        let (package_present, launcher_present) = owned_payload_present(&prefix)?;
        if package_present || launcher_present {
            return Err(
                "CodeCLI 专属 npm prefix 中存在无可信 ownership 的 Claude Code 残留；为防误认领已拒绝覆盖，请先人工核查状态目录"
                    .into(),
            );
        }
        if which_cmd(COMMAND).is_some() {
            return unowned_cli_result(verify_cli_works().ok(), force);
        }
    }

    let pending = if let Some(record) = existing {
        let (package_present, launcher_present) = owned_payload_present(&record.prefix)?;
        if !package_present && !launcher_present && which_cmd(COMMAND).is_some() {
            return Err(
                "ownership 存在但 CodeCLI 专属 Claude Code 副作用已不存在，同时检测到用户自装 CLI。请先点击卸载以只清理本工具记录，本次拒绝安装/覆盖"
                    .into(),
            );
        }
        // Installed 是此前已经完成固定版本/归属验证的状态，可执行
        // --version 做健康检查。Fresh Pending 没有这个信任前提，绝不
        // 运行其 package code/native 来“自证”可信。
        if record.state == CliInstallState::Installed
            && package_present
            && launcher_present
            && record.receipt.as_ref().is_some_and(|receipt| {
                verify_pinned_bundle_receipt(&record.prefix, PKG, COMMAND, receipt).is_ok()
            })
        {
            if let Ok(version) = verify_owned_npm_cli(&record.prefix, PKG, COMMAND) {
                if !force && is_supported_version(&version) {
                    let ensured_prefix = ensure_owned_npm_prefix()?;
                    if ensured_prefix != record.prefix {
                        return Err("Claude Code ownership prefix 与 PATH 配置目标不一致".into());
                    }
                    return Ok(InstallResult {
                        ok: true,
                        skipped: true,
                        version: Some(version.clone()),
                        message: format!("本工具安装的 Claude Code 已可用 {}，跳过", version),
                    });
                }
            }
        }

        if record.state == CliInstallState::Installed {
            let next = CliInstallRecord::pending_from(&record);
            // npm 副作用前先把 installed 降为 pending；失败/崩溃时仍能精确清理。
            persist_cli_install_record(&path, &next, Some(&record))?;
            next
        } else {
            record
        }
    } else {
        let next = CliInstallRecord::pending(PKG, prefix.clone());
        // 这是首次归属保留，必须发生在固定 bundle 下载/发布之前。
        persist_cli_install_record(&path, &next, None)?;
        next
    };

    check_cancelled()?;
    // 直接调用该 command 时也必须守住上游 engines >=22，
    // 不能只依赖外层安装编排器。
    super::runtime::ensure_node_sync(Some(22))?;
    let ensured_prefix = ensure_owned_npm_prefix()?;
    if ensured_prefix != pending.prefix {
        return Err("Claude Code Pending ownership prefix 与安装目标不一致".into());
    }
    let (package_present, launcher_present) = owned_payload_present(&pending.prefix)?;
    if let Some(receipt) = pending.receipt.as_ref() {
        remove_pinned_bundle_exact(&pending.prefix, PKG, COMMAND, receipt)?;
    } else if package_present || launcher_present {
        return Err(
            "Claude Code Pending 存在副作用但缺少首次发布前 durable 收据；已 fail closed，不执行、不删除、不调用 npm"
                .into(),
        );
    }
    check_cancelled()?;
    let prepared = prepare_claude_bundle(&pending.prefix).map_err(|error| {
        format!("Claude Code 固定官方 tarball/SRI bundle 准备失败（最终 prefix 尚未发布）: {error}")
    })?;
    let receipt_pending = pending.with_receipt(prepared.receipt().clone());
    // 这是硬边界：首个 final-prefix rename 之前必须将精确
    // package/launcher 指纹用 CAS 持久绑定到 Pending。
    persist_cli_install_record(&path, &receipt_pending, Some(&pending))?;
    prepared.publish().map_err(|error| {
        format!("Claude Code 固定 bundle 发布失败（Pending 已持久收据，可精确恢复/卸载）: {error}")
    })?;
    verify_pinned_bundle_receipt(
        &receipt_pending.prefix,
        PKG,
        COMMAND,
        receipt_pending.receipt.as_ref().expect("receipt just set"),
    )?;
    let version = verify_owned_npm_cli(&receipt_pending.prefix, PKG, COMMAND).map_err(|error| {
        format!(
            "固定 bundle 发布结束但 CodeCLI 专属 Claude Code 验证失败（Pending ownership 已保留，可重试或卸载）: {error}"
        )
    })?;
    if !is_supported_version(&version) {
        return Err(format!(
            "固定 bundle 的 Claude Code 版本不是已审计的 {SUPPORTED_PACKAGE_VERSION}（实际 {version}）；Pending ownership 已保留，已拒绝宣告安装成功"
        ));
    }
    let installed = CliInstallRecord::installed_from(&receipt_pending, version.clone());
    persist_cli_install_record(&path, &installed, Some(&receipt_pending))?;
    Ok(InstallResult {
        ok: true,
        skipped: false,
        version: Some(version),
        message: format!(
            "Claude Code {SUPPORTED_PACKAGE_VERSION} 已从官方固定 tarball 安装到 CodeCLI 专属 prefix，并完成 SHA-512 SRI 与精确归属验证"
        ),
    })
}

#[tauri::command]
pub async fn uninstall_claude_code() -> Result<InstallResult, String> {
    super::util::spawn_blocking_result(|| with_new_operation(uninstall_claude_code_sync)).await
}

pub fn uninstall_claude_code_sync() -> Result<InstallResult, String> {
    let path = install_record_path()?;
    let Some(record) = load_install_record()? else {
        return Ok(InstallResult {
            ok: true,
            skipped: true,
            version: None,
            message: "没有本工具的 Claude Code 精确归属记录，已跳过；用户自装 CLI 不会被删除"
                .into(),
        });
    };

    let (package_present, launcher_present) = owned_payload_present(&record.prefix)?;
    if let Some(receipt) = record.receipt.as_ref() {
        remove_pinned_bundle_exact(&record.prefix, PKG, COMMAND, receipt)?;
    } else if package_present || launcher_present {
        return Err(
            "Claude Code ownership 缺少发布前收据且仍有副作用；已拒绝调用 npm 或猜测删除".into(),
        );
    }

    let (package_still_present, launcher_still_present) = owned_payload_present(&record.prefix)?;
    if package_still_present || launcher_still_present {
        return Err("收据限定的精确卸载后仍有 Claude Code 包或 launcher；ownership 已保留".into());
    }
    // 只有在精确包和 launcher 都证明不存在后，才持久删除 record。
    remove_cli_install_record(&path, &record)?;
    refresh_path_from_system();
    let other_install_remains = which_cmd(COMMAND).is_some();
    Ok(InstallResult {
        ok: true,
        skipped: false,
        version: None,
        message: if other_install_remains {
            "已卸载 CodeCLI 专属 Claude Code；PATH 中仍有另一份用户安装，未触碰".into()
        } else {
            "已卸载 CodeCLI 专属 Claude Code，并在确认副作用不存在后删除归属记录".into()
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_audited_claude_package_version_is_supported() {
        assert!(is_supported_version("2.1.211 (Claude Code)"));
        assert!(!is_supported_version("2.1.210 (Claude Code)"));
        assert!(!is_supported_version("2.1.209 (Claude Code)"));
    }

    #[test]
    fn user_installed_cli_is_never_claimed_on_force_upgrade() {
        let error = unowned_cli_result(Some("2.0.0".into()), true)
            .expect_err("force must not overwrite a user-owned CLI");
        assert!(error.contains("拒绝强制升级"));
    }

    #[test]
    fn current_user_installed_cli_is_only_skipped() {
        let result = unowned_cli_result(Some("2.0.0".into()), false).expect("safe skip");
        assert!(result.skipped);
        assert!(result.message.contains("不抢占"));
    }

    #[test]
    fn unreadable_user_installed_version_is_not_treated_as_current() {
        let error = unowned_cli_result(Some("unknown".into()), false)
            .expect_err("unparseable version cannot authorize a skip/upgrade");
        assert!(error.contains("无法验证"));
    }
}
