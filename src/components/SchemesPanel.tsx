// SPDX-License-Identifier: MPL-2.0
import { useCallback, useEffect, useState } from "react";
import { api } from "../lib/api";
import type { Scheme, SchemeListResult } from "../types";

export function SchemesPanel({
  open,
  onClose,
  appendLog,
  onOpenConfig,
}: {
  open: boolean;
  onClose: () => void;
  appendLog: (line: string) => void;
  onOpenConfig: () => void;
}) {
  const [data, setData] = useState<SchemeListResult | null>(null);
  const [busy, setBusy] = useState(false);
  const [msg, setMsg] = useState("");

  const refresh = useCallback(async () => {
    setMsg("");
    try {
      const r = await api.listSchemes();
      setData(r);
    } catch (e) {
      setMsg(e instanceof Error ? e.message : String(e));
    }
  }, []);

  useEffect(() => {
    if (!open) return;
    void refresh();
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

  const claude = (data?.schemes || []).filter((s) => s.target === "claude");
  const codex = (data?.schemes || []).filter((s) => s.target === "codex");

  async function switchTo(s: Scheme) {
    if (
      !confirm(
        `切换到「${s.name}」？\n目标: ${s.target}\nProvider: ${s.providerId}\nBase: ${s.baseUrl}\n模型: ${s.model || "—"}\nKey: ${s.apiKeyMasked}`,
      )
    ) {
      return;
    }
    setBusy(true);
    setMsg("");
    try {
      const r = await api.switchScheme(s.id);
      appendLog(`[方案] ${r.message}`);
      setMsg(r.message);
      await refresh();
    } catch (e) {
      const t = e instanceof Error ? e.message : String(e);
      setMsg(t);
      appendLog(`[方案失败] ${t}`);
    } finally {
      setBusy(false);
    }
  }

  async function remove(s: Scheme) {
    if (!confirm(`删除方案「${s.name}」？\n不会修改当前 CLI 配置。`)) return;
    setBusy(true);
    setMsg("");
    try {
      const r = await api.deleteScheme(s.id);
      appendLog(`[方案] ${r.message}`);
      await refresh();
    } catch (e) {
      setMsg(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  }

  function renderGroup(title: string, list: Scheme[], activeId?: string | null) {
    return (
      <div className="group" style={{ marginBottom: 12 }}>
        <div className="group-header">{title}</div>
        {list.length === 0 ? (
          <p className="group-note">暂无方案。先「配置 API」保存一套，会自动收入此处。</p>
        ) : (
          list.map((s) => {
            const active = activeId === s.id;
            return (
              <div key={s.id} className="group-row" style={{ flexWrap: "wrap", gap: 8 }}>
                <div style={{ flex: 1, minWidth: 160 }}>
                  <div className="group-row-label">
                    {s.name}
                    {active ? (
                      <span className="ok" style={{ marginLeft: 8, fontSize: 11 }}>
                        当前
                      </span>
                    ) : null}
                  </div>
                  <div className="hint" style={{ marginTop: 2 }}>
                    {s.providerId} · {s.model || "默认模型"} · {s.apiKeyMasked}
                  </div>
                  <div
                    className={s.lastVerifiedOk === true ? "ok" : s.lastVerifiedOk === false ? "bad" : "hint"}
                    style={{ marginTop: 2, fontSize: 11 }}
                  >
                    {s.lastVerifiedOk === true
                      ? "上次连通验证通过"
                      : s.lastVerifiedOk === false
                        ? "上次连通验证失败（已保留原配置）"
                        : "尚未验证"}
                  </div>
                  <div className="hint mono" style={{ marginTop: 2, wordBreak: "break-all" }}>
                    {s.baseUrl}
                  </div>
                </div>
                <div style={{ display: "flex", gap: 6 }}>
                  <button
                    type="button"
                    className="btn btn-primary"
                    disabled={busy || active}
                    onClick={() => void switchTo(s)}
                  >
                    切换
                  </button>
                  <button
                    type="button"
                    className="btn btn-default"
                    disabled={busy}
                    onClick={() => void remove(s)}
                  >
                    删除
                  </button>
                </div>
              </div>
            );
          })
        )}
      </div>
    );
  }

  return (
    <div
      className="sheet-mask"
      onClick={(e) => {
        if (e.target === e.currentTarget && !busy) onClose();
      }}
    >
      <div className="sheet" role="dialog" aria-modal="true" aria-label="Provider 与模型方案">
        <div className="sheet-titlebar">
          <div className="sheet-title">Provider / 模型方案</div>
          <div className="sheet-sub">整套切换 · Key 仅本机 · 界面只显示尾号</div>
        </div>
        <div className="sheet-body">
          <p className="hint" style={{ marginTop: 0 }}>
            切换会写入 Claude / Codex 配置。数据将发往该方案的 Base URL。
          </p>
          {renderGroup("Claude Code", claude, data?.activeClaude)}
          {renderGroup("Codex CLI", codex, data?.activeCodex)}
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
            className="btn btn-default"
            disabled={busy}
            onClick={() => void refresh()}
          >
            刷新
          </button>
          <button
            type="button"
            className="btn btn-primary"
            disabled={busy}
            onClick={() => {
              onClose();
              onOpenConfig();
            }}
          >
            新建 / 配置 API…
          </button>
        </div>
      </div>
    </div>
  );
}
