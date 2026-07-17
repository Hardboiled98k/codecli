// SPDX-License-Identifier: MPL-2.0
import { useCallback, useEffect, useRef, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { invoke } from "@tauri-apps/api/core";
import { StatusCards } from "./components/StatusCards";
import { StepList } from "./components/StepList";
import { LogPanel } from "./components/LogPanel";
import { ApiModal } from "./components/ApiModal";
import { type MoreAction } from "./components/MoreMenu";
import { MorePage } from "./components/MorePage";
import { SchemesPanel } from "./components/SchemesPanel";
import { HealthPanel } from "./components/HealthPanel";
import { FirstProjectPanel } from "./components/FirstProjectPanel";
import { TemplatesPanel } from "./components/TemplatesPanel";
import { VersionsPanel } from "./components/VersionsPanel";
import { BackupPanel } from "./components/BackupPanel";
import {
  LegalCenterPanel,
  type LegalDocumentId,
} from "./components/LegalCenterPanel";
import { ExtensionsPanel } from "./components/ExtensionsPanel";
import { ChangelogPanel } from "./components/ChangelogPanel";
import { api } from "./lib/api";
import type {
  InstallProgressEvent,
  Provider,
  StepStatus,
  SystemReport,
} from "./types";
import packageJson from "../package.json";
import "./index.css";

const DEFAULT_STEPS: StepStatus[] = [
  { id: "probe", title: "系统检查", status: "pending", message: "" },
  { id: "node", title: "安装 Node.js", status: "pending", message: "" },
  { id: "claude", title: "安装 Claude Code", status: "pending", message: "" },
  { id: "codex_cli", title: "安装 Codex CLI", status: "pending", message: "" },
  { id: "codex_app", title: "安装 ChatGPT/Codex App", status: "pending", message: "" },
  { id: "connectivity", title: "验证 API 连通性", status: "pending", message: "" },
  { id: "config", title: "验证后写入配置", status: "pending", message: "" },
  { id: "done", title: "完成", status: "pending", message: "" },
];

/** 左下角品牌区 — 按需改文案 */
const BRAND = {
  title: "帮助与说明",
  lines: [
    "开源社区版 · MPL-2.0",
    "不含官方账号 / 模型额度，Key 自备",
    "安装与配置操作仅在当前设备执行",
  ],
  contactLabel: "社区支持：GitHub Issues · 825242058@qq.com",
  contactHref: "" as string,
};

type InstallPlanSnapshot = Readonly<{
  installClaude: boolean;
  installCodexCli: boolean;
  installCodexApp: boolean;
  preferMirror: boolean;
}>;

type SelectedInstallStepId = "claude" | "codex_cli" | "codex_app";

interface CodexAppStatus {
  available: boolean;
  installed: boolean;
  message: string;
  downloadUrl?: string | null;
}

interface InstallVerification {
  report: SystemReport;
  appStatus: CodexAppStatus | null;
  missing: Array<{ stepId: SelectedInstallStepId; label: string }>;
}

function selectedConfigTargets(
  plan: InstallPlanSnapshot,
): Array<"claude" | "codex"> {
  const targets: Array<"claude" | "codex"> = [];
  if (plan.installClaude) targets.push("claude");
  if (plan.installCodexCli) targets.push("codex");
  return targets;
}

function initialStepsForSelection(plan: {
  installClaude: boolean;
  installCodexCli: boolean;
  installCodexApp: boolean;
}): StepStatus[] {
  const hasCli = plan.installClaude || plan.installCodexCli;
  return DEFAULT_STEPS.map((step) => {
    if (step.id === "node" && !hasCli) {
      return { ...step, status: "skipped", message: "未选择 CLI，无需安装 Node.js" };
    }
    if (step.id === "claude" && !plan.installClaude) {
      return { ...step, status: "skipped", message: "未选择 Claude Code" };
    }
    if (step.id === "codex_cli" && !plan.installCodexCli) {
      return { ...step, status: "skipped", message: "未选择 Codex CLI" };
    }
    if (step.id === "codex_app" && !plan.installCodexApp) {
      return { ...step, status: "skipped", message: "未选择桌面 App" };
    }
    if ((step.id === "connectivity" || step.id === "config") && !hasCli) {
      return { ...step, status: "skipped", message: "未选择 CLI，无需配置 API" };
    }
    return { ...step };
  });
}

async function verifySelectedInstallations(
  plan: InstallPlanSnapshot,
): Promise<InstallVerification> {
  const report = await api.probeSystem();
  const missing: InstallVerification["missing"] = [];

  if (plan.installClaude && !report.claudeInstalled) {
    missing.push({ stepId: "claude", label: "Claude Code" });
  }
  if (plan.installCodexCli && !report.codexInstalled) {
    missing.push({ stepId: "codex_cli", label: "Codex CLI" });
  }

  let appStatus: CodexAppStatus | null = null;
  if (plan.installCodexApp) {
    appStatus = await invoke<CodexAppStatus>("codex_app_available");
    if (!appStatus.installed) {
      missing.push({ stepId: "codex_app", label: "ChatGPT/Codex 桌面 App" });
    }
  }

  return { report, appStatus, missing };
}

function nowHms() {
  return new Date().toLocaleTimeString("zh-CN", { hour12: false });
}

export default function App() {
  const [report, setReport] = useState<SystemReport | null>(null);
  const [probeState, setProbeState] = useState<"loading" | "ready" | "error">("loading");
  const [probeError, setProbeError] = useState("");
  const [providers, setProviders] = useState<Provider[]>([]);
  const [steps, setSteps] = useState<StepStatus[]>(DEFAULT_STEPS);
  const [logs, setLogs] = useState<string[]>([]);
  const [busy, setBusy] = useState(false);
  const [operation, setOperation] = useState<"install" | "uninstall" | "clear" | null>(null);
  const [modalOpen, setModalOpen] = useState(false);
  const [installClaude, setInstallClaude] = useState(true);
  const [installCodexCli, setInstallCodexCli] = useState(true);
  const [installCodexApp, setInstallCodexApp] = useState(true);
  const [schemesOpen, setSchemesOpen] = useState(false);
  const [healthOpen, setHealthOpen] = useState(false);
  const [firstProjectOpen, setFirstProjectOpen] = useState(false);
  const [templatesOpen, setTemplatesOpen] = useState(false);
  const [versionsOpen, setVersionsOpen] = useState(false);
  const [backupOpen, setBackupOpen] = useState(false);
  const [legalCenterOpen, setLegalCenterOpen] = useState(false);
  const [legalCenterInitial, setLegalCenterInitial] =
    useState<LegalDocumentId>("privacy");
  const [changelogOpen, setChangelogOpen] = useState(false);
  const [extensionsOpen, setExtensionsOpen] = useState(false);
  const [extensionsFilter, setExtensionsFilter] = useState<string>("all");
  const [moreOpen, setMoreOpen] = useState(false);
  const [moreToast, setMoreToast] = useState<string | null>(null);
  const [installAwaitingConfig, setInstallAwaitingConfig] = useState(false);
  const [pendingConfigTargets, setPendingConfigTargets] = useState<Array<"claude" | "codex">>([]);
  const [configuredConfigTargets, setConfiguredConfigTargets] = useState<Array<"claude" | "codex">>([]);
  const [installPlanSnapshot, setInstallPlanSnapshot] = useState<InstallPlanSnapshot | null>(null);
  const persistDiagnosticLogsRef = useRef(true);

  const appendLog = useCallback((line: string) => {
    const formatted = `[${nowHms()}] ${line}`;
    setLogs((prev) => {
      const next = [...prev, formatted];
      return next.length > 1500 ? next.slice(next.length - 1500) : next;
    });
    // 日志只落本机私有文件；失败不能反过来阻断安装主流程。
    if (persistDiagnosticLogsRef.current) {
      void api.appendDiagnosticLog(formatted).catch(() => undefined);
    }
  }, []);


  const refreshProbe = useCallback(async () => {
    setProbeState("loading");
    setProbeError("");
    try {
      const r = await api.probeSystem();
      setReport(r);
      setProbeState("ready");
      appendLog(
        `系统: ${r.os} | 网络=${r.networkOk ? "OK" : "NG"} | 磁盘=${r.diskFreeGb}GB | Node=${r.nodeVersion || "无"} | Claude=${r.claudeVersion || "无"} | Codex=${r.codexVersion || "无"}`,
      );
    } catch (e) {
      const message = e instanceof Error ? e.message : String(e);
      setReport(null);
      setProbeState("error");
      setProbeError(message);
      appendLog(`系统检查失败: ${message}`);
    }
  }, [appendLog]);

  useEffect(() => {
    let disposed = false;
    void refreshProbe();
    void api
      .listProviders()
      .then((items) => {
        if (!disposed) setProviders(items);
      })
      .catch((e) => {
        if (!disposed) appendLog(`加载 Provider 失败: ${String(e)}`);
      });
    let unlisten: (() => void) | undefined;
    const listenTask = listen<InstallProgressEvent>("install-progress", (ev) => {
      const p = ev.payload;
      setSteps((prev) =>
        prev.map((s) =>
          s.id === p.stepId ? { ...s, status: p.status, message: p.message } : s,
        ),
      );
      appendLog(`[${p.stepId}] ${p.message}`);
    })
      .then((fn) => {
        // React StrictMode 可能在 listen Promise 返回前已执行 cleanup。
        // 此时立即解绑，不把第一次 mount 的监听器泄漏到第二次。
        if (disposed) fn();
        else unlisten = fn;
      })
      .catch((e) => {
        if (!disposed) appendLog(`安装进度监听失败: ${String(e)}`);
      });
    return () => {
      disposed = true;
      unlisten?.();
      // 显式保留 Promise，使上面的 disposed 分支在异步返回时完成解绑。
      void listenTask;
    };
  }, [appendLog, refreshProbe]);

  function resetInstallFlowForSelection(next: {
    installClaude: boolean;
    installCodexCli: boolean;
    installCodexApp: boolean;
  }) {
    setInstallPlanSnapshot(null);
    setInstallAwaitingConfig(false);
    setPendingConfigTargets([]);
    setConfiguredConfigTargets([]);
    setSteps(initialStepsForSelection(next));
  }

  async function startInstall() {
    if (installAwaitingConfig && pendingConfigTargets.length > 0) {
      appendLog("安装已完成，请先完成剩余 API 配置与真实连通测试");
      setModalOpen(true);
      return;
    }
    persistDiagnosticLogsRef.current = true;
    await api.resumeDiagnosticLog().catch(() => undefined);
    const plan: InstallPlanSnapshot = Object.freeze({
      installClaude,
      installCodexCli,
      installCodexApp,
      // CLI tarball 仅从 registry.npmjs.org 下载并对内嵌
      // SHA-512 SRI 校验；不再向用户提供降级镜像开关。
      preferMirror: false,
    });
    setInstallPlanSnapshot(plan);
    setBusy(true);
    setOperation("install");
    setInstallAwaitingConfig(false);
    setPendingConfigTargets([]);
    setConfiguredConfigTargets([]);
    setSteps(initialStepsForSelection(plan));
    appendLog("开始安装…");
    try {
      const result = await api.runInstallPlan(plan);
      // 桌面 App 只能打开下载页；未检测到真实安装仍保持整体未完成，
      // 但后端已单独验收成功的 CLI 不应因此失去 API onboarding。
      const canContinueCliOnboarding =
        (plan.installClaude || plan.installCodexCli) &&
        (result.ok || result.failedStep === "codex_app");
      const needsCliOnboarding = canContinueCliOnboarding && selectedConfigTargets(plan).length > 0;
      const normalized = result.steps.map((step) => {
        // 后端本轮没收到 Key 时会把这两步标为 skipped；
        // 但 CLI 已安装后，前端正要继续 onboarding，必须显示为待完成。
        if (needsCliOnboarding && step.id === "connectivity") {
          return {
            ...step,
            status: "pending",
            message: "请先完成真实 API 连通测试",
          };
        }
        if (needsCliOnboarding && step.id === "config") {
          return {
            ...step,
            status: "pending",
            message: "连通测试通过后写入所选 CLI 配置",
          };
        }
        if (step.id === "done" && needsCliOnboarding && result.ok) {
          return {
            ...step,
            status: "pending",
            message: "CLI 已可用；完成 API 验证与写入后结束",
          };
        }
        return step;
      });
      setSteps(normalized);
      appendLog(result.ok ? "安装结束" : `失败${result.failedStep ? `: ${result.failedStep}` : ""}`);
      if (result.requiresRestart) appendLog("请关闭后重开本工具（PATH）");
      await refreshProbe();
      if (canContinueCliOnboarding) {
        const required = selectedConfigTargets(plan);
        setPendingConfigTargets(required);
        setInstallAwaitingConfig(required.length > 0);
        appendLog(
          result.ok
            ? "下一步：配置 API，并完成真实连通性验证"
            : "桌面 App 尚未安装；已验收成功的 CLI 可继续配置 API",
        );
        setModalOpen(true);
      }
    } catch (e) {
      const message = e instanceof Error ? e.message : String(e);
      setSteps((prev) => {
        const failedStep =
          prev.find((step) => step.status === "running") ??
          prev.find((step) => step.status === "pending") ??
          prev.find((step) => step.id === "done");
        if (!failedStep) return prev;
        return prev.map((step) =>
          step.id === failedStep.id
            ? {
                ...step,
                status: "failed",
                message: `安装流程被拒绝或中断：${message}`,
              }
            : step,
        );
      });
      appendLog(`安装失败: ${message}`);
    } finally {
      setBusy(false);
      setOperation(null);
    }
  }

  async function retryFailedStep(stepId: string) {
    if (busy) return;
    const plan = installPlanSnapshot;
    if (!plan) {
      appendLog("无法重试：未找到原始安装计划，请点击“开始安装”重新执行");
      return;
    }

    if (stepId === "config" || stepId === "connectivity") {
      const required = selectedConfigTargets(plan);
      if (required.length === 0) {
        appendLog("原始安装计划未选择 CLI，无需配置 API");
        return;
      }
      setPendingConfigTargets(required);
      setInstallAwaitingConfig(true);
      appendLog(`重试 ${stepId === "config" ? "API 配置" : "连通性验证"}：请在弹窗中测试并保存`);
      setModalOpen(true);
      return;
    }

    persistDiagnosticLogsRef.current = true;
    await api.resumeDiagnosticLog().catch(() => undefined);
    setBusy(true);
    setOperation("install");
    setSteps((prev) =>
      prev.map((step) =>
        step.id === stepId ? { ...step, status: "running", message: "正在单独重试此步骤…" } : step,
      ),
    );
    appendLog(`单步重试: ${stepId}`);

    try {
      let message = "";
      let status: StepStatus["status"] = "success";
      if (stepId === "probe") {
        const result = await api.probeSystem();
        setReport(result);
        setProbeState("ready");
        setProbeError("");
        if (!result.osSupported) {
          throw new Error(result.supportMessage || "系统版本仍不受支持");
        }
        const requiredDiskGb = plan.installClaude || plan.installCodexCli ? 2 : 1;
        if (result.diskKnown && result.diskFreeGb < requiredDiskGb) {
          throw new Error(`磁盘可用空间仍不足 ${requiredDiskGb}GB`);
        }
        if (
          (plan.installClaude || plan.installCodexCli) &&
          !result.network.npmOfficialOk
        ) {
          throw new Error(
            "官方 npm registry.npmjs.org 仍不可达；为保持 SRI 身份闭环，不会降级到镜像",
          );
        }
        message = "系统、网络与磁盘预检已通过；可点击“开始安装”继续后续步骤";
      } else if (stepId === "node") {
        const result = await api.ensureNode(plan.installClaude ? 22 : 18);
        if (result.requiresRestart) {
          throw new Error(`${result.message || "Node.js 已安装"}；PATH 尚未生效，请关闭并重开安装器后继续`);
        }
        if (!result.ok) throw new Error(result.message || "Node.js 重试失败");
        status = result.skipped ? "skipped" : "success";
        message = `${result.message}；可点击“开始安装”继续后续步骤`;
      } else if (stepId === "claude") {
        const result = await api.installClaude(plan.preferMirror);
        if (!result.ok) throw new Error(result.message || "Claude Code 重试失败");
        status = result.skipped ? "skipped" : "success";
        message = result.message;
      } else if (stepId === "codex_cli") {
        const result = await api.installCodexCli(plan.preferMirror);
        if (!result.ok) throw new Error(result.message || "Codex CLI 重试失败");
        status = result.skipped ? "skipped" : "success";
        message = result.message;
      } else if (stepId === "codex_app") {
        const result = await api.installCodexApp();
        if (!result.ok || result.status === "action_required") {
          throw new Error(result.message || "桌面 App 尚未安装");
        }
        status = result.skipped ? "skipped" : "success";
        message = result.message;
      } else if (stepId === "done") {
        status = "running";
        message = "正在验收原始安装计划中的全部安装项";
      } else {
        throw new Error(`不支持重试步骤: ${stepId}`);
      }

      setSteps((prev) =>
        prev.map((step) =>
          step.id === stepId ? { ...step, status, message } : step,
        ),
      );
      if (stepId !== "done") appendLog(`单步重试成功: ${message}`);

      const aggregatesInstallState =
        stepId === "claude" ||
        stepId === "codex_cli" ||
        stepId === "codex_app" ||
        stepId === "done";
      if (aggregatesInstallState) {
        const verification = await verifySelectedInstallations(plan);
        setReport(verification.report);
        const missingIds = new Set(verification.missing.map((item) => item.stepId));
        if (verification.missing.length > 0) {
          const missingNames = verification.missing.map((item) => item.label).join("、");
          const failureMessage = `原始安装计划终验未通过：${missingNames} 尚不可用`;
          setSteps((prev) =>
            prev.map((step) => {
              if (missingIds.has(step.id as SelectedInstallStepId)) {
                return { ...step, status: "failed", message: `${step.title} 尚未检测为已安装` };
              }
              if (step.id === "done") {
                return { ...step, status: "failed", message: failureMessage };
              }
              return step;
            }),
          );
          appendLog(failureMessage);
          return;
        }

        const requiredTargets = selectedConfigTargets(plan);
        const connectivityVerified = steps.some(
          (step) => step.id === "connectivity" && step.status === "success",
        );
        const configComplete =
          requiredTargets.length === 0 ||
          (connectivityVerified && !installAwaitingConfig && pendingConfigTargets.length === 0);

        setSteps((prev) =>
          prev.map((step) => {
            if (
              (step.id === "claude" && plan.installClaude) ||
              (step.id === "codex_cli" && plan.installCodexCli) ||
              (step.id === "codex_app" && plan.installCodexApp)
            ) {
              return {
                ...step,
                status: "success",
                message:
                  step.id === "codex_app"
                    ? verification.appStatus?.message || "已检测到 ChatGPT/Codex 桌面 App"
                    : `${step.title} 已重新探测并验收`,
              };
            }
            if (step.id === "done") {
              return configComplete
                ? { ...step, status: "success", message: "原始安装计划、配置与连通性均已验收" }
                : {
                    ...step,
                    status: "pending",
                    message: "安装项已验收；完成 API 配置与真实连通测试后结束",
                  };
            }
            if (!configComplete && step.id === "config" && step.status !== "success") {
              return { ...step, status: "pending", message: "请完成所选 CLI 的 API 配置" };
            }
            if (!configComplete && step.id === "connectivity") {
              return { ...step, status: "pending", message: "请完成真实 API 连通测试" };
            }
            return step;
          }),
        );

        if (!configComplete) {
          const stillPending = pendingConfigTargets.filter((target) =>
            requiredTargets.includes(target),
          );
          const nextTargets = stillPending.length > 0 ? stillPending : requiredTargets;
          setPendingConfigTargets(nextTargets);
          setInstallAwaitingConfig(true);
          setModalOpen(true);
          appendLog("原始计划中的全部安装项已就绪，继续配置 API 与真实连通测试");
        } else {
          appendLog("原始安装计划中的全部安装项、配置与连通性均已验收");
        }
      }
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      setSteps((prev) =>
        prev.map((step) => {
          if (step.id === stepId) return { ...step, status: "failed", message };
          if (
            step.id === "done" &&
            (stepId === "claude" || stepId === "codex_cli" || stepId === "codex_app")
          ) {
            return {
              ...step,
              status: "failed",
              message: "原始安装计划仍有安装项未通过，请先重试失败步骤",
            };
          }
          return step;
        }),
      );
      appendLog(`单步重试失败: ${message}`);
    } finally {
      setBusy(false);
      setOperation(null);
    }
  }

  async function cancelInstall() {
    try {
      await api.cancelInstall();
      appendLog("已请求取消（进行中的下载/安装将尽快停止）…");
    } catch (e) {
      appendLog(`取消失败: ${String(e)}`);
    }
  }

  async function copyLogs() {
    try {
      await navigator.clipboard.writeText(logs.join("\n"));
      appendLog("日志已复制");
    } catch {
      appendLog("复制失败");
    }
  }

  async function exportLogs() {
    try {
      const path = await api.exportDiagnosticLog(logs);
      appendLog(`诊断日志已导出: ${path}`);
      showMoreToast(`日志已导出：${path}`);
    } catch (e) {
      const message = e instanceof Error ? e.message : String(e);
      appendLog(`导出日志失败: ${message}`);
      showMoreToast(`导出失败：${message}`);
    }
  }

  async function fullUninstall(): Promise<boolean> {
    if (
      !confirm(
        "完整卸载将：\n\n· 卸载本工具有精确归属记录的 Claude Code / Codex CLI\n· 恢复或清除本工具管理的 Claude / Codex 配置\n· 删除本工具安装的扩展和运行时路径\n· 永久删除本工具保存的方案、密钥、备份、日志与应用本地状态\n\n备份也会被删除，之后无法从本工具恢复。\n不会删除用户自行安装的 CLI 或非本工具管理的配置。\n\n确定继续？",
      )
    ) {
      return false;
    }
    setBusy(true);
    setOperation("uninstall");
    let purgeSucceeded = false;
    try {
      const cliFailures: string[] = [];
      const cliCompleted: string[] = [];
      try {
        const c = await api.uninstallClaude();
        appendLog(c.message || "Claude 卸载完成");
        if (!c.ok) cliFailures.push(`Claude: ${c.message || "卸载失败"}`);
        else cliCompleted.push(c.skipped ? "Claude：无本工具可卸载项" : "Claude：已卸载");
      } catch (e) {
        const message = e instanceof Error ? e.message : String(e);
        cliFailures.push(`Claude: ${message}`);
        appendLog(`Claude 卸载失败: ${message}`);
      }
      try {
        const x = await api.uninstallCodex();
        appendLog(x.message || "Codex 卸载完成");
        if (!x.ok) cliFailures.push(`Codex: ${x.message || "卸载失败"}`);
        else cliCompleted.push(x.skipped ? "Codex：无本工具可卸载项" : "Codex：已卸载");
      } catch (e) {
        const message = e instanceof Error ? e.message : String(e);
        cliFailures.push(`Codex: ${message}`);
        appendLog(`Codex 卸载失败: ${message}`);
      }
      if (cliFailures.length > 0) {
        const completed = cliCompleted.length > 0
          ? `已完成：${cliCompleted.join("；")}。`
          : "";
        const summary = `完整卸载${cliCompleted.length > 0 ? "仅部分完成" : "未完成"}。${completed}失败：${cliFailures.join(" | ")}。尚未执行配置与本地数据清理；成功项已实际处理，失败项的归属记录保留，修复后可重试。`;
        appendLog(summary);
        window.alert(summary);
        await refreshProbe();
        return false;
      }
      try {
        persistDiagnosticLogsRef.current = false;
        const cfg = await api.purgeToolData();
        if (!cfg.ok) throw new Error(cfg.message || "配置与本地数据清理未完成");
        purgeSucceeded = true;
        appendLog(cfg.message || "本工具配置与本地数据已清除");
      } catch (e) {
        persistDiagnosticLogsRef.current = true;
        const message = e instanceof Error ? e.message : String(e);
        const summary = `CLI 处理已完成，但配置与本地数据清理失败：${message}。已卸载的 CLI 不会自动恢复；请修复后重试完整卸载。`;
        appendLog(summary);
        window.alert(summary);
      }
      await refreshProbe();
      if (purgeSucceeded) {
        setInstallAwaitingConfig(false);
        setPendingConfigTargets([]);
        setConfiguredConfigTargets([]);
        setInstallPlanSnapshot(null);
        setSteps(initialStepsForSelection({ installClaude, installCodexCli, installCodexApp }));
      }
    } finally {
      // 成功 purge 后保持暂停，避免收尾日志立即重建已删目录。
      // 后续重新安装时会显式 resume；失败时则继续记录可重试日志。
      if (!purgeSucceeded) persistDiagnosticLogsRef.current = true;
      setBusy(false);
      setOperation(null);
    }
    return purgeSucceeded;
  }

  async function clearCfg(): Promise<boolean> {
    if (!confirm("清除本工具写入的配置，并尽量恢复改前环境变量？")) return false;
    setBusy(true);
    setOperation("clear");
    try {
      const r = await api.clearConfig("both");
      appendLog(r.message);
      if (!r.ok) throw new Error(r.message || "清除配置未成功");
      const required = installPlanSnapshot ? selectedConfigTargets(installPlanSnapshot) : [];
      setConfiguredConfigTargets([]);
      setPendingConfigTargets(required);
      setInstallAwaitingConfig(required.length > 0);
      setSteps((prev) =>
        prev.map((step) => {
          if (step.id === "connectivity") {
            return {
              ...step,
              status: required.length > 0 ? "pending" : step.status,
              message: required.length > 0
                ? "配置已清除，需重新验证真实 API 连通性"
                : "配置已清除；下次配置时需重新验证",
            };
          }
          if (step.id === "config") {
            return {
              ...step,
              status: required.length > 0 ? "pending" : step.status,
              message: required.length > 0
                ? "配置已清除，请验证通过后重新写入"
                : "本工具写入的配置已清除",
            };
          }
          if (step.id === "done" && required.length > 0) {
            return {
              ...step,
              status: "pending",
              message: "配置已清除；重新完成 API 验证与写入后才算完成",
            };
          }
          return step;
        }),
      );
      if (required.length > 0) appendLog("安装 onboarding 已重置，需重新配置并验证所选 CLI");
      return true;
    } catch (e) {
      appendLog(`清除失败: ${String(e)}`);
      return false;
    } finally {
      setBusy(false);
      setOperation(null);
    }
  }

  function showMoreToast(text: string) {
    setMoreToast(text);
    window.setTimeout(() => {
      setMoreToast((cur) => (cur === text ? null : cur));
    }, 3200);
  }

  function handleMore(action: MoreAction) {
    // 功能弹层叠在「更多」之上；不关闭更多页，方便连续点其它功能
    // 无弹窗的动作必须用 moreToast 可见反馈（日志在更多页下面看不见）
    switch (action) {
      case "schemes":
        setSchemesOpen(true);
        break;
      case "health":
        setHealthOpen(true);
        break;
      case "first_project":
        setFirstProjectOpen(true);
        break;
      case "templates":
        setTemplatesOpen(true);
        break;
      case "versions":
        setVersionsOpen(true);
        break;
      case "backup":
        setBackupOpen(true);
        break;
      case "skills":
        setExtensionsFilter("skill");
        setExtensionsOpen(true);
        break;
      case "mcp":
        setExtensionsFilter("mcp");
        setExtensionsOpen(true);
        break;
      case "feishu":
        setExtensionsFilter("cli");
        setExtensionsOpen(true);
        break;
      case "privacy":
        setLegalCenterInitial("privacy");
        setLegalCenterOpen(true);
        break;
      case "support_logs":
        void exportLogs();
        break;
      case "about":
        setChangelogOpen(true);
        break;
      default:
        showMoreToast("未知操作");
        appendLog("未知操作");
    }
  }

  return (
    <div className="app">
      <div className="toolbar">
        <div>
          <div className="toolbar-title">安装与配置</div>
          <div className="toolbar-sub">Claude Code · Codex · v{packageJson.version}</div>
        </div>
        <div style={{ display: "flex", gap: 8, alignItems: "center" }}>
          <button
            type="button"
            className="btn btn-default"
            disabled={busy && !moreOpen}
            onClick={() => setMoreOpen((v) => !v)}
          >
            {moreOpen ? "返回" : "更多"}
          </button>
          <button
            type="button"
            className="btn btn-default"
            disabled={busy}
            onClick={() => void refreshProbe()}
          >
            刷新
          </button>
          <button
            type="button"
            className="btn btn-default"
            disabled={busy}
            onClick={() => setModalOpen(true)}
          >
            配置 API…
          </button>
          {operation === "install" ? (
            <button type="button" className="btn btn-default" onClick={() => void cancelInstall()}>
              取消
            </button>
          ) : null}
          <button
            type="button"
            className="btn btn-primary"
            disabled={busy}
            onClick={() => void startInstall()}
          >
            {busy ? "进行中…" : installAwaitingConfig ? "继续配置 API" : "开始安装"}
          </button>
        </div>
      </div>

      <div className="body">
        <aside className="sidebar">
          <div className="sidebar-steps">
            <StepList
              steps={steps}
              onRetry={(stepId) => void retryFailedStep(stepId)}
              retryDisabled={busy}
            />
          </div>
          <div className="sidebar-brand">
            <div className="brand-title">{BRAND.title}</div>
            <p className="brand-line">● 开源社区版</p>
            {BRAND.lines.map((line) => (
              <p key={line} className="brand-line">
                {line}
              </p>
            ))}
            {BRAND.contactHref ? (
              <p className="brand-line">
                <a href={BRAND.contactHref} target="_blank" rel="noreferrer">
                  {BRAND.contactLabel}
                </a>
              </p>
            ) : (
              <p className="brand-line">{BRAND.contactLabel}</p>
            )}
          </div>
        </aside>

        <section className="main">
          <div className="main-scroll">
            <div className="group">
              <div className="group-header">开源社区版</div>
              <div className="row">
                <span>许可</span>
                <span className="ok">MPL-2.0 · 开源社区版</span>
              </div>
              <p className="group-note muted">
                所有核心本地功能均可直接使用，默认不收集遥测数据。
              </p>
            </div>

            <StatusCards report={report} state={probeState} error={probeError} />

            <div className="group">
              <div className="group-header">安装选项</div>
              <label className="check">
                <input
                  type="checkbox"
                  checked={installClaude}
                  disabled={busy}
                  onChange={(e) => {
                    const checked = e.target.checked;
                    setInstallClaude(checked);
                    resetInstallFlowForSelection({
                      installClaude: checked,
                      installCodexCli,
                      installCodexApp,
                    });
                  }}
                />
                Claude Code
              </label>
              <label className="check">
                <input
                  type="checkbox"
                  checked={installCodexCli}
                  disabled={busy}
                  onChange={(e) => {
                    const checked = e.target.checked;
                    setInstallCodexCli(checked);
                    resetInstallFlowForSelection({
                      installClaude,
                      installCodexCli: checked,
                      installCodexApp,
                    });
                  }}
                />
                Codex CLI
              </label>
              <label className="check">
                <input
                  type="checkbox"
                  checked={installCodexApp}
                  disabled={busy}
                  onChange={(e) => {
                    const checked = e.target.checked;
                    setInstallCodexApp(checked);
                    resetInstallFlowForSelection({
                      installClaude,
                      installCodexCli,
                      installCodexApp: checked,
                    });
                  }}
                />
                ChatGPT/Codex App（打开官方下载页并验收）
              </label>
              <div className="group-note">
                安装包固定从官方 npm 下载并校验 SHA-512，不降级使用第三方镜像。
                仅本机安装与配置，不含账号/额度。Key 自备。失败可复制日志。
              </div>
            </div>


            <LogPanel lines={logs} />
          </div>

          <div className="footer-bar">
            {operation === "install" ? (
              <button type="button" className="btn btn-default" onClick={() => void cancelInstall()}>
                取消
              </button>
            ) : null}
            <button
              type="button"
              className="btn btn-primary"
              disabled={busy}
              onClick={() => void startInstall()}
            >
              {busy ? "进行中…" : installAwaitingConfig ? "继续配置 API" : "开始安装"}
            </button>
            <button
              type="button"
              className="btn btn-default"
              disabled={busy}
              onClick={() => setModalOpen(true)}
            >
              配置 API…
            </button>
            <button type="button" className="btn btn-default" onClick={() => void copyLogs()}>
              复制日志
            </button>
            <button type="button" className="btn btn-default" onClick={() => void exportLogs()}>
              导出日志
            </button>
            <button
              type="button"
              className="btn btn-default"
              disabled={busy}
              onClick={() => void refreshProbe()}
              title="重新检测系统、网络和已安装版本"
            >
              重新检测环境
            </button>
            <span style={{ flex: 1 }} />
            <button
              type="button"
              className="btn btn-text"
              disabled={busy}
              onClick={() => void clearCfg()}
            >
              清除配置
            </button>
            <button
              type="button"
              className="btn btn-danger-text"
              disabled={busy}
              onClick={() => void fullUninstall()}
            >
              完整卸载
            </button>
          </div>
        </section>
      </div>

      <ApiModal
        open={modalOpen}
        onClose={() => setModalOpen(false)}
        providers={providers}
        preferredTarget={pendingConfigTargets[0] ?? null}
        onSaved={async ({ target, tested }) => {
          if (installAwaitingConfig) {
            const plan = installPlanSnapshot;
            if (!plan) {
              appendLog("API 已保存，但找不到对应的原始安装计划；请重新开始安装后再做终验");
              await refreshProbe();
              return;
            }
            const remaining = tested
              ? pendingConfigTargets.filter((item) => item !== target)
              : pendingConfigTargets;
            const remainingNames = remaining.map((item) =>
              item === "claude" ? "Claude" : "Codex",
            );
            const requiredTargets = selectedConfigTargets(plan);
            const configured = requiredTargets.filter(
              (item) => item === target || configuredConfigTargets.includes(item),
            );
            const unconfigured = requiredTargets.filter((item) => !configured.includes(item));
            const unconfiguredNames = unconfigured.map((item) =>
              item === "claude" ? "Claude" : "Codex",
            );
            let verification: InstallVerification | null = null;
            let verificationError = "";
            if (tested && remaining.length === 0) {
              try {
                verification = await verifySelectedInstallations(plan);
                setReport(verification.report);
              } catch (error) {
                verificationError = error instanceof Error ? error.message : String(error);
                appendLog(`安装项终验失败: ${verificationError}`);
              }
            }

            const missingIds = new Set(
              verification?.missing.map((item) => item.stepId) ?? [],
            );
            const missingNames = verification?.missing.map((item) => item.label).join("、") ?? "";
            setPendingConfigTargets(remaining);
            setConfiguredConfigTargets(configured);
            if (tested && remaining.length === 0) setInstallAwaitingConfig(false);
            setSteps((prev) =>
              prev.map((step) => {
                if (verification && missingIds.has(step.id as SelectedInstallStepId)) {
                  return { ...step, status: "failed", message: `${step.title} 尚未检测为已安装` };
                }
                if (
                  verification &&
                  ((step.id === "claude" && plan.installClaude) ||
                    (step.id === "codex_cli" && plan.installCodexCli) ||
                    (step.id === "codex_app" && plan.installCodexApp))
                ) {
                  return {
                    ...step,
                    status: "success",
                    message:
                      step.id === "codex_app"
                        ? verification.appStatus?.message || "已检测到 ChatGPT/Codex 桌面 App"
                        : `${step.title} 已重新探测并验收`,
                  };
                }
                if (step.id === "config") {
                  return {
                    ...step,
                    status: unconfigured.length === 0 ? "success" : "pending",
                    message:
                      unconfigured.length > 0
                        ? `${target === "claude" ? "Claude" : "Codex"} 已写入；仍需写入：${unconfiguredNames.join("、")}`
                        : tested && remaining.length === 0
                          ? "所选 CLI 均已在验证通过后写入配置"
                          : "所选 CLI 配置均已写入，但仍有连通测试未通过",
                  };
                }
                if (step.id === "connectivity") {
                  return {
                    ...step,
                    status: tested && remaining.length === 0 ? "success" : "pending",
                    message: !tested
                      ? "已强制保存，尚未验证连通性"
                      : remaining.length > 0
                        ? `已验证当前配置；仍需配置并验证：${remainingNames.join("、")}`
                        : "所选 CLI 的真实 API 连通测试均通过",
                  };
                }
                if (step.id === "done") {
                  if (!tested || remaining.length > 0) {
                    if (step.status === "failed") {
                      return {
                        ...step,
                        status: "failed",
                        message: !tested
                          ? "配置已强制保存但尚未通过连通测试；原始安装计划仍未完成终验"
                          : `仍需配置并验证：${remainingNames.join("、")}`,
                      };
                    }
                    return {
                      ...step,
                      status: "pending",
                      message: !tested
                        ? "配置已强制保存但尚未通过连通测试"
                        : `仍需配置并验证：${remainingNames.join("、")}`,
                    };
                  }
                  if (verificationError) {
                    return {
                      ...step,
                      status: "failed",
                      message: `API 连通测试已通过，但安装项终验失败：${verificationError}`,
                    };
                  }
                  if (!verification || verification.missing.length > 0) {
                    return {
                      ...step,
                      status: "failed",
                      message: missingNames
                        ? `API 连通测试已通过，但原始计划仍缺少：${missingNames}`
                        : "API 连通测试已通过，但安装项终验未完成",
                    };
                  }
                  return {
                    ...step,
                    status: "success",
                    message: "原始安装计划、配置与连通性均已验收",
                  };
                }
                return step;
              }),
            );
            if (tested && remaining.length > 0) {
              appendLog(`仍需配置并验证: ${remainingNames.join("、")}`);
              // 当两个 CLI 都被选中时，自动引导到下一个目标，
              // 避免用户以为只配置一个就已经完成。
              window.setTimeout(() => setModalOpen(true), 100);
            } else if (tested && remaining.length === 0 && missingNames) {
              appendLog(`API 连通测试已通过，但原始计划仍缺少：${missingNames}`);
            }
          }
          await refreshProbe();
        }}
        appendLog={appendLog}
      />
      <MorePage
        open={moreOpen}
        onAction={handleMore}
        toast={moreToast}
      />
      <SchemesPanel
        open={schemesOpen}
        onClose={() => setSchemesOpen(false)}
        appendLog={appendLog}
        onOpenConfig={() => setModalOpen(true)}
      />
      <HealthPanel
        open={healthOpen}
        onClose={() => setHealthOpen(false)}
        appendLog={appendLog}
      />
      <FirstProjectPanel
        open={firstProjectOpen}
        onClose={() => setFirstProjectOpen(false)}
        appendLog={appendLog}
      />
      <TemplatesPanel
        open={templatesOpen}
        onClose={() => setTemplatesOpen(false)}
        appendLog={appendLog}
      />
      <VersionsPanel
        open={versionsOpen}
        onClose={() => setVersionsOpen(false)}
        appendLog={appendLog}
      />
      <BackupPanel
        open={backupOpen}
        onClose={() => setBackupOpen(false)}
        appendLog={appendLog}
      />
      <LegalCenterPanel
        open={legalCenterOpen}
        initialDocument={legalCenterInitial}
        onClose={() => setLegalCenterOpen(false)}
      />
      <ChangelogPanel open={changelogOpen} onClose={() => setChangelogOpen(false)} />
      <ExtensionsPanel
        open={extensionsOpen}
        onClose={() => setExtensionsOpen(false)}
        appendLog={appendLog}
        filterKind={extensionsFilter}
      />
    </div>
  );
}
