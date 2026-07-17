// SPDX-License-Identifier: MPL-2.0
import { useCallback, useEffect, useState } from "react";
import { api } from "../lib/api";
import type { HealthReport } from "../types";

function formatCheckedAt(raw?: string | null) {
  if (!raw) return "—";
  const n = Number(raw);
  if (Number.isFinite(n) && n > 1e9) {
    try {
      return new Date(n * 1000).toLocaleString("zh-CN", { hour12: false });
    } catch {
      return raw;
    }
  }
  return raw;
}

export function HealthPanel({
  open,
  onClose,
  appendLog,
}: {
  open: boolean;
  onClose: () => void;
  appendLog: (line: string) => void;
}) {
  const [report, setReport] = useState<HealthReport | null>(null);
  const [busy, setBusy] = useState(false);
  const [msg, setMsg] = useState("");

  const runCheck = useCallback(async () => {
    setBusy(true);
    setMsg("");
    try {
      const r = await api.healthCheck();
      setReport(r);
      appendLog(`[体检] ${r.summary}`);
    } catch (e) {
      const t = e instanceof Error ? e.message : String(e);
      setMsg(t);
      appendLog(`[体检失败] ${t}`);
    } finally {
      setBusy(false);
    }
  }, [appendLog]);

  useEffect(() => {
    if (!open) return;
    void runCheck();
  }, [open, runCheck]);

  useEffect(() => {
    if (!open) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape" && !busy) onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [open, busy, onClose]);

  if (!open) return null;

  async function doFix(ids?: string[]) {
    setBusy(true);
    setMsg("");
    try {
      const r = await api.healthFix(ids);
      appendLog(`[修复] ${r.message}`);
      if (r.fixed.length) appendLog(`[修复项] ${r.fixed.join(" · ")}`);
      if (r.skipped.length) appendLog(`[跳过] ${r.skipped.join(" · ")}`);
      setMsg(r.message);
      const again = await api.healthCheck();
      setReport(again);
    } catch (e) {
      const t = e instanceof Error ? e.message : String(e);
      setMsg(t);
      appendLog(`[修复失败] ${t}`);
    } finally {
      setBusy(false);
    }
  }

  function levelClass(level: string) {
    if (level === "ok") return "ok";
    if (level === "bad") return "bad";
    return "";
  }

  return (
    <div
      className="sheet-mask"
      onClick={(e) => {
        if (e.target === e.currentTarget && !busy) onClose();
      }}
    >
      <div className="sheet" role="dialog" aria-modal="true" aria-label="环境体检与修复">
        <div className="sheet-titlebar">
          <div className="sheet-title">环境体检</div>
          <div className="sheet-sub">
            {busy && !report
              ? "检查中…"
              : report
                ? report.summary
                : "—"}
          </div>
          {report ? (
            <div className="sheet-sub" style={{ marginTop: 2, opacity: 0.85 }}>
              修复保守 · 不静默改配置
            </div>
          ) : null}
        </div>
        <div className="sheet-body">
          {report ? (
            <div className="group">
              <div className="group-header">检查项 · {formatCheckedAt(report.checkedAt)}</div>
              {report.items.map((it) => (
                <div key={it.id} className="group-row" style={{ alignItems: "flex-start" }}>
                  <div style={{ flex: 1, minWidth: 0 }}>
                    <div className="group-row-label">{it.title}</div>
                    <div className={`hint ${levelClass(it.level)}`} style={{ marginTop: 2 }}>
                      [{it.level}] {it.message}
                    </div>
                    {it.detail ? (
                      <div className="hint mono" style={{ marginTop: 2, wordBreak: "break-all" }}>
                        {it.detail}
                      </div>
                    ) : null}
                  </div>
                  {it.fixable && it.fixId ? (
                    <button
                      type="button"
                      className="btn btn-default"
                      disabled={busy}
                      onClick={() => void doFix([it.fixId!])}
                    >
                      修复
                    </button>
                  ) : null}
                </div>
              ))}
            </div>
          ) : (
            <p className="hint">{busy ? "正在体检…" : "暂无结果"}</p>
          )}
          {msg ? (
            <pre className="group" style={{ marginTop: 12, padding: 10, whiteSpace: "pre-wrap", fontSize: 11 }}>
              {msg}
            </pre>
          ) : null}
          <p className="hint" style={{ marginTop: 12 }}>
            可自动修：刷新 PATH、secrets 权限。PATH/网络/解析失败多数需新开终端或按诊断提示手动处理。
          </p>
        </div>
        <div className="sheet-foot">
          <button type="button" className="btn btn-default" onClick={onClose} disabled={busy}>
            关闭
          </button>
          <button type="button" className="btn btn-default" disabled={busy} onClick={() => void runCheck()}>
            重新体检
          </button>
          <button
            type="button"
            className="btn btn-primary"
            disabled={busy || !report?.autoFixable?.length}
            onClick={() => void doFix(report?.autoFixable)}
          >
            一键修复可修项
          </button>
        </div>
      </div>
    </div>
  );
}
