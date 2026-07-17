// SPDX-License-Identifier: MPL-2.0
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};

use super::claude_code::install_claude_code_sync;
use super::codex_app::install_codex_app_sync;
use super::codex_cli::install_codex_cli_sync;
use super::config::{apply_config_sync, ConfigApplyRequest};
use super::connectivity::{test_connectivity_sync, ConnectivityRequest};
use super::runtime::ensure_node_sync;
use super::system::probe_system_sync;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallPlan {
    pub install_claude: bool,
    pub install_codex_cli: bool,
    pub install_codex_app: bool,
    pub prefer_mirror: bool,
    /// 若提供则安装末尾写配置
    pub config: Option<ConfigApplyRequest>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StepStatus {
    pub id: String,
    pub title: String,
    pub status: String, // pending|running|success|skipped|failed
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallProgressEvent {
    pub step_id: String,
    pub status: String,
    pub message: String,
    pub log_line: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallPlanResult {
    pub ok: bool,
    pub failed_step: Option<String>,
    pub requires_restart: bool,
    pub steps: Vec<StepStatus>,
}

fn emit(app: &AppHandle, step_id: &str, status: &str, message: &str) {
    let ev = InstallProgressEvent {
        step_id: step_id.into(),
        status: status.into(),
        message: message.into(),
        log_line: message.into(),
    };
    let _ = app.emit("install-progress", ev);
}

fn skip_remaining(app: &AppHandle, steps: &mut [StepStatus], from_id: &str) {
    let mut skip = false;
    for s in steps.iter_mut() {
        if s.id == from_id {
            skip = true;
            continue;
        }
        if skip && s.status == "pending" {
            s.status = "skipped".into();
            s.message = "因前序步骤失败跳过".into();
            emit(app, &s.id, "skipped", &s.message);
        }
    }
}

#[tauri::command]
pub async fn run_install_plan(
    app: AppHandle,
    plan: InstallPlan,
) -> Result<InstallPlanResult, String> {
    super::util::spawn_blocking_result(move || {
        super::op_lock::with_op_lock(|| run_install_plan_sync(app, plan))
    })
    .await
}

fn run_install_plan_sync(app: AppHandle, plan: InstallPlan) -> Result<InstallPlanResult, String> {
    let mut steps: Vec<StepStatus> = vec![
        StepStatus {
            id: "probe".into(),
            title: "系统检查".into(),
            status: "pending".into(),
            message: String::new(),
        },
        StepStatus {
            id: "node".into(),
            title: "安装 Node.js".into(),
            status: "pending".into(),
            message: String::new(),
        },
        StepStatus {
            id: "claude".into(),
            title: "安装 Claude Code".into(),
            status: "pending".into(),
            message: String::new(),
        },
        StepStatus {
            id: "codex_cli".into(),
            title: "安装 Codex CLI".into(),
            status: "pending".into(),
            message: String::new(),
        },
        StepStatus {
            id: "codex_app".into(),
            title: "安装 ChatGPT/Codex App".into(),
            status: "pending".into(),
            message: String::new(),
        },
        StepStatus {
            id: "config".into(),
            title: "写入配置".into(),
            status: "pending".into(),
            message: String::new(),
        },
        StepStatus {
            id: "connectivity".into(),
            title: "连通性验证".into(),
            status: "pending".into(),
            message: String::new(),
        },
        StepStatus {
            id: "done".into(),
            title: "完成".into(),
            status: "pending".into(),
            message: String::new(),
        },
    ];

    let need_node = plan.install_claude || plan.install_codex_cli;
    let requires_restart = false;

    // 1 probe（OS / 网络 / 磁盘硬预检）
    super::cmd::clear_cancel();
    set_running(&app, &mut steps, "probe", "正在检查系统环境...");
    match probe_system_sync() {
        Ok(r) => {
            let msg = format!(
                "{} | {} | 网络:{} | 磁盘:{:.1}GB{} | Node:{}",
                r.os,
                r.support_message,
                if r.network_ok { "OK" } else { "NG" },
                r.disk_free_gb,
                if r.disk_known { "" } else { "(未知)" },
                r.node_version.clone().unwrap_or_else(|| "未安装".into())
            );

            if !r.os_supported {
                set_failed(&app, &mut steps, "probe", &r.support_message);
                skip_remaining(&app, &mut steps, "probe");
                return Ok(InstallPlanResult {
                    ok: false,
                    failed_step: Some("probe".into()),
                    requires_restart: false,
                    steps,
                });
            }

            // CLI 平台原生包 + Node archive + npm cache/解包临时副本
            // 在同时安装 Claude/Codex 时可超过 1GB；预留 2GB
            // 避免 npm 在持久化 Pending ownership 后因 ENOSPC 半途失败。
            // 磁盘未知时仍只警告继续，不制造假阻断。
            let required_disk_gb = if need_node { 2.0 } else { 1.0 };
            if r.disk_known && r.disk_free_gb < required_disk_gb {
                set_failed(
                    &app,
                    &mut steps,
                    "probe",
                    &format!("磁盘可用空间不足 {required_disk_gb:.0}GB，请清理后重试"),
                );
                skip_remaining(&app, &mut steps, "probe");
                return Ok(InstallPlanResult {
                    ok: false,
                    failed_step: Some("probe".into()),
                    requires_restart: false,
                    steps,
                });
            }

            // 网络：装 CLI 时必须至少一个 npm 源；无 Node 时还要有装 Node 通道
            if need_node {
                if let Err(e) =
                    super::system::network_sufficient_for_install(r.node_installed, &r.network)
                {
                    set_failed(&app, &mut steps, "probe", &e);
                    skip_remaining(&app, &mut steps, "probe");
                    return Ok(InstallPlanResult {
                        ok: false,
                        failed_step: Some("probe".into()),
                        requires_restart: false,
                        steps,
                    });
                }
            }

            set_success(&app, &mut steps, "probe", &msg);
        }
        Err(e) => {
            set_failed(&app, &mut steps, "probe", &e);
            skip_remaining(&app, &mut steps, "probe");
            return Ok(InstallPlanResult {
                ok: false,
                failed_step: Some("probe".into()),
                requires_restart: false,
                steps,
            });
        }
    }

    // 2 node — 仅当要装 CLI
    if need_node {
        set_running(&app, &mut steps, "node", "检查 / 安装 Node.js...");
        // 当前 Claude Code npm wrapper 明确要求 Node >=22；只装 Codex
        // 时仍允许 Node 18。不能先用 18 放行、再让 npm 在 Claude
        // 步骤留下半安装的 Pending ownership。
        let min_node_major = if plan.install_claude { 22 } else { 18 };
        match ensure_node_sync(Some(min_node_major)) {
            Ok(st) => {
                if st.requires_restart {
                    set_status(&app, &mut steps, "node", "failed", &st.message);
                    skip_remaining(&app, &mut steps, "node");
                    return Ok(InstallPlanResult {
                        ok: false,
                        failed_step: Some("node".into()),
                        requires_restart: true,
                        steps,
                    });
                }
                let status = if st.skipped { "skipped" } else { "success" };
                set_status(&app, &mut steps, "node", status, &st.message);
            }
            Err(e) => {
                set_failed(&app, &mut steps, "node", &e);
                skip_remaining(&app, &mut steps, "node");
                return Ok(InstallPlanResult {
                    ok: false,
                    failed_step: Some("node".into()),
                    requires_restart: false,
                    steps,
                });
            }
        }
    } else {
        set_status(
            &app,
            &mut steps,
            "node",
            "skipped",
            "未选择 CLI 安装，跳过 Node",
        );
    }

    // 3 claude
    if plan.install_claude {
        set_running(&app, &mut steps, "claude", "正在安装 Claude Code...");
        match install_claude_code_sync(Some(plan.prefer_mirror)) {
            Ok(r) => {
                let status = if r.skipped { "skipped" } else { "success" };
                set_status(&app, &mut steps, "claude", status, &r.message);
            }
            Err(e) => {
                set_failed(&app, &mut steps, "claude", &e);
                skip_remaining(&app, &mut steps, "claude");
                return Ok(InstallPlanResult {
                    ok: false,
                    failed_step: Some("claude".into()),
                    requires_restart,
                    steps,
                });
            }
        }
    } else {
        set_status(
            &app,
            &mut steps,
            "claude",
            "skipped",
            "用户未选择安装 Claude Code",
        );
    }

    // 4 codex cli
    if plan.install_codex_cli {
        set_running(&app, &mut steps, "codex_cli", "正在安装 Codex CLI...");
        match install_codex_cli_sync(Some(plan.prefer_mirror)) {
            Ok(r) => {
                let status = if r.skipped { "skipped" } else { "success" };
                set_status(&app, &mut steps, "codex_cli", status, &r.message);
            }
            Err(e) => {
                set_failed(&app, &mut steps, "codex_cli", &e);
                skip_remaining(&app, &mut steps, "codex_cli");
                return Ok(InstallPlanResult {
                    ok: false,
                    failed_step: Some("codex_cli".into()),
                    requires_restart,
                    steps,
                });
            }
        }
    } else {
        set_status(
            &app,
            &mut steps,
            "codex_cli",
            "skipped",
            "用户未选择安装 Codex CLI",
        );
    }

    // 5 桌面 App：只负责打开下载页，不把“页面已打开”
    // 偷换为“App 已安装”。用户明确勾选后，未检测到安装仍使整体
    // 结果为 action-required（failed_step=codex_app）；CLI 终验仍独立执行。
    let mut app_note = String::new();
    let mut app_ok = true;
    if plan.install_codex_app {
        set_running(
            &app,
            &mut steps,
            "codex_app",
            "处理 ChatGPT/Codex 桌面 App...",
        );
        match install_codex_app_sync() {
            Ok(r) => {
                if r.skipped {
                    app_note = r.message.clone();
                    set_status(&app, &mut steps, "codex_app", "skipped", &r.message);
                } else {
                    let msg = format!("需手动安装桌面 App（未计入完成）: {}", r.message);
                    app_note = msg.clone();
                    app_ok = false;
                    set_status(&app, &mut steps, "codex_app", "failed", &msg);
                }
            }
            Err(e) => {
                let msg = format!("打开下载页失败；CLI 安装结果将继续单独验收: {}", e);
                app_note = msg.clone();
                app_ok = false;
                set_status(&app, &mut steps, "codex_app", "failed", &msg);
            }
        }
    } else {
        set_status(
            &app,
            &mut steps,
            "codex_app",
            "skipped",
            "用户未选择 ChatGPT/Codex 桌面 App",
        );
    }

    // 6 config + connectivity：默认必须先真实测通，再写入本机。
    // 这样失败不会把一份未验证配置留在用户环境里；“强制保存”只存在于
    // 明确二次确认的独立配置弹窗，不属于自动安装计划。
    if let Some(cfg) = plan.config {
        let connectivity_req = ConnectivityRequest {
            provider_id: cfg.provider_id.clone(),
            api_key: cfg.api_key.clone(),
            base_url: cfg.base_url.clone(),
            protocol: Some(if cfg.target == "claude" {
                "anthropic".into()
            } else {
                "openai".into()
            }),
            model: cfg.model.clone(),
        };
        set_running(
            &app,
            &mut steps,
            "connectivity",
            "写入前正在验证真实 API 连通性...",
        );
        match test_connectivity_sync(connectivity_req) {
            Ok(test) if test.ok => {
                set_success(&app, &mut steps, "connectivity", &test.message);
            }
            Ok(test) => {
                set_failed(&app, &mut steps, "connectivity", &test.message);
                set_status(
                    &app,
                    &mut steps,
                    "config",
                    "skipped",
                    "连通测试未通过，未写入本机配置",
                );
                skip_remaining(&app, &mut steps, "connectivity");
                return Ok(InstallPlanResult {
                    ok: false,
                    failed_step: Some("connectivity".into()),
                    requires_restart,
                    steps,
                });
            }
            Err(e) => {
                set_failed(&app, &mut steps, "connectivity", &e);
                set_status(
                    &app,
                    &mut steps,
                    "config",
                    "skipped",
                    "连通测试出错，未写入本机配置",
                );
                skip_remaining(&app, &mut steps, "connectivity");
                return Ok(InstallPlanResult {
                    ok: false,
                    failed_step: Some("connectivity".into()),
                    requires_restart,
                    steps,
                });
            }
        }

        set_running(
            &app,
            &mut steps,
            "config",
            "连通测试通过，正在写入 API 配置...",
        );
        match apply_config_sync(cfg) {
            Ok(result) => set_success(&app, &mut steps, "config", &result.message),
            Err(error) => {
                set_failed(&app, &mut steps, "config", &error);
                skip_remaining(&app, &mut steps, "config");
                return Ok(InstallPlanResult {
                    ok: false,
                    failed_step: Some("config".into()),
                    requires_restart,
                    steps,
                });
            }
        }
    } else {
        set_status(
            &app,
            &mut steps,
            "config",
            "skipped",
            "未提供配置，可稍后点「配置 API Key」",
        );
        set_status(
            &app,
            &mut steps,
            "connectivity",
            "skipped",
            "未提供配置，跳过连通性验证",
        );
    }

    // 7 终验：所选 CLI 必须 --version 可用
    set_running(&app, &mut steps, "done", "正在验收安装结果...");
    let mut verify_msgs = Vec::new();
    let mut verify_ok = true;
    if plan.install_claude {
        match super::claude_code::claude_code_version_sync() {
            Some(v) => verify_msgs.push(format!("claude OK ({})", v)),
            None => {
                verify_ok = false;
                verify_msgs.push("claude 不可用".into());
            }
        }
    }
    if plan.install_codex_cli {
        match super::codex_cli::codex_cli_version_sync() {
            Some(v) => verify_msgs.push(format!("codex OK ({})", v)),
            None => {
                verify_ok = false;
                verify_msgs.push("codex 不可用".into());
            }
        }
    }
    if !plan.install_claude && !plan.install_codex_cli {
        verify_msgs.push("未选择 CLI，跳过 CLI 终验".into());
    }

    let mut summary = verify_msgs.join("；");
    if !app_note.is_empty() {
        summary = format!("{}｜App: {}", summary, app_note);
    }
    // 总结果同时要求所选 CLI 终验与所选桌面 App 真实安装。
    let only_app = plan.install_codex_app && !plan.install_claude && !plan.install_codex_cli;
    if verify_ok && app_ok {
        let done_msg = if only_app {
            format!("未安装 CLI（仅 App 引导）。{}。", summary)
        } else if plan.install_claude || plan.install_codex_cli {
            format!("CLI 安装完成。{}。请「配置 API Key」并测试连通。", summary)
        } else {
            format!("流程结束（未选择安装项）。{}。", summary)
        };
        set_success(&app, &mut steps, "done", &done_msg);
        Ok(InstallPlanResult {
            ok: true,
            failed_step: None,
            requires_restart,
            steps,
        })
    } else {
        // CLI 终验失败优先标为 done；只有 CLI 已验收成功时，
        // failed_step=codex_app 才能成为前端继续 API onboarding 的可靠信号。
        let failed_step = if !verify_ok { "done" } else { "codex_app" };
        let failure_prefix = if !app_ok && verify_ok {
            "ChatGPT/Codex 桌面 App 尚未检测为已安装"
        } else {
            "CLI 安装未完全成功"
        };
        let action_hint = if !app_ok && verify_ok {
            "已可用的 CLI 可继续配置 API；请安装桌面 App 后重试该步，或取消勾选后重新运行"
        } else if !app_ok {
            "请重新运行安装并完成桌面 App 安装，或导出日志排查"
        } else {
            "请重新运行安装，或导出日志排查"
        };
        set_failed(
            &app,
            &mut steps,
            "done",
            &format!("{}：{}。{}", failure_prefix, summary, action_hint),
        );
        Ok(InstallPlanResult {
            ok: false,
            failed_step: Some(failed_step.into()),
            requires_restart,
            steps,
        })
    }
}

fn set_running(app: &AppHandle, steps: &mut [StepStatus], id: &str, msg: &str) {
    set_status(app, steps, id, "running", msg);
}

fn set_success(app: &AppHandle, steps: &mut [StepStatus], id: &str, msg: &str) {
    set_status(app, steps, id, "success", msg);
}

fn set_failed(app: &AppHandle, steps: &mut [StepStatus], id: &str, msg: &str) {
    set_status(app, steps, id, "failed", msg);
}

fn set_status(app: &AppHandle, steps: &mut [StepStatus], id: &str, status: &str, msg: &str) {
    if let Some(s) = steps.iter_mut().find(|s| s.id == id) {
        s.status = status.into();
        s.message = msg.into();
    }
    emit(app, id, status, msg);
}
