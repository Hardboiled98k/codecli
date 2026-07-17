// SPDX-License-Identifier: MPL-2.0
import { invoke } from "@tauri-apps/api/core";
import type {
  BackupActionResult,
  BackupListResult,
  ConfigApplyRequest,
  ConfigApplyResult,
  ConnectivityResult,
  ExtensionActionResult,
  ExtensionListResult,
  FirstProjectResult,
  HealthFixResult,
  HealthReport,
  InstallPlanResult,
  Provider,
  SchemeListResult,
  SystemReport,
  UpgradeResult,
  VersionsReport,
} from "../types";

export const api = {
  appendDiagnosticLog: (line: string) =>
    invoke<void>("append_diagnostic_log", { line }),
  exportDiagnosticLog: (lines: string[]) =>
    invoke<string>("export_diagnostic_log", { lines }),
  resumeDiagnosticLog: () => invoke<void>("resume_diagnostic_log"),
  probeSystem: () => invoke<SystemReport>("probe_system"),
  ensureNode: (minMajor = 18) =>
    invoke<{
      ok: boolean;
      skipped: boolean;
      nodeVersion?: string | null;
      message: string;
      requiresRestart: boolean;
    }>("ensure_node", { minMajor }),
  installClaude: (preferMirror?: boolean) =>
    invoke<{ ok: boolean; skipped: boolean; version?: string | null; message: string }>(
      "install_claude_code",
      { preferMirror: preferMirror ?? null },
    ),
  installCodexCli: (preferMirror?: boolean) =>
    invoke<{ ok: boolean; skipped: boolean; version?: string | null; message: string }>(
      "install_codex_cli",
      { preferMirror: preferMirror ?? null },
    ),
  installCodexApp: () =>
    invoke<{ ok: boolean; skipped: boolean; status: string; message: string }>(
      "install_codex_app",
    ),
  listProviders: () => invoke<Provider[]>("list_providers"),
  runInstallPlan: (plan: {
    installClaude: boolean;
    installCodexCli: boolean;
    installCodexApp: boolean;
    preferMirror: boolean;
    config?: ConfigApplyRequest | null;
  }) =>
    invoke<InstallPlanResult>("run_install_plan", {
      plan: {
        installClaude: plan.installClaude,
        installCodexCli: plan.installCodexCli,
        installCodexApp: plan.installCodexApp,
        preferMirror: plan.preferMirror,
        config: plan.config ?? null,
      },
    }),
  applyConfig: (req: ConfigApplyRequest) =>
    invoke<ConfigApplyResult>("apply_config", { req }),
  clearConfig: (target: string) =>
    invoke<ConfigApplyResult>("clear_config", { target }),
  purgeToolData: () => invoke<ConfigApplyResult>("purge_tool_data"),
  testConnectivity: (req: {
    providerId: string;
    apiKey: string;
    baseUrl?: string;
    protocol?: string;
    model?: string;
  }) => invoke<ConnectivityResult>("test_connectivity", { req }),
  uninstallClaude: () =>
    invoke<{ ok: boolean; skipped: boolean; message: string }>("uninstall_claude_code"),
  uninstallCodex: () =>
    invoke<{ ok: boolean; skipped: boolean; message: string }>("uninstall_codex_cli"),
  cancelInstall: () => invoke<void>("cancel_install"),
  listSchemes: () => invoke<SchemeListResult>("list_schemes"),
  switchScheme: (schemeId: string) =>
    invoke<ConfigApplyResult>("switch_scheme", { schemeId }),
  deleteScheme: (schemeId: string) =>
    invoke<ConfigApplyResult>("delete_scheme", { schemeId }),
  upsertScheme: (req: {
    id?: string;
    name?: string;
    target: string;
    providerId: string;
    apiKey: string;
    baseUrl?: string;
    model?: string;
    apply?: boolean;
  }) => invoke<ConfigApplyResult>("upsert_scheme", { req }),
  healthCheck: () => invoke<HealthReport>("health_check"),
  healthFix: (fixIds?: string[]) =>
    invoke<HealthFixResult>("health_fix", { fixIds: fixIds ?? null }),
  prepareFirstProject: (req: {
    mode: "create" | "existing" | string;
    name?: string;
    path?: string;
    goal: string;
    stack?: string;
    success?: string;
    tool: "claude" | "codex" | string;
    writeReadme?: boolean;
  }) => invoke<FirstProjectResult>("prepare_first_project", { req }),
  openProjectFolder: (path: string) =>
    invoke<string>("open_project_folder", { path }),
  openProjectTerminal: (path: string, tool: string) =>
    invoke<string>("open_project_terminal", { path, tool }),
  pickProjectDirectory: () =>
    invoke<string | null>("pick_project_directory"),
  createBackup: (note?: string) =>
    invoke<BackupActionResult>("create_backup", { note: note ?? null }),
  listBackups: () => invoke<BackupListResult>("list_backups"),
  restoreBackup: (backupId: string) =>
    invoke<BackupActionResult>("restore_backup", { backupId }),
  deleteBackup: (backupId: string) =>
    invoke<BackupActionResult>("delete_backup", { backupId }),
  openBackupsFolder: () => invoke<string>("open_backups_folder"),
  versionsReport: () => invoke<VersionsReport>("versions_report"),
  upgradeComponent: (id: string, preferMirror?: boolean) =>
    invoke<UpgradeResult>("upgrade_component", {
      id,
      preferMirror: preferMirror ?? null,
    }),
  listExtensions: () => invoke<ExtensionListResult>("list_extensions"),
  installExtension: (id: string) =>
    invoke<ExtensionActionResult>("install_extension", { id }),
  uninstallExtension: (id: string) =>
    invoke<ExtensionActionResult>("uninstall_extension", { id }),
};
