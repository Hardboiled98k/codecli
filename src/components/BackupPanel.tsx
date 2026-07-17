// SPDX-License-Identifier: MPL-2.0
import { useCallback, useEffect, useState } from "react";
import { api } from "../lib/api";
import type { BackupItem } from "../types";

export function BackupPanel({
  open,
  onClose,
  appendLog,
}: {
  open: boolean;
  onClose: () => void;
  appendLog: (line: string) => void;
}) {
  const [items, setItems] = useState<BackupItem[]>([]);
  const [busy, setBusy] = useState(false);
  const [msg, setMsg] = useState("");

  const refresh = useCallback(async () => {
    setMsg("");
    try {
      const r = await api.listBackups();
      setItems(r.items || []);
    } catch (e) {
      setMsg(e instanceof Error ? e.message : String(e));
    }
  }, []);

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

  async function create() {
    setBusy(true);
    setMsg("");
    try {
      const r = await api.createBackup("手动备份");
      appendLog(`[备份] ${r.message}`);
      setMsg(r.message);
      await refresh();
    } catch (e) {
      setMsg(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  }

  async function restore(id: string) {
    if (
      !confirm(
        `恢复备份 ${id}？\n\n· schemes / ownership / secrets：按备份精确恢复（备份中缺失则删除当前文件）\n· settings.json：精确恢复本工具管理的 Claude env 键，hooks / 其他字段保留\n· config.toml：精确恢复 model、model_provider 和 codecli_installer provider，其他 provider / MCP 保留\n\n恢复前会自动再备份当前状态。确定？`,
      )
    ) {
      return;
    }
    setBusy(true);
    setMsg("");
    try {
      const r = await api.restoreBackup(id);
      appendLog(`[恢复] ${r.message}`);
      setMsg(r.message);
      await refresh();
    } catch (e) {
      setMsg(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  }

  async function remove(id: string) {
    if (!confirm(`删除备份 ${id}？`)) return;
    setBusy(true);
    try {
      const r = await api.deleteBackup(id);
      appendLog(`[备份] ${r.message}`);
      await refresh();
    } catch (e) {
      setMsg(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="sheet-mask" onClick={(e) => e.target === e.currentTarget && !busy && onClose()}>
      <div className="sheet" role="dialog" aria-modal="true" aria-label="配置备份与恢复">
        <div className="sheet-titlebar">
          <div className="sheet-title">配置备份与恢复</div>
          <div className="sheet-sub">含 schemes / ownership / secrets / settings / codex toml</div>
        </div>
        <div className="sheet-body">
          <p className="hint" style={{ marginTop: 0 }}>
            恢复会将本工具的私有状态和受管配置还原到所选备份点，保留 hooks、其他
            provider / MCP 等非受管自定义。恢复前会自动再备份当前状态；最多保留 20 份。
          </p>
          <div className="group">
            <div className="group-header">备份列表 · {items.length}</div>
            {items.length === 0 ? (
              <p className="group-note">暂无备份。点下方「立即备份」。</p>
            ) : (
              items.map((it) => (
                <div key={it.id} className="group-row" style={{ alignItems: "flex-start", flexWrap: "wrap" }}>
                  <div style={{ flex: 1, minWidth: 160 }}>
                    <div className="group-row-label mono">{it.id}</div>
                    <div className="hint" style={{ marginTop: 2 }}>
                      {it.createdAt || "—"} · {it.note}
                    </div>
                    <div className="hint" style={{ marginTop: 2 }}>
                      {it.files?.length || 0} 个文件
                    </div>
                  </div>
                  <div style={{ display: "flex", gap: 6 }}>
                    <button type="button" className="btn btn-primary" disabled={busy} onClick={() => void restore(it.id)}>
                      恢复
                    </button>
                    <button type="button" className="btn btn-default" disabled={busy} onClick={() => void remove(it.id)}>
                      删除
                    </button>
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
          <button
            type="button"
            className="btn btn-default"
            disabled={busy}
            onClick={() =>
              void api
                .openBackupsFolder()
                .then(appendLog)
                .catch((e) => {
                  const text = `打开备份目录失败: ${String(e)}`;
                  setMsg(text);
                  appendLog(text);
                })
            }
          >
            打开目录
          </button>
          <button type="button" className="btn btn-primary" disabled={busy} onClick={() => void create()}>
            立即备份
          </button>
        </div>
      </div>
    </div>
  );
}
