// SPDX-License-Identifier: MPL-2.0
mod installer;

#[cfg(desktop)]
use tauri::Manager;

use installer::{
    append_diagnostic_log, apply_config, claude_code_version, clear_config, codex_app_available,
    codex_cli_version, create_backup, delete_backup, delete_scheme, ensure_node,
    export_diagnostic_log, health_check, health_fix, install_claude_code, install_codex_app,
    install_codex_cli, install_extension, list_backups, list_extensions, list_providers,
    list_schemes, open_backups_folder, open_project_folder, open_project_terminal,
    pick_project_directory, prepare_first_project, probe_system, purge_tool_data, restore_backup,
    resume_diagnostic_log, run_install_plan, switch_scheme, test_connectivity,
    uninstall_claude_code, uninstall_codex_cli, uninstall_extension, upgrade_component,
    upsert_scheme, versions_report,
};

#[tauri::command]
fn cancel_install() {
    installer::cmd::request_cancel();
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let builder = tauri::Builder::default();
    // 桌面端保持单实例，重复打开时聚焦已有主窗口。
    // 按 Tauri 要求，single-instance 必须先于其它插件注册。
    #[cfg(desktop)]
    let builder = builder.plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
        if let Some(window) = app.get_webview_window("main") {
            let _ = window.show();
            let _ = window.set_focus();
        }
    }));

    let builder =
        builder
            .plugin(tauri_plugin_opener::init())
            .invoke_handler(tauri::generate_handler![
                probe_system,
                list_providers,
                ensure_node,
                install_claude_code,
                uninstall_claude_code,
                claude_code_version,
                install_codex_cli,
                uninstall_codex_cli,
                codex_cli_version,
                install_codex_app,
                codex_app_available,
                apply_config,
                clear_config,
                purge_tool_data,
                test_connectivity,
                run_install_plan,
                cancel_install,
                list_schemes,
                upsert_scheme,
                switch_scheme,
                delete_scheme,
                health_check,
                health_fix,
                prepare_first_project,
                open_project_folder,
                open_project_terminal,
                pick_project_directory,
                create_backup,
                list_backups,
                restore_backup,
                delete_backup,
                open_backups_folder,
                versions_report,
                upgrade_component,
                list_extensions,
                install_extension,
                uninstall_extension,
                append_diagnostic_log,
                export_diagnostic_log,
                resume_diagnostic_log,
            ]);

    builder
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
