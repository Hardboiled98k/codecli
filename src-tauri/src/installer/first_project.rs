// SPDX-License-Identifier: MPL-2.0
//! 开始第一个项目：建目录、写需求 README、打开终端启动 CLI

use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::platform::{home_dir, which_cmd};
use super::util::{chrono_like_now, shell_single_quote};

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FirstProjectRequest {
    /// create | existing
    pub mode: String,
    /// 项目名（create 时用）
    pub name: Option<String>,
    /// 已有目录绝对路径（existing）
    pub path: Option<String>,
    /// 一句话目标
    pub goal: String,
    /// 技术栈/约束（可选）
    pub stack: Option<String>,
    /// 成功标准（可选）
    pub success: Option<String>,
    /// claude | codex
    pub tool: String,
    /// 是否写 README
    pub write_readme: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FirstProjectResult {
    pub ok: bool,
    pub path: String,
    pub readme_path: Option<String>,
    pub tool: String,
    pub tool_available: bool,
    pub prompts: Vec<String>,
    pub message: String,
    pub terminal_opened: bool,
}

fn sanitize_project_name(raw: &str) -> Result<String, String> {
    let s = raw.trim();
    if s.is_empty() {
        return Err("项目名不能为空".into());
    }
    if s.chars().count() > 64 {
        return Err("项目名过长（≤64）".into());
    }
    let ok = s.chars().all(|c| {
        c.is_alphanumeric()
            || c == '-'
            || c == '_'
            || c == '.'
            || ('\u{4e00}'..='\u{9fff}').contains(&c)
    });
    if !ok {
        return Err("项目名仅允许中文、字母、数字、.-_".into());
    }
    // 禁止路径穿越
    if s.contains("..") || s.contains('/') || s.contains('\\') {
        return Err("项目名不能含路径分隔符".into());
    }
    Ok(s.to_string())
}

fn projects_root() -> Result<PathBuf, String> {
    let home = home_dir().ok_or("找不到 HOME")?;
    let desktop = home.join("Desktop");
    let base = if desktop.is_dir() {
        desktop.join("CodeCLI-Projects")
    } else {
        home.join("CodeCLI-Projects")
    };
    Ok(base)
}

fn is_cli_config_path(path: &Path, home: &Path) -> bool {
    [home.join(".claude"), home.join(".codex")]
        .into_iter()
        .map(|root| root.canonicalize().unwrap_or(root))
        .any(|root| path == root || path.starts_with(&root))
}

fn validate_existing_dir(raw: &str) -> Result<PathBuf, String> {
    let p = PathBuf::from(raw.trim());
    if !p.is_absolute() {
        return Err("请使用绝对路径".into());
    }
    if !p.is_dir() {
        return Err(format!("目录不存在: {}", p.display()));
    }
    let canon = p
        .canonicalize()
        .map_err(|e| format!("无法解析路径: {}", e))?;
    let s = canon.to_string_lossy().to_string();
    if s == "/"
        || s.eq_ignore_ascii_case("C:\\")
        || s.eq_ignore_ascii_case("C:/")
        || s.ends_with(":/")
        || s.ends_with(":\\")
    {
        return Err("不能把系统根目录当作项目目录".into());
    }
    // 拒绝敏感目录
    let lower = s.to_lowercase();
    for ban in [
        "/.ssh",
        "\\.ssh",
        "/.gnupg",
        "/library/keychains",
        "/etc",
        "\\windows\\system32",
    ] {
        if lower.contains(ban) {
            return Err("拒绝在敏感目录创建/打开项目".into());
        }
    }
    // CLI 配置树中可含 Key、会话、ownership 和备份；整个子树都不能当项目。
    if let Some(home) = home_dir() {
        if let Ok(h) = home.canonicalize() {
            if is_cli_config_path(&canon, &h) {
                return Err("请勿使用 CLI 配置目录或其子目录作为项目路径".into());
            }
        }
    }
    Ok(canon)
}

fn build_readme(name: &str, goal: &str, stack: &str, success: &str, tool: &str) -> String {
    format!(
        r#"# {name}

> 由 CodeCLI「开始第一个项目」生成 · {now}

## 目标

{goal}

## 技术栈 / 约束

{stack}

## 成功标准

{success}

## 建议用法

本项目用 **{tool_label}** 启动。

### 首轮可以这样说

1. 先读本 README 和当前目录，用中文总结项目目标与现状
2. 给出分步计划（先小后大，每步可验证）
3. 做一个最小、可回滚的改动，并说明改了什么

### 安全提醒

- 不要把 API Key 写进仓库
- 大改前先确认；重要文件先备份
- 看不懂的命令先问再执行

---

生成时间：{now}
"#,
        name = name,
        goal = goal.trim(),
        stack = if stack.trim().is_empty() {
            "（未填写 — 可让 AI 根据目标推荐）"
        } else {
            stack.trim()
        },
        success = if success.trim().is_empty() {
            "能跑通最小示例 / 界面可见 / 无报错"
        } else {
            success.trim()
        },
        tool_label = if tool == "codex" {
            "Codex CLI"
        } else {
            "Claude Code"
        },
        now = chrono_like_now(),
    )
}

fn prompts_for(tool: &str) -> Vec<String> {
    let who = if tool == "codex" { "Codex" } else { "Claude" };
    vec![
        format!("请阅读本目录与 README，用中文总结项目目标、现状和风险。"),
        format!("基于 README，给出分步实现计划（每步可验证），先做最小可行。"),
        format!("请做一个小且可回滚的改动，并说明你改了哪些文件。不要动与目标无关的文件。"),
        format!("（可选）用 {who} 解释刚生成/修改的代码，指出下一步最值得做的一件事。"),
    ]
}

/// 在项目目录打开终端并尽量启动 CLI（安全：不拼用户自由 shell）
fn open_terminal_in(dir: &Path, tool: &str) -> Result<bool, String> {
    let dir_s = dir.to_str().ok_or("项目路径含非法字符")?;
    let bin = if tool == "codex" { "codex" } else { "claude" };
    let has = which_cmd(bin).is_some();

    if cfg!(target_os = "macos") {
        // Terminal.app：cd 后执行 claude/codex；无则只 cd
        let cmd = if has {
            format!("cd {} && {}", shell_single_quote(dir_s), bin)
        } else {
            format!("cd {}", shell_single_quote(dir_s))
        };
        // AppleScript 字符串转义
        let escaped = cmd.replace('\\', "\\\\").replace('"', "\\\"");
        let script = format!(
            "tell application \"Terminal\"\n activate\n do script \"{}\"\nend tell",
            escaped
        );
        let status = Command::new("osascript")
            .arg("-e")
            .arg(&script)
            .status()
            .map_err(|e| format!("打开 Terminal 失败: {}", e))?;
        if !status.success() {
            // 回退：只打开目录
            let _ = Command::new("open").arg(dir_s).status();
            return Ok(false);
        }
        return Ok(true);
    }

    if cfg!(target_os = "windows") {
        // 用 PowerShell Start-Process -WorkingDirectory 避免 cmd 二次解析路径
        let wd = dir_s.replace('\'', "''");
        let ps = if has {
            // 优先绝对路径（which_cmd），避免 cwd 下恶意同名
            let abs = which_cmd(bin).unwrap_or_else(|| bin.to_string());
            let abs_e = abs.replace('\'', "''");
            format!(
                "Start-Process -FilePath 'cmd.exe' -WorkingDirectory '{}' -ArgumentList '/K','\"{}\"'",
                wd, abs_e
            )
        } else {
            format!(
                "Start-Process -FilePath 'cmd.exe' -WorkingDirectory '{}'",
                wd
            )
        };
        let status = Command::new("powershell")
            .args(["-NoProfile", "-Command", &ps])
            .status()
            .map_err(|e| format!("打开终端失败: {}", e))?;
        return Ok(status.success());
    }

    // Linux 兜底：xdg-open 目录
    let _ = Command::new("xdg-open").arg(dir_s).status();
    Ok(false)
}

fn open_folder(dir: &Path) -> Result<(), String> {
    let s = dir.to_str().ok_or("路径非法")?;
    let status = if cfg!(target_os = "macos") {
        Command::new("open")
            .arg(s)
            .status()
            .map_err(|e| e.to_string())?
    } else if cfg!(target_os = "windows") {
        Command::new("explorer")
            .arg(s)
            .status()
            .map_err(|e| e.to_string())?
    } else {
        Command::new("xdg-open")
            .arg(s)
            .status()
            .map_err(|e| e.to_string())?
    };
    if !status.success() {
        return Err(format!("打开项目目录失败（退出码 {:?}）", status.code()));
    }
    Ok(())
}

fn write_readme_if_absent(path: &Path, body: &str) -> Result<(), String> {
    let mut file = match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Ok(()),
        Err(e) => return Err(format!("创建 {} 失败: {}", path.display(), e)),
    };
    if let Err(e) = file.write_all(body.as_bytes()).and_then(|_| file.flush()) {
        return Err(format!("写入 {} 失败: {}", path.display(), e));
    }
    Ok(())
}

