// SPDX-License-Identifier: MPL-2.0
import { useEffect, useMemo, useRef, useState } from "react";
import licenseMarkdown from "../../LICENSE?raw";
import privacyMarkdown from "../../PRIVACY.md?raw";
import supportMarkdown from "../../SUPPORT.md?raw";
import thirdPartyMarkdown from "../../THIRD_PARTY_NOTICES.md?raw";
import trademarksMarkdown from "../../TRADEMARKS.md?raw";
import { useDialogFocus } from "../hooks/useDialogFocus";

export type LegalDocumentId =
  | "license"
  | "privacy"
  | "support"
  | "third-party"
  | "trademarks";

const DOCUMENTS: Array<{
  id: LegalDocumentId;
  label: string;
  markdown: string;
}> = [
  { id: "license", label: "开源许可", markdown: licenseMarkdown },
  { id: "privacy", label: "隐私说明", markdown: privacyMarkdown },
  { id: "support", label: "支持政策", markdown: supportMarkdown },
  { id: "third-party", label: "第三方说明", markdown: thirdPartyMarkdown },
  { id: "trademarks", label: "商标政策", markdown: trademarksMarkdown },
];

export function LegalCenterPanel({
  open,
  onClose,
  initialDocument = "license",
}: {
  open: boolean;
  onClose: () => void;
  initialDocument?: LegalDocumentId;
}) {
  const [active, setActive] = useState<LegalDocumentId>(initialDocument);
  const dialogRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (open) setActive(initialDocument);
  }, [open, initialDocument]);

  useDialogFocus(open, dialogRef, onClose);

  const document = DOCUMENTS.find((item) => item.id === active) || DOCUMENTS[0];
  const resolved = useMemo(() => document.markdown, [document.markdown]);

  if (!open) return null;

  return (
    <div className="sheet-mask legal-layer" onClick={(event) => event.target === event.currentTarget && onClose()}>
      <div
        ref={dialogRef}
        className="sheet legal-center-sheet"
        role="dialog"
        aria-modal="true"
        aria-labelledby="legal-center-title"
        tabIndex={-1}
      >
        <div className="sheet-titlebar">
          <div
            className="sheet-title"
            id="legal-center-title"
            tabIndex={-1}
            data-dialog-initial-focus
          >开源许可、隐私与支持</div>
          <div className="sheet-sub">CodeCLI 开源社区版 · 可随时查看</div>
        </div>

        <div className="legal-center-body">
          <nav className="legal-tabs" aria-label="项目文档">
            {DOCUMENTS.map((item) => (
              <button
                key={item.id}
                type="button"
                className={active === item.id ? "on" : ""}
                aria-current={active === item.id ? "page" : undefined}
                onClick={() => setActive(item.id)}
              >
                {item.label}
              </button>
            ))}
          </nav>

          <article className="legal-document" aria-label={document.label}>
            <pre>{resolved}</pre>
          </article>
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
