// SPDX-License-Identifier: MPL-2.0
import { useEffect } from "react";
import changelog from "../../docs/CHANGELOG.md?raw";

function latestRelease(markdown: string) {
  const start = markdown.search(/^##\s+\d/m);
  if (start < 0) return markdown.trim();
  const release = markdown.slice(start);
  const next = release.slice(1).search(/^##\s+\d/m);
  return (next < 0 ? release : release.slice(0, next + 1)).trim();
}

const LATEST_RELEASE = latestRelease(changelog);

export function ChangelogPanel({
  open,
  onClose,
}: {
  open: boolean;
  onClose: () => void;
}) {
  useEffect(() => {
    if (!open) return;
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [open, onClose]);

  if (!open) return null;

  return (
    <div className="sheet-mask" onClick={(event) => event.target === event.currentTarget && onClose()}>
      <div className="sheet" role="dialog" aria-modal="true" aria-label="版本与更新日志">
        <div className="sheet-titlebar">
          <div className="sheet-title">版本 / 更新日志</div>
          <div className="sheet-sub">随当前安装包构建的最新源码记录</div>
        </div>
        <div className="sheet-body">
          <pre
            className="group"
            style={{ margin: 0, padding: 12, whiteSpace: "pre-wrap", fontSize: 11, lineHeight: 1.55 }}
          >
            {LATEST_RELEASE || "暂无更新日志"}
          </pre>
        </div>
        <div className="sheet-foot">
          <button type="button" className="btn btn-primary" onClick={onClose}>
            关闭
          </button>
        </div>
      </div>
    </div>
  );
}
