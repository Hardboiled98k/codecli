// SPDX-License-Identifier: MPL-2.0
import { useEffect, useState } from "react";

const TEMPLATES: { id: string; title: string; desc: string; prompt: string }[] = [
  {
    id: "clarify",
    title: "需求澄清",
    desc: "把模糊想法变成可执行需求",
    prompt: `我是新手。请用中文通过 5 个以内问题帮我澄清需求，然后输出：
1) 一句话目标
2) 范围内/范围外
3) 验收标准
4) 最小可行版本（MVP）步骤
不要直接写大量代码，先确认需求。`,
  },
  {
    id: "bug",
    title: "Bug 排查",
    desc: "结构化定位与最小修复",
    prompt: `请按以下结构排查问题（中文）：
1. 复现步骤 / 期望 / 实际
2. 2–3 个假设（按验证成本排序）
3. 需要我提供的日志或文件
4. 最小修复方案与回归点
先别大范围改代码。`,
  },
  {
    id: "web",
    title: "做个小网页",
    desc: "从零做一个可打开的页面",
    prompt: `请帮我做一个最小可运行的网页项目：
- 单页即可，中文界面
- 本地能直接打开或简单启动
- 先给目录结构与计划，再实现 MVP
- 每步说明如何验证
技术栈若未指定，优先最简单方案。`,
  },
  {
    id: "explain",
    title: "解释这段代码",
    desc: "读懂现有文件",
    prompt: `请阅读我指定的文件/目录，用中文解释：
1. 整体做什么
2. 关键入口与数据流
3. 我最该先改哪里
4. 风险点
先总结，不要急着重构。`,
  },
  {
    id: "refactor",
    title: "小步重构",
    desc: "可回滚的小改动",
    prompt: `请做一次小步、可回滚的重构：
- 先说明动机与影响面
- 一次只改一个关注点
- 保持行为不变
- 给出如何验证没坏
不要顺手加新功能。`,
  },
  {
    id: "test",
    title: "补测试",
    desc: "为关键路径加测试",
    prompt: `请为当前改动/模块补最小必要测试：
- 先指出最值得测的 2–3 个路径
- 写出可运行的测试
- 说明如何本地跑
优先覆盖回归风险，不追求 100% 覆盖。`,
  },
];

export function TemplatesPanel({
  open,
  onClose,
  appendLog,
}: {
  open: boolean;
  onClose: () => void;
  appendLog: (line: string) => void;
}) {
  const [msg, setMsg] = useState("");

  useEffect(() => {
    if (!open) return;
    setMsg("");
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [open, onClose]);

  if (!open) return null;

  async function copy(text: string, title: string) {
    try {
      await navigator.clipboard.writeText(text);
      setMsg(`已复制「${title}」`);
      appendLog(`[模板] 已复制：${title}`);
    } catch {
      setMsg("复制失败，请手动选中");
    }
  }

  return (
    <div
      className="sheet-mask"
      onClick={(e) => {
        if (e.target === e.currentTarget) onClose();
      }}
    >
      <div className="sheet" role="dialog" aria-modal="true" aria-label="新手任务模板">
        <div className="sheet-titlebar">
          <div className="sheet-title">新手任务模板</div>
          <div className="sheet-sub">复制提示 → 粘贴到 Claude / Codex</div>
        </div>
        <div className="sheet-body">
          <div className="group">
            {TEMPLATES.map((t) => (
              <div key={t.id} className="group-row" style={{ alignItems: "flex-start" }}>
                <div style={{ flex: 1, minWidth: 0 }}>
                  <div className="group-row-label">{t.title}</div>
                  <div className="hint" style={{ marginTop: 2 }}>
                    {t.desc}
                  </div>
                </div>
                <button type="button" className="btn btn-default" onClick={() => void copy(t.prompt, t.title)}>
                  复制
                </button>
              </div>
            ))}
          </div>
          {msg ? <p className="hint" style={{ marginTop: 10 }}>{msg}</p> : null}
        </div>
        <div className="sheet-foot">
          <button type="button" className="btn btn-default" onClick={onClose}>
            关闭
          </button>
        </div>
      </div>
    </div>
  );
}
