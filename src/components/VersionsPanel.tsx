// SPDX-License-Identifier: MPL-2.0
import { useCallback, useEffect, useState } from "react";
import { openUrl } from "@tauri-apps/plugin-opener";
import { api } from "../lib/api";
import type { VersionsReport } from "../types";

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

export function VersionsPanel({
  open,
  onClose,
  appendLog,
}: {
  open: boolean;
  onClose: () => void;
  appendLog: (line: string) => void;
}) {
  const [report, setReport] = useState<VersionsReport | null>(null);
  const [busy, setBusy] = useState(false);
  const [msg, setMsg] = useState("");

  const refresh = useCallback(async () => {
    setBusy(true);
    setMsg("");
    try {
      const r = await api.versionsReport();
      setReport(r);
      appendLog(`[版本] ${r.message}`);
    } catch (e) {
      setMsg(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  }, [appendLog]);

  useEffect(() => {
    if (open) void refresh();
  }, [open, refresh]);

  useEffect(() => {
    if (!open) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape" && !busy) onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [open, busy, onClose]);

  if (!open) return null;

  async function upgrade(id: string) {
    if (!confirm(id === "all" ? "升级 Node + Claude + Codex？" : `升级 ${id}？`)) return;
    setBusy(true);
    setMsg("");
    try {
      const r = await api.upgradeComponent(id, true);
      appendLog(`[升级] ${r.message}`);
      r.details.forEach((d) => appendLog(`  · ${d}`));
      setMsg([r.message, ...r.details].join("\n"));
      await refresh();
    } catch (e) {
      const t = e instanceof Error ? e.message : String(e);
      setMsg(t);
      appendLog(`[升级失败] ${t}`);
    } finally {
      setBusy(false);
    }
  }

  const downloadUrl = "https://github.com/Hardboiled98k/codecli/releases/latest";
  const downloadConfigured = true;

  async function openLatestDownload() {
    if (!downloadConfigured) {
      setMsg("无法确定公开版下载地址，请访问 GitHub Releases。");
      return;
    }
    try {
      await openUrl(downloadUrl);
      appendLog(`[更新] 已打开签名版下载页: ${downloadUrl}`);
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      setMsg(`无法打开下载页：${message}`);
    }
  }

  return (
    <div className="sheet-mask" onClick={(e) => e.target === e.currentTarget && !busy && onClose()}>
      <div className="sheet" role="dialog" aria-modal="true" aria-label="版本与更新">
        <div className="sheet-titlebar">
          <div className="sheet-title">版本与更新</div>
          <div className="sheet-sub">
            {formatCheckedAt(report?.checkedAt)} · {report?.message || ""}
          </div>
        </div>
        <div className="sheet-body">
          <div className="group">
            {(report?.components || []).map((c) => (
              <div key={c.id} className="group-row" style={{ alignItems: "flex-start" }}>
                <div style={{ flex: 1, minWidth: 0 }}>
                  <div className="group-row-label">{c.name}</div>
                  <div className={`hint ${c.installed ? "ok" : "bad"}`} style={{ marginTop: 2 }}>
                    {c.installed ? c.version || "已安装" : "未安装"}
                  </div>
                  {c.path ? (
                    <div className="hint mono" style={{ marginTop: 2, wordBreak: "break-all" }}>
                      {c.path}
                    </div>
                  ) : null}
                </div>
                {c.id === "codecli" ? (
                  <button
                    type="button"
                    className="btn btn-default"
                    disabled={busy || !downloadConfigured}
                    title={downloadConfigured ? "前往正式 HTTPS 下载页" : "访问 GitHub Releases"}
                    onClick={() => void openLatestDownload()}
                  >
                    获取最新签名版
                  </button>
                ) : c.upgradable ? (
                  <button type="button" className="btn btn-default" disabled={busy} onClick={() => void upgrade(c.id)}>
                    {c.installed ? "升级" : "安装"}
                  </button>
                ) : null}
              </div>
            ))}
          </div>
          {msg ? (
            <pre className="group" style={{ marginTop: 12, padding: 10, whiteSpace: "pre-wrap", fontSize: 11 }}>
              {msg}
            </pre>
          ) : null}
        </div>
        <div className="sheet-foot">
          <button type="button" className="btn btn-default" onClick={onClose} disabled={busy}>
            关闭
          </button>
          <button type="button" className="btn btn-default" disabled={busy} onClick={() => void refresh()}>
            刷新
          </button>
          <button type="button" className="btn btn-primary" disabled={busy} onClick={() => void upgrade("all")}>
            全部升级
          </button>
        </div>
      </div>
    </div>
  );
}
