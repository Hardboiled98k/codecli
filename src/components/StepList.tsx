// SPDX-License-Identifier: MPL-2.0
import type { StepStatus } from "../types";

const ORDER = [
  { id: "probe", title: "系统检查" },
  { id: "node", title: "安装 Node.js" },
  { id: "claude", title: "安装 Claude Code" },
  { id: "codex_cli", title: "安装 Codex CLI" },
  { id: "codex_app", title: "安装 ChatGPT/Codex App" },
  { id: "connectivity", title: "验证 API 连通性" },
  { id: "config", title: "验证后写入配置" },
  { id: "done", title: "完成" },
];

function mark(status: string) {
  switch (status) {
    case "success":
      return { cls: "success", text: "✓" };
    case "skipped":
      return { cls: "skipped", text: "–" };
    case "running":
      return { cls: "running", text: "…" };
    case "failed":
      return { cls: "failed", text: "!" };
    default:
      return { cls: "pending", text: "" };
  }
}

export function StepList({
  steps,
  onRetry,
  retryDisabled = false,
}: {
  steps: StepStatus[];
  onRetry?: (stepId: string) => void;
  retryDisabled?: boolean;
}) {
  const map = new Map(steps.map((s) => [s.id, s]));

  return (
    <div>
      <div className="group-header" style={{ paddingLeft: 6, paddingBottom: 6 }}>
        步骤
      </div>
      {ORDER.map((o) => {
        const s = map.get(o.id);
        const status = s?.status || "pending";
        const m = mark(status);
        return (
          <div key={o.id} className={`step ${status === "running" ? "active" : ""}`}>
            <span className={`step-dot ${m.cls}`}>{m.text}</span>
            <div style={{ minWidth: 0, flex: 1 }}>
              <div className={`step-title ${status === "pending" ? "muted" : ""}`}>{o.title}</div>
              {s?.message ? <div className="step-msg">{s.message}</div> : null}
            </div>
            {status === "failed" && onRetry ? (
              <button
                type="button"
                className="btn btn-text"
                style={{ height: 24, padding: "0 6px", flexShrink: 0 }}
                disabled={retryDisabled}
                onClick={() => onRetry(o.id)}
                aria-label={`重试${o.title}`}
              >
                重试此步
              </button>
            ) : null}
          </div>
        );
      })}
    </div>
  );
}
