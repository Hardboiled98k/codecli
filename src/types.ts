// SPDX-License-Identifier: MPL-2.0
export interface NetworkProbe {
  npmOfficialOk: boolean;
  nodejsOrgOk: boolean;
  detail: string;
}

export interface SystemReport {
  os: string;
  osKind: string;
  osVersion?: string | null;
  arch: string;
  osSupported: boolean;
  supportMessage: string;
  networkOk: boolean;
  network: NetworkProbe;
  diskFreeGb: number;
  diskKnown: boolean;
  nodeInstalled: boolean;
  nodeVersion?: string | null;
  npmInstalled: boolean;
  npmVersion?: string | null;
  claudeInstalled: boolean;
  claudeVersion?: string | null;
  codexInstalled: boolean;
  codexVersion?: string | null;
  home?: string | null;
  hasAdminHint: boolean;
}

export interface Provider {
  id: string;
  name: string;
  group: string;
  protocol: string;
  baseUrl: string;
  keyUrl?: string | null;
  defaultModel?: string | null;
  note?: string | null;
}

export interface StepStatus {
  id: string;
  title: string;
  status: "pending" | "running" | "success" | "skipped" | "failed" | string;
  message: string;
}

export interface InstallProgressEvent {
  stepId: string;
  status: string;
  message: string;
  logLine: string;
}

export interface InstallPlanResult {
  ok: boolean;
  failedStep?: string | null;
  requiresRestart: boolean;
  steps: StepStatus[];
}

export interface ConfigApplyRequest {
  providerId: string;
  apiKey: string;
  baseUrl?: string;
  model?: string;
  target: "claude" | "codex";
}

export interface ConnectivityResult {
  ok: boolean;
  statusCode?: number | null;
  message: string;
  detail: string;
}

export interface ConfigApplyResult {
  ok: boolean;
  message: string;
  written: string[];
}


export interface Scheme {
  id: string;
  name: string;
  target: "claude" | "codex" | string;
  providerId: string;
  baseUrl: string;
  model?: string | null;
  apiKeyMasked: string;
  secretKey: string;
  updatedAt: string;
  lastVerifiedOk?: boolean | null;
}

export interface SchemeListResult {
  ok: boolean;
  activeClaude?: string | null;
  activeCodex?: string | null;
  schemes: Scheme[];
  message: string;
}

export interface HealthItem {
  id: string;
  title: string;
  level: "ok" | "warn" | "bad" | "info" | string;
  message: string;
  detail?: string | null;
  fixable: boolean;
  fixId?: string | null;
}

export interface HealthReport {
  ok: boolean;
  summary: string;
  checkedAt: string;
  items: HealthItem[];
  autoFixable: string[];
}

export interface HealthFixResult {
  ok: boolean;
  message: string;
  fixed: string[];
  skipped: string[];
}

export interface FirstProjectResult {
  ok: boolean;
  path: string;
  readmePath?: string | null;
  tool: string;
  toolAvailable: boolean;
  prompts: string[];
  message: string;
  terminalOpened: boolean;
}

export interface ComponentVersion {
  id: string;
  name: string;
  installed: boolean;
  version?: string | null;
  path?: string | null;
  upgradable: boolean;
}

export interface VersionsReport {
  ok: boolean;
  checkedAt: string;
  components: ComponentVersion[];
  message: string;
}

export interface UpgradeResult {
  ok: boolean;
  message: string;
  details: string[];
}

export interface BackupItem {
  id: string;
  createdAt: string;
  note: string;
  files: string[];
  path: string;
}

export interface BackupListResult {
  ok: boolean;
  items: BackupItem[];
  message: string;
}

export interface BackupActionResult {
  ok: boolean;
  message: string;
  id?: string | null;
  written: string[];
}

export interface ExtensionItem {
  id: string;
  kind: string;
  name: string;
  description: string;
  risk: string;
  source: string;
  installed: boolean;
  ownedByTool: boolean;
  canUninstall: boolean;
  detail?: string | null;
}

export interface ExtensionListResult {
  ok: boolean;
  items: ExtensionItem[];
  message: string;
}

export interface ExtensionActionResult {
  ok: boolean;
  message: string;
  written: string[];
}
