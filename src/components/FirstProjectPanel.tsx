// SPDX-License-Identifier: MPL-2.0
import { useEffect, useState } from "react";
import { api } from "../lib/api";
import type { FirstProjectResult } from "../types";

export function FirstProjectPanel({
  open,
  onClose,
  appendLog,
}: {
  open: boolean;
  onClose: () => void;
  appendLog: (line: string) => void;
}) {
  const [mode, setMode] = useState<"create" | "existing">("create");
  const [name, setName] = useState("my-first-app");
  const [path, setPath] = useState("");
  const [goal, setGoal] = useState("");
  const [stack, setStack] = useState("");
  const [success, setSuccess] = useState("");
  const [tool, setTool] = useState<"claude" | "codex">("claude");
  const [writeReadme, setWriteReadme] = useState(true);
  const [busy, setBusy] = useState(false);
  const [result, setResult] = useState<FirstProjectResult | null>(null);
  const [msg, setMsg] = useState("");

  useEffect(() => {
    if (!open) return;
    setResult(null);
    setMsg("");
  }, [open]);

  useEffect(() => {
    if (!open) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape" && !busy) onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [open, busy, onClose]);

  if (!open) return null;

  async function run() {
    setBusy(true);
    setMsg("");
    setResult(null);
    try {
      const r = await api.prepareFirstProject({
        mode,
        name: mode === "create" ? name.trim() : undefined,
        path: mode === "existing" ? path.trim() : undefined,
        goal: goal.trim(),
        stack: stack.trim() || undefined,
        success: success.trim() || undefined,
        tool,
        writeReadme,
      });
      setResult(r);
      appendLog(`[项目] ${r.message}`);
      setMsg(r.message);
    } catch (e) {
      const t = e instanceof Error ? e.message : String(e);
      setMsg(t);
      appendLog(`[项目失败] ${t}`);
    } finally {
      setBusy(false);
    }
  }

  async function copyPrompt(p: string) {
    try {
      await navigator.clipboard.writeText(p);
      appendLog("首轮提示已复制");
      setMsg("已复制到剪贴板，粘贴到终端对话即可");
    } catch {
      setMsg("复制失败，请手动选中复制");
    }
  }

  return (
    <div
      className="sheet-mask"
      onClick={(e) => {
        if (e.target === e.currentTarget && !busy) onClose();
      }}
    >
      <div className="sheet" role="dialog" aria-modal="true" aria-label="开始第一个项目">
        <div className="sheet-titlebar">
          <div className="sheet-title">开始第一个项目</div>
          <div className="sheet-sub">建文件夹 · 写 README · 开终端启动 CLI</div>
        </div>

        <div className="sheet-body space-y-3">
          <div>
            <div className="field-label">方式</div>
            <div className="seg">
              <button
                type="button"
                className={mode === "create" ? "on" : ""}
                aria-pressed={mode === "create"}
                onClick={() => setMode("create")}
              >
                新建项目
              </button>
              <button
                type="button"
                className={mode === "existing" ? "on" : ""}
                aria-pressed={mode === "existing"}
                onClick={() => setMode("existing")}
              >
                已有文件夹
              </button>
            </div>
          </div>

          {mode === "create" ? (
            <div>
              <label className="field-label" htmlFor="project-name">项目名</label>
              <input
                id="project-name"
                className="field"
                value={name}
                onChange={(e) => setName(e.target.value)}
                placeholder="my-first-app"
              />
              <p className="hint" style={{ marginTop: 4 }}>
                将创建在 Desktop/CodeCLI-Projects/ 下
              </p>
            </div>
          ) : (
            <div>
              <label className="field-label" htmlFor="project-path">项目绝对路径</label>
              <div style={{ display: "flex", gap: 8, alignItems: "center" }}>
                <input
                  id="project-path"
                  className="field mono"
                  style={{ flex: 1, minWidth: 0 }}
                  value={path}
                  onChange={(e) => setPath(e.target.value)}
                  placeholder="/Users/你/项目"
                />
                <button
                  type="button"
                  className="btn btn-default"
                  disabled={busy}
                  onClick={() => {
                    setMsg("");
                    void api
                      .pickProjectDirectory()
                      .then((selected) => {
                        if (selected) setPath(selected);
                      })
                      .catch((e) => setMsg(`选择目录失败: ${String(e)}`));
                  }}
                >
                  选择…
                </button>
              </div>
              <p className="hint" style={{ marginTop: 4 }}>
                粘贴访达路径；默认建议根：{path || "Desktop/CodeCLI-Projects"}
              </p>
            </div>
          )}

          <div>
            <label className="field-label" htmlFor="project-goal">项目目标（必填）</label>
            <input
              id="project-goal"
              className="field"
              value={goal}
              onChange={(e) => setGoal(e.target.value)}
              placeholder="例：做一个待办清单网页，可增删完成项"
            />
          </div>

          <div>
            <label className="field-label" htmlFor="project-stack">技术栈 / 约束（可选）</label>
            <input
              id="project-stack"
              className="field"
              value={stack}
              onChange={(e) => setStack(e.target.value)}
              placeholder="例：纯 HTML 或 React + Tailwind"
            />
          </div>

          <div>
            <label className="field-label" htmlFor="project-success">成功标准（可选）</label>
            <input
              id="project-success"
              className="field"
              value={success}
              onChange={(e) => setSuccess(e.target.value)}
              placeholder="例：浏览器能打开，增删功能正常"
            />
          </div>

          <div>
            <div className="field-label">用哪个工具启动</div>
            <div className="seg">
              <button
                type="button"
                className={tool === "claude" ? "on" : ""}
                aria-pressed={tool === "claude"}
                onClick={() => setTool("claude")}
              >
                Claude Code
              </button>
              <button
                type="button"
                className={tool === "codex" ? "on" : ""}
                aria-pressed={tool === "codex"}
                onClick={() => setTool("codex")}
              >
                Codex CLI
              </button>
            </div>
          </div>

          <label className="check" style={{ borderTop: "none", paddingLeft: 0 }}>
            <input
              type="checkbox"
              checked={writeReadme}
              onChange={(e) => setWriteReadme(e.target.checked)}
            />
            生成 README.md（已存在则不覆盖）
          </label>

          {result ? (
            <div className="group">
              <div className="group-header">已就绪</div>
              <div className="group-row">
                <span className="group-row-label">路径</span>
                <span className="group-row-value mono">{result.path}</span>
              </div>
              <div className="group-row">
                <span className="group-row-label">终端</span>
                <span className={`group-row-value ${result.terminalOpened ? "ok" : ""}`}>
                  {result.terminalOpened ? "已打开" : "未自动打开"}
                </span>
              </div>
              <div className="group-note">首轮提示（点复制 → 粘贴到 CLI）：</div>
              {result.prompts.map((p, i) => (
                <div key={i} className="group-row" style={{ alignItems: "flex-start" }}>
                  <div className="hint" style={{ flex: 1 }}>
                    {i + 1}. {p}
                  </div>
                  <button
                    type="button"
                    className="btn btn-default"
                    onClick={() => void copyPrompt(p)}
                  >
                    复制
                  </button>
                </div>
              ))}
              <div style={{ display: "flex", gap: 8, padding: "8px 12px 12px" }}>
                <button
                  type="button"
                  className="btn btn-default"
                  onClick={() =>
                    void api
                      .openProjectFolder(result.path)
                      .then(appendLog)
                      .catch((e) => {
                        const text = `打开文件夹失败: ${String(e)}`;
                        setMsg(text);
                        appendLog(text);
                      })
                  }
                >
                  打开文件夹
                </button>
                <button
                  type="button"
                  className="btn btn-default"
                  onClick={() =>
                    void api
                      .openProjectTerminal(result.path, result.tool)
                      .then(appendLog)
                      .catch((e) => appendLog(String(e)))
                  }
                >
                  再开终端
                </button>
              </div>
            </div>
          ) : null}

          {msg ? (
            <pre className="group" style={{ margin: 0, padding: 10, whiteSpace: "pre-wrap", fontSize: 11 }}>
              {msg}
            </pre>
          ) : null}
        </div>

        <div className="sheet-foot">
          <button type="button" className="btn btn-default" onClick={onClose} disabled={busy}>
            关闭
          </button>
          <button
            type="button"
            className="btn btn-primary"
            disabled={
              busy ||
              !goal.trim() ||
              (mode === "create" ? !name.trim() : !path.trim())
            }
            onClick={() => void run()}
          >
            {busy ? "处理中…" : "创建并启动"}
          </button>
        </div>
      </div>
    </div>
  );
}