pub fn prepare_first_project_sync(req: FirstProjectRequest) -> Result<FirstProjectResult, String> {
    let goal = req.goal.trim();
    if goal.is_empty() {
        return Err("请填写项目目标（一句话即可）".into());
    }
    if goal.chars().count() > 500 {
        return Err("目标过长".into());
    }
    let tool = req.tool.to_lowercase();
    if tool != "claude" && tool != "codex" {
        return Err("tool 须为 claude|codex".into());
    }
    let stack = req.stack.unwrap_or_default();
    let success = req.success.unwrap_or_default();
    if stack.chars().count() > 400 || success.chars().count() > 400 {
        return Err("技术栈/成功标准过长".into());
    }

    let mode = req.mode.to_lowercase();
    let (path, display_name) = if mode == "create" {
        let name = sanitize_project_name(req.name.as_deref().unwrap_or(""))?;
        let root = projects_root()?;
        std::fs::create_dir_all(&root).map_err(|e| format!("创建项目根目录失败: {}", e))?;
        let p = root.join(&name);
        match std::fs::create_dir(&p) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err(format!(
                    "目录已存在: {}\n请换名，或改用「打开已有文件夹」",
                    p.display()
                ));
            }
            Err(e) => return Err(format!("创建项目失败: {}", e)),
        }
        (p, name)
    } else if mode == "existing" {
        let p = validate_existing_dir(req.path.as_deref().unwrap_or(""))?;
        let name = p
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("project")
            .to_string();
        (p, name)
    } else {
        return Err("mode 须为 create|existing".into());
    };

    let write_readme = req.write_readme.unwrap_or(true);
    let mut readme_path = None;
    if write_readme {
        let rp = path.join("README.md");
        // create_new 从根源上保证并发新建的 README 也不会被覆盖。
        let body = build_readme(&display_name, goal, &stack, &success, &tool);
        write_readme_if_absent(&rp, &body)?;
        readme_path = Some(rp.display().to_string());
    }

    let tool_available = which_cmd(if tool == "codex" { "codex" } else { "claude" }).is_some();
    let terminal_opened = open_terminal_in(&path, &tool).unwrap_or(false);

    let mut message = format!("项目就绪：{}", path.display());
    if !tool_available {
        message.push_str(&format!(
            "\n未在 PATH 找到 `{}`，已打开终端到项目目录；请先安装 CLI 或新开终端后再运行。",
            if tool == "codex" { "codex" } else { "claude" }
        ));
    } else if terminal_opened {
        message.push_str(&format!(
            "\n已打开终端并尝试启动 `{}`。",
            if tool == "codex" { "codex" } else { "claude" }
        ));
    } else {
        message.push_str("\n未能自动开终端，请手动进入该目录运行 CLI。");
    }

    let prompts = prompts_for(&tool);
    Ok(FirstProjectResult {
        ok: true,
        path: path.display().to_string(),
        readme_path,
        tool,
        tool_available,
        prompts,
        message,
        terminal_opened,
    })
}

