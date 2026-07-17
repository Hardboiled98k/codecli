// SPDX-License-Identifier: MPL-2.0
import type { SystemReport } from "../types";

export function StatusCards({
  report,
  state,
  error,
}: {
  report: SystemReport | null;
  state: "loading" | "ready" | "error";
  error?: string;
}) {
  const rows = report
    ? [
        {
          label: "系统",
          value: `${report.os}${report.osVersion ? " " + report.osVersion : ""} · ${report.arch}`,
          tone: report.osSupported ? ("default" as const) : ("bad" as const),
        },
        {
          label: "兼容",
          value: report.supportMessage,
          tone: report.osSupported ? ("ok" as const) : ("bad" as const),
        },
        {
          label: "网络",
          value: report.networkOk
            ? report.network?.detail || "正常"
            : report.network?.detail || "异常",
          tone: report.networkOk ? ("ok" as const) : ("bad" as const),
        },
        {
          label: "磁盘",
          value: report.diskKnown
            ? `${report.diskFreeGb.toFixed(1)} GB 可用`
            : "未能检测",
          tone: !report.diskKnown
            ? ("default" as const)
            : report.diskFreeGb >= 1
              ? ("ok" as const)
              : ("bad" as const),
        },
        {
          label: "Node.js",
          value: report.nodeInstalled ? report.nodeVersion || "已安装" : "未安装",
          tone: report.nodeInstalled ? ("ok" as const) : ("default" as const),
        },
        {
          label: "Claude",
          value: report.claudeInstalled ? report.claudeVersion || "已安装" : "未安装",
          tone: "default" as const,
        },
        {
          label: "Codex",
          value: report.codexInstalled ? report.codexVersion || "已安装" : "未安装",
          tone: "default" as const,
        },
      ]
    : state === "error"
      ? [
          { label: "系统", value: "检测失败", tone: "bad" as const },
          {
            label: "原因",
            value: error?.trim() || "请点右上角“刷新”重试",
            tone: "bad" as const,
          },
        ]
      : [
        { label: "系统", value: "检测中…", tone: "default" as const },
        { label: "网络", value: "检测中…", tone: "default" as const },
        { label: "磁盘", value: "检测中…", tone: "default" as const },
        { label: "Node.js", value: "检测中…", tone: "default" as const },
        ];

  return (
    <div className="group">
      <div className="group-header">环境状态</div>
      {rows.map((r) => (
        <div key={r.label} className="group-row">
          <span className="group-row-label">{r.label}</span>
          <span className={`group-row-value ${r.tone}`}>{r.value}</span>
        </div>
      ))}
    </div>
  );
}
