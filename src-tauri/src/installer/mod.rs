// SPDX-License-Identifier: MPL-2.0
pub mod backup;
pub mod claude_code;
pub mod cmd;
pub mod codex_app;
pub mod codex_cli;
pub mod config;
pub mod connectivity;
pub mod extensions;
pub mod first_project;
pub mod health;
pub mod log_bus;
pub mod op_lock;
pub mod orchestrator;
pub mod pinned_npm;
pub mod platform;
pub mod providers;
pub mod runtime;
pub mod schemes;
pub mod system;
pub mod util;
pub mod versions;

/// 唯一客户端版本真源，避免 Cargo/Tauri/UI/HTTP 请求各自硬编码后漂移。
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

pub use backup::{create_backup, delete_backup, list_backups, open_backups_folder, restore_backup};
pub use claude_code::{claude_code_version, install_claude_code, uninstall_claude_code};
pub use codex_app::{codex_app_available, install_codex_app};
pub use codex_cli::{codex_cli_version, install_codex_cli, uninstall_codex_cli};
pub use config::{apply_config, clear_config, purge_tool_data};
pub use connectivity::test_connectivity;
pub use extensions::{install_extension, list_extensions, uninstall_extension};
pub use first_project::{
    open_project_folder, open_project_terminal, pick_project_directory, prepare_first_project,
};
pub use health::{health_check, health_fix};
pub use log_bus::{append_diagnostic_log, export_diagnostic_log, resume_diagnostic_log};
pub use orchestrator::run_install_plan;
pub use providers::list_providers;
pub use runtime::ensure_node;
pub use schemes::{delete_scheme, list_schemes, switch_scheme, upsert_scheme};
pub use system::probe_system;
pub use versions::{upgrade_component, versions_report};
