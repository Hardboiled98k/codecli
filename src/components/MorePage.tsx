// SPDX-License-Identifier: MPL-2.0
import type { MoreAction } from "./MoreMenu";

const SECTIONS: {
  title: string;
  items: {
    id: MoreAction;
    label: string;
    desc: string;
    /** 不弹窗，只给提示的动作 */
    soft?: boolean;
  }[];
}[] = [
  {
    title: "日常使用",
    items: [
      {
        id: "schemes",
        label: "Provider / 模型方案",
        desc: "多套线路保存与一键切换，Key 仅本机",
      },
      {
        id: "first_project",
        label: "开始第一个项目",
        desc: "选择或创建文件夹、生成需求草稿并启动 CLI",
      },
      {
        id: "templates",
        label: "新手任务模板",
        desc: "需求澄清、改 Bug、做网页等提示模板",
      },
    ],
  },
  {
    title: "维护与修复",
    items: [
      {
        id: "health",
        label: "环境体检与一键修复",
        desc: "PATH、命令路径、配置解析、secrets 权限",
      },
      {
        id: "versions",
        label: "版本与更新",
        desc: "查看 Node / Claude / Codex 版本并安全升级",
      },
      {
        id: "backup",
        label: "配置备份与恢复",
        desc: "改配置前备份，出问题可回滚",
      },
    ],
  },
  {
    title: "扩展",
    items: [
      {
        id: "skills",
        label: "精选 Skills",
        desc: "白名单任务方法模板，可装可卸",
      },
      {
        id: "mcp",
        label: "精选 MCP",
        desc: "安全说明与手动添加指引（不装任意包）",
      },
      {
        id: "feishu",
        label: "飞书 CLI",
        desc: "可选安装 lark-cli，需自行登录授权",
      },
    ],
  },
  {
    title: "支持与关于",
    items: [
      {
        id: "support_logs",
        label: "诊断与导出日志",
        desc: "生成仅保存在本机的脱敏日志文件",
        soft: true,
      },
      {
        id: "privacy",
        label: "法律、隐私与支持",
        desc: "开源许可、隐私、支持与第三方说明",
      },
      {
        id: "about",
        label: "版本 / 更新日志",
        desc: "查看当前安装包附带的最新发布记录",
      },
    ],
  },
];

export type { MoreAction };

/** 覆盖主内容区；顶栏保留，用顶栏「返回」关闭 */
export function MorePage({
  open,
  onAction,
  toast,
}: {
  open: boolean;
  onClose?: () => void;
  onAction: (action: MoreAction) => void;
  /** 页内可见反馈（已复制/关于等） */
  toast?: string | null;
}) {
  if (!open) return null;

  return (
    <div className="more-page" role="region" aria-label="更多">
      <div className="more-page-scroll">
        <div className="more-page-intro">
          <div className="more-page-title">更多</div>
          <div className="more-page-sub">
            点功能打开面板，关闭后仍在此页 · 顶栏「返回」回首页
          </div>
          {toast ? (
            <div className="more-page-toast" role="status">
              {toast}
            </div>
          ) : null}
        </div>
        {SECTIONS.map((sec) => (
          <div key={sec.title} className="group more-page-group">
            <div className="group-header">{sec.title}</div>
            {sec.items.map((it) => (
              <button
                key={it.id}
                type="button"
                className="more-page-row"
                onClick={() => onAction(it.id)}
              >
                <div className="more-page-row-text">
                  <div className="more-page-row-label">{it.label}</div>
                  <div className="more-page-row-desc">{it.desc}</div>
                </div>
                <span className="more-page-chevron" aria-hidden>
                  {it.soft ? "·" : "›"}
                </span>
              </button>
            ))}
          </div>
        ))}
      </div>
    </div>
  );
}
