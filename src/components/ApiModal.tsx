// SPDX-License-Identifier: MPL-2.0
import { useEffect, useMemo, useState } from "react";
import type { Provider } from "../types";
import { api } from "../lib/api";

export function ApiModal({
  open,
  onClose,
  providers,
  preferredTarget,
  onSaved,
  appendLog,
}: {
  open: boolean;
  onClose: () => void;
  providers: Provider[];
  preferredTarget?: "claude" | "codex" | null;
  onSaved: (result: { target: "claude" | "codex"; tested: boolean }) => void;
  appendLog: (line: string) => void;
}) {
  const [apiType, setApiType] = useState<"domestic" | "official">("domestic");
  const [protocol, setProtocol] = useState<"anthropic" | "openai">("anthropic");
  const [providerId, setProviderId] = useState("deepseek-anthropic");
  const [baseUrl, setBaseUrl] = useState("");
  const [model, setModel] = useState("");
  const [apiKey, setApiKey] = useState("");
  const [showKey, setShowKey] = useState(false);
  const [busy, setBusy] = useState(false);
  const [msg, setMsg] = useState("");

  const target = protocol === "anthropic" ? "claude" : "codex";

  useEffect(() => {
    if (!open || !preferredTarget) return;
    setProtocol(preferredTarget === "claude" ? "anthropic" : "openai");
  }, [open, preferredTarget]);

  const filtered = useMemo(() => {
    return providers.filter((p) => {
      if (p.protocol !== protocol) return false;
      if (apiType === "domestic") return p.group === "domestic" || p.group === "custom";
      return p.group === "official" || p.group === "custom";
    });
  }, [providers, protocol, apiType]);

  useEffect(() => {
    if (!open) return;
    // Provider 列表由后端异步加载；弹窗先打开时也要在数据到达后
    // 选中当前用途/类型的有效 Provider，不能沿用另一协议的旧 id。
    if (!filtered.some((provider) => provider.id === providerId)) {
      const first = filtered[0];
      if (first) setProviderId(first.id);
    }
  }, [open, filtered, providerId]);

  useEffect(() => {
    const p = providers.find((x) => x.id === providerId);
    if (!p) return;
    if (p.group === "custom") {
      setBaseUrl("");
      setModel("");
    } else {
      setBaseUrl(p.baseUrl);
      setModel(p.defaultModel || "");
    }
    // 切换服务商时清 Key，避免串用
    setApiKey("");
    setShowKey(false);
    setMsg("");
  }, [providerId, providers]);

  useEffect(() => {
    if (!open) {
      setApiKey("");
      setShowKey(false);
      setMsg("");
      setBusy(false);
    }
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

  async function testOnly() {
    if (!model.trim()) {
      setMsg("请填写实际模型名");
      return;
    }
    setBusy(true);
    setMsg("");
    try {
      const t = await api.testConnectivity({
        providerId,
        apiKey,
        baseUrl,
        protocol,
        model: model.trim(),
      });
      appendLog(`[测试] ${t.message}`);
      if (t.detail) appendLog(`[摘要] ${t.detail.slice(0, 200)}`);
      setMsg(t.message + (t.detail ? `\n${t.detail}` : ""));
    } catch (e) {
      const text = e instanceof Error ? e.message : String(e);
      setMsg(text);
      appendLog(`[测试失败] ${text}`);
    } finally {
      setBusy(false);
    }
  }

  async function save(opts: { force: boolean }) {
    if (!model.trim()) {
      setMsg("请填写实际模型名");
      return;
    }
    setBusy(true);
    setMsg("");
    try {
      if (opts.force && !confirm("未测通将直接写入本机配置，确定？")) {
        setBusy(false);
        return;
      }

      // 正常保存走后端原子 scheme 事务：连通测试、方案 verified 标记、
      // secrets 与 CLI 配置一次提交，避免“刚测通却显示尚未验证”。
      // 强制保存仍走 apply_config，并明确保留 lastVerifiedOk = null。
      const res = opts.force
        ? await api.applyConfig({
            providerId,
            apiKey,
            baseUrl,
            model: model.trim(),
            target,
          })
        : await api.upsertScheme({
            target,
            providerId,
            apiKey,
            baseUrl,
            model: model.trim(),
            apply: true,
          });
      appendLog(`[配置] ${res.message}`);
      appendLog(`[写入] ${res.written.join(" | ")}`);
      setMsg(res.message);
      onSaved({ target, tested: !opts.force });
      onClose();
    } catch (e) {
      const text = e instanceof Error ? e.message : String(e);
      setMsg(text);
      appendLog(`[配置失败] ${text}`);
    } finally {
      setBusy(false);
    }
  }

  const current = providers.find((p) => p.id === providerId);
  const formReady = Boolean(
    current?.protocol === protocol && baseUrl.trim() && model.trim() && apiKey.trim(),
  );

  return (
    <div
      className="sheet-mask"
      onClick={(e) => {
        if (e.target === e.currentTarget && !busy) onClose();
      }}
    >
      <div className="sheet" role="dialog" aria-modal="true" aria-label="配置 API">
        <div className="sheet-titlebar">
          <div className="sheet-title">配置 API</div>
          <div className="sheet-sub">Key 仅存本机 · 默认先测通再保存</div>
        </div>

        <div className="sheet-body space-y-3">
          <div>
            <div className="field-label">用途</div>
            <div className="seg">
              <button
                type="button"
                className={protocol === "anthropic" ? "on" : ""}
                aria-pressed={protocol === "anthropic"}
                disabled={busy}
                onClick={() => setProtocol("anthropic")}
              >
                Claude Code
              </button>
              <button
                type="button"
                className={protocol === "openai" ? "on" : ""}
                aria-pressed={protocol === "openai"}
                disabled={busy}
                onClick={() => setProtocol("openai")}
              >
                Codex CLI
              </button>
            </div>
          </div>

          <div>
            <div className="field-label">类型</div>
            <div className="seg">
              <button
                type="button"
                className={apiType === "domestic" ? "on" : ""}
                aria-pressed={apiType === "domestic"}
                disabled={busy}
                onClick={() => setApiType("domestic")}
              >
                国产模型
              </button>
              <button
                type="button"
                className={apiType === "official" ? "on" : ""}
                aria-pressed={apiType === "official"}
                disabled={busy}
                onClick={() => setApiType("official")}
              >
                官方 / 自定义
              </button>
            </div>
          </div>

          <div>
            <div className="field-label">服务商</div>
            <div className="chips">
              {filtered.map((p) => (
                <button
                  key={p.id}
                  type="button"
                  className={`chip ${providerId === p.id ? "on" : ""}`}
                  aria-pressed={providerId === p.id}
                  disabled={busy}
                  onClick={() => setProviderId(p.id)}
                >
                  {p.name}
                </button>
              ))}
            </div>
            {current?.note ? <p className="hint mt-1.5">{current.note}</p> : null}
          </div>

          <div>
            <label className="field-label" htmlFor="api-base-url">Base URL</label>
            <input
              id="api-base-url"
              className="field mono"
              value={baseUrl}
              onChange={(e) => setBaseUrl(e.target.value)}
              placeholder="https://..."
              disabled={busy}
              required
            />
          </div>

          <div>
            <label className="field-label" htmlFor="api-model">模型（必填）</label>
            <input
              id="api-model"
              className="field"
              value={model}
              onChange={(e) => setModel(e.target.value)}
              placeholder={
                protocol === "anthropic"
                  ? "deepseek-v4-pro / glm-5.2 / MiniMax-M3"
                  : "qwen3.7-plus / gpt-5.6"
              }
              disabled={busy}
              required
            />
            {!model.trim() ? <p className="hint mt-1.5">请填写该服务商真实支持的模型名。</p> : null}
          </div>

          <div>
            <div className="field-label" style={{ display: "flex", justifyContent: "space-between" }}>
              <label htmlFor="api-key">API Key</label>
              <button type="button" className="btn-text" style={{ height: "auto", padding: 0 }} disabled={busy} onClick={() => setShowKey((v) => !v)}>
                {showKey ? "隐藏" : "显示"}
              </button>
            </div>
            <input
              id="api-key"
              className="field mono"
              type={showKey ? "text" : "password"}
              value={apiKey}
              onChange={(e) => setApiKey(e.target.value)}
              placeholder="sk-..."
              disabled={busy}
            />
            {current?.keyUrl ? (
              <a
                className="hint mt-1 inline-block text-[var(--blue-solid)]"
                href={current.keyUrl}
                target="_blank"
                rel="noreferrer"
              >
                获取 Key →
              </a>
            ) : null}
          </div>

          <div className="group">
            <div className="group-row">
              <span className="group-row-label">写入目标</span>
              <span className="group-row-value">
                {target === "claude" ? "Claude Code" : "Codex CLI"}
              </span>
            </div>
          </div>

          {msg ? (
            <pre className="group" style={{ margin: 0, padding: 10, whiteSpace: "pre-wrap", fontSize: 11 }}>
              {msg}
            </pre>
          ) : null}
        </div>

        <div className="sheet-foot">
          <button type="button" className="btn btn-default" onClick={onClose} disabled={busy}>
            取消
          </button>
          <button
            type="button"
            className="btn btn-default"
            onClick={() => void testOnly()}
            disabled={busy || !formReady}
          >
            仅测试
          </button>
          <button
            type="button"
            className="btn btn-default"
            onClick={() => void save({ force: true })}
            disabled={busy || !formReady}
          >
            强制保存
          </button>
          <button
            type="button"
            className="btn btn-primary"
            onClick={() => void save({ force: false })}
            disabled={busy || !formReady}
          >
            {busy ? "处理中…" : "测试并保存"}
          </button>
        </div>
      </div>
    </div>
  );
}