#[tauri::command]
pub async fn prepare_first_project(req: FirstProjectRequest) -> Result<FirstProjectResult, String> {
    super::util::spawn_blocking_result(move || prepare_first_project_sync(req)).await
}

#[tauri::command]
pub async fn open_project_folder(path: String) -> Result<String, String> {
    super::util::spawn_blocking_result(move || {
        let p = validate_existing_dir(&path)?;
        open_folder(&p)?;
        Ok(format!("已打开: {}", p.display()))
    })
    .await
}

#[tauri::command]
pub async fn open_project_terminal(path: String, tool: String) -> Result<String, String> {
    super::util::spawn_blocking_result(move || {
        let p = validate_existing_dir(&path)?;
        let t = tool.to_lowercase();
        if t != "claude" && t != "codex" {
            return Err("tool 须为 claude|codex".into());
        }
        let ok = open_terminal_in(&p, &t)?;
        if ok {
            Ok(format!("已在终端打开: {}", p.display()))
        } else {
            Ok(format!("已尝试打开目录: {}", p.display()))
        }
    })
    .await
}

#[tauri::command]
pub async fn pick_project_directory() -> Result<Option<String>, String> {
    super::util::spawn_blocking_result(pick_project_directory_sync).await
}

fn pick_project_directory_sync() -> Result<Option<String>, String> {
    #[cfg(target_os = "macos")]
    {
        let output = Command::new("osascript")
            .args([
                "-e",
                "POSIX path of (choose folder with prompt \"选择项目文件夹\")",
            ])
            .output()
            .map_err(|e| format!("打开系统目录选择器失败: {e}"))?;
        if !output.status.success() {
            let err = String::from_utf8_lossy(&output.stderr);
            if err.contains("-128") || err.to_ascii_lowercase().contains("cancel") {
                return Ok(None);
            }
            return Err(format!("目录选择器失败: {}", err.trim()));
        }
        let selected = String::from_utf8_lossy(&output.stdout)
            .trim()
            .trim_end_matches('/')
            .to_string();
        return if selected.is_empty() {
            Ok(None)
        } else {
            Ok(Some(selected))
        };
    }

    #[cfg(target_os = "windows")]
    {
        let script = r#"Add-Type -AssemblyName System.Windows.Forms; $d = New-Object System.Windows.Forms.FolderBrowserDialog; $d.Description = '选择项目文件夹'; if ($d.ShowDialog() -eq [System.Windows.Forms.DialogResult]::OK) { [Console]::Out.Write($d.SelectedPath) }"#;
        let output = Command::new("powershell")
            .args(["-NoProfile", "-STA", "-Command", script])
            .output()
            .map_err(|e| format!("打开系统目录选择器失败: {e}"))?;
        if !output.status.success() {
            return Err(format!(
                "目录选择器失败: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        let selected = String::from_utf8_lossy(&output.stdout).trim().to_string();
        return if selected.is_empty() {
            Ok(None)
        } else {
            Ok(Some(selected))
        };
    }

    #[cfg(target_os = "linux")]
    {
        let output = Command::new("zenity")
            .args(["--file-selection", "--directory", "--title=选择项目文件夹"])
            .output()
            .map_err(|_| "未找到系统目录选择器 zenity，请手工粘贴绝对路径".to_string())?;
        if !output.status.success() {
            return Ok(None);
        }
        let selected = String::from_utf8_lossy(&output.stdout).trim().to_string();
        return if selected.is_empty() {
            Ok(None)
        } else {
            Ok(Some(selected))
        };
    }

    #[allow(unreachable_code)]
    Err("当前系统不支持目录选择器，请手工粘贴绝对路径".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_rejects_path() {
        assert!(sanitize_project_name("../x").is_err());
        assert!(sanitize_project_name("a/b").is_err());
        assert!(sanitize_project_name("my-app").is_ok());
    }

    #[test]
    fn readme_contains_goal() {
        let s = build_readme("demo", "做一个待办", "React", "能增删", "claude");
        assert!(s.contains("做一个待办"));
        assert!(s.contains("demo"));
    }

    #[test]
    fn cli_config_subtrees_are_always_rejected() {
        let home = PathBuf::from("/tmp/codecli-first-project-home");
        assert!(is_cli_config_path(&home.join(".claude"), &home));
        assert!(is_cli_config_path(
            &home.join(".claude/projects/nested"),
            &home
        ));
        assert!(is_cli_config_path(
            &home.join(".codex/sessions/2026/session.jsonl"),
            &home
        ));
        assert!(is_cli_config_path(&home.join(".codex/mcp/cache"), &home));
        assert!(!is_cli_config_path(&home.join("projects/demo"), &home));
    }
}
