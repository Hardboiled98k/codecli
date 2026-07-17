// SPDX-License-Identifier: MPL-2.0
import { useCallback, useEffect, useState } from "react";
import { api } from "../lib/api";
import type { ExtensionItem } from "../types";

export function ExtensionsPanel({
  open,
  onClose,
  appendLog,
  filterKind,
}: {
  open: boolean;
  onClose: () => void;
  appendLog: (line: string) => void;
  /** skill | mcp | cli | all */
  filterKind?: string;
}) {
  const [items, setItems] = useState<ExtensionItem[]>([]);
  const [busy, setBusy] = useState(false);
  const [msg, setMsg] = useState("");

  const refresh = useCallback(async () => {
    setMsg("");
    try {
      const r = await api.listExtensions();
      let list = r.items || [];
      if (filterKind && filterKind !== "all") {
        list = list.filter((i) => i.kind === filterKind);
      }
      setItems(list);
    } catch (e) {
      setMsg(e instanceof Error ? e.message : String(e));
    }
  }, [filterKind]);

  useEffect(() => {
    if (!open) return;
    void refresh();
  }, [open, filterKind, refresh]);

  useEffect(() => {
    if (!open) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape" && !busy) onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [open, busy, onClose]);

  if (!open) return null;

  async function install(id: string, name: string) {
    if (!confirm(`安装「${name}」？\n仅白名单扩展，请阅读风险说明。`)) return;
    setBusy(true);
    setMsg("");
    try {
      const r = await api.installExtension(id);
      appendLog(`[扩展] ${r.message}`);
      setMsg(r.message);
      await refresh();
    } catch (e) {
      const t = e instanceof Error ? e.message : String(e);
      setMsg(t);
      appendLog(`[扩展失败] ${t}`);
    } finally {
      setBusy(false);
    }
  }

  async function uninstall(id: string, name: string) {
    if (!confirm(`卸载「${name}」？`)) return;
    setBusy(true);
    try {
      const r = await api.uninstallExtension(id);
      appendLog(`[扩展] ${r.message}`);
      setMsg(r.message);
      await refresh();
    } catch (e) {
      setMsg(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  }

  const title =
    filterKind === "skill"
      ? "精选 Skills"
      : filterKind === "mcp"
        ? "精选 MCP"
        : filterKind === "cli"
          ? "飞书 CLI"
          : "精选扩展";

  return (
    <div className="sheet-mask" onClick={(e) => e.target === e.currentTarget && !busy && onClose()}>
      <div className="sheet" role="dialog" aria-modal="true" aria-label={title}>
        <div className="sheet-titlebar">
          <div className="sheet-title">{title}</div>
          <div className="sheet-sub">白名单 only · 仅可卸载本工具安装项 · 不做任意商店</div>
        </div>
        <div className="sheet-body">
          <div className="group">
            {items.length === 0 ? (
              <p className="group-note">暂无条目</p>
            ) : (
              items.map((it) => (
                <div key={it.id} className="group-row" style={{ alignItems: "flex-start", flexWrap: "wrap" }}>
                  <div style={{ flex: 1, minWidth: 180 }}>
                    <div className="group-row-label">
                      {it.name}
                      {it.ownedByTool ? (
                        <span className="ok" style={{ marginLeft: 8, fontSize: 11 }}>
                          {it.installed ? "本工具已装" : "待清理"}
                        </span>
                      ) : it.installed ? (
                        <span className="hint" style={{ marginLeft: 8, fontSize: 11 }}>
                          用户自装
                        </span>
                      ) : null}
                    </div>
                    <div className="hint" style={{ marginTop: 2 }}>
                      {it.description}
                    </div>
                    <div className="hint" style={{ marginTop: 2 }}>
                      风险：{it.risk}
                    </div>
                    <div className="hint mono" style={{ marginTop: 2, wordBreak: "break-all" }}>
                      来源：{it.source}
                    </div>
                    {it.detail ? (
                      <div className="hint mono" style={{ marginTop: 2, wordBreak: "break-all" }}>
                        {it.detail}
                      </div>
                    ) : null}
                  </div>
                  <div style={{ display: "flex", gap: 6 }}>
                    {it.canUninstall ? (
                      <button
                        type="button"
                        className="btn btn-default"
                        disabled={busy}
                        onClick={() => void uninstall(it.id, it.name)}
                      >
                        卸载
                      </button>
                    ) : it.installed ? (
                      <button type="button" className="btn btn-default" disabled>
                        用户自装
                      </button>
                    ) : (
                      <button
                        type="button"
                        className="btn btn-primary"
                        disabled={busy}
                        onClick={() => void install(it.id, it.name)}
                      >
                        安装
                      </button>
                    )}
                  </div>
                </div>
              ))
            )}
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
        </div>
      </div>
    </div>
  );
}
