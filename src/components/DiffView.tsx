import { useEffect, useMemo, useState } from "react";
import type { CompareResult } from "../types";

type Props = {
  result: CompareResult | null;
  emptyMessage?: string;
};

const PAGE = 1500;

type Line = {
  kind: "same" | "ins" | "del" | "skip" | "empty";
  oldNo: number | null;
  newNo: number | null;
  text: string;
};

export function DiffView({ result, emptyMessage = "Select two versions to compare them." }: Props) {
  const [mode, setMode] = useState<"split" | "unified">("split");
  const [onlyChanges, setOnlyChanges] = useState(false);

  const lines = useMemo<Line[]>(() => {
    if (!result) return [];
    const out: Line[] = [];
    for (const row of result.rows) {
      if (row.tag === "skip") {
        out.push({ kind: "skip", oldNo: null, newNo: null, text: row.text });
        continue;
      }
      if (row.tag === "same") {
        out.push({ kind: "same", oldNo: row.oldLine, newNo: row.newLine, text: row.text });
        continue;
      }
      if (row.tag === "del") {
        out.push({ kind: "del", oldNo: row.oldLine, newNo: null, text: row.text });
        continue;
      }
      out.push({ kind: "ins", oldNo: null, newNo: row.newLine, text: row.text });
    }
    return out;
  }, [result]);

  const [limit, setLimit] = useState(PAGE);

  useEffect(() => {
    setLimit(PAGE);
  }, [result, onlyChanges]);

  const filtered = useMemo(
    () => (onlyChanges ? lines.filter((line) => line.kind === "ins" || line.kind === "del" || line.kind === "skip") : lines),
    [lines, onlyChanges],
  );
  const visible = filtered.slice(0, limit);

  if (!result) {
    return <div className="diff-empty">{emptyMessage}</div>;
  }

  if (!result.isText) {
    return (
      <div className="diff-binary">
        <div className="binary-card">
          <div className="binary-title">Binary content</div>
          <div className="binary-grid">
            <div>
              <span className="label">Previous</span>
              <span>{result.oldLabel}</span>
              <span className="muted">{result.oldSize.toLocaleString()} bytes</span>
            </div>
            <div>
              <span className="label">Selected</span>
              <span>{result.newLabel}</span>
              <span className="muted">{result.newSize.toLocaleString()} bytes</span>
            </div>
          </div>
          <pre className="unified">{result.unified}</pre>
          <p className="muted">
            Text previews are available for text files, Word documents, Excel workbooks and PDF files. Other file types
            are restored byte for byte.
          </p>
        </div>
      </div>
    );
  }

  return (
    <div className="diff">
      <div className="diff-toolbar">
        <div className="diff-summary">
          <span className="badge added">+{result.added}</span>
          <span className="badge removed">-{result.removed}</span>
          <span className="muted">{result.rows.length.toLocaleString()} rows</span>
          {result.truncated ? <span className="muted">truncated</span> : null}
        </div>
        <div className="diff-actions">
          <label className="toggle">
            <input type="checkbox" checked={onlyChanges} onChange={(e) => setOnlyChanges(e.target.checked)} />
            only changes
          </label>
          <div className="segmented">
            <button className={mode === "split" ? "active" : ""} onClick={() => setMode("split")}>
              side by side
            </button>
            <button className={mode === "unified" ? "active" : ""} onClick={() => setMode("unified")}>
              unified
            </button>
          </div>
        </div>
      </div>

      {mode === "unified" ? (
        <div className="diff-pane">
          <pre className="unified">{result.unified}</pre>
        </div>
      ) : (
        <div className="diff-split">
          <div className="diff-head">
            <div>
              <span className="label">Previous</span> {result.oldLabel}
            </div>
            <div>
              <span className="label">Selected</span> {result.newLabel}
            </div>
          </div>
          <div className="diff-body">
            {visible.map((line, index) => (
              <div className="diff-row" key={`${index}-${line.kind}`}>
                <div className={`cell num ${line.kind === "ins" ? "empty" : line.kind}`}>{line.oldNo ?? ""}</div>
                <div className={`cell text ${line.kind === "ins" ? "empty" : line.kind}`}>
                  {line.kind === "skip" ? (
                    <span className="skip-label">{line.text}</span>
                  ) : line.kind === "ins" ? null : (
                    <>
                      <span className="marker">{line.kind === "del" ? "-" : " "}</span>
                      {line.text}
                    </>
                  )}
                </div>
                <div className={`cell num ${line.kind === "del" ? "empty" : line.kind}`}>{line.newNo ?? ""}</div>
                <div className={`cell text ${line.kind === "del" ? "empty" : line.kind}`}>
                  {line.kind === "skip" ? (
                    <span className="skip-label">{line.text}</span>
                  ) : line.kind === "del" ? null : (
                    <>
                      <span className="marker">{line.kind === "ins" ? "+" : " "}</span>
                      {line.text}
                    </>
                  )}
                </div>
              </div>
            ))}
            {filtered.length > limit ? (
              <div className="pad">
                <button className="secondary" onClick={() => setLimit((l) => l + PAGE)}>
                  Show more ({filtered.length - limit} remaining)
                </button>
              </div>
            ) : null}
          </div>
        </div>
      )}
    </div>
  );
}
