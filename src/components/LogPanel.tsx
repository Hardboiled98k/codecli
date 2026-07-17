// SPDX-License-Identifier: MPL-2.0
import { useEffect, useRef } from "react";

export function LogPanel({ lines }: { lines: string[] }) {
  const ref = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (ref.current) ref.current.scrollTop = ref.current.scrollHeight;
  }, [lines]);

  return (
    <div className="group log-group">
      <div className="group-header log-head-row">
        <span>活动日志</span>
        <span className="log-count">{lines.length}</span>
      </div>
      <div ref={ref} className="log-body">
        {lines.length === 0 ? (
          <div className="log-empty">暂无记录。开始安装或配置后会显示在这里。</div>
        ) : (
          lines.map((line, i) => (
            <div key={`${i}-${line.slice(0, 20)}`} className="log-line">
              {line}
            </div>
          ))
        )}
      </div>
    </div>
  );
}
