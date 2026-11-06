import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { open, save } from "@tauri-apps/plugin-dialog";
import { api } from "../api";
import { DiffView } from "../components/DiffView";
import type { CompareResult, FileInfo, FolderInfo, VersionInfo, VersionPreview } from "../types";
import { eventLabel, formatBytes, formatDateTime, formatTime, relativeTime } from "../utils";

type Props = {
  folders: FolderInfo[];
  historyTick: number;
  settingsTick: number;
  notify: (message: string, kind?: "info" | "error") => void;
};

export function RestorePage({ folders, historyTick, settingsTick, notify }: Props) {
  const [folderId, setFolderId] = useState<number | null>(null);
  const [files, setFiles] = useState<FileInfo[]>([]);
  const [fileId, setFileId] = useState<number | null>(null);
  const [versions, setVersions] = useState<VersionInfo[]>([]);
  const [selectedVersion, setSelectedVersion] = useState<number | null>(null);
  const [compareIds, setCompareIds] = useState<number[]>([]);
  const [preview, setPreview] = useState<VersionPreview | null>(null);
  const [compare, setCompare] = useState<CompareResult | null>(null);
  const [showCompare, setShowCompare] = useState(false);
  const [query, setQuery] = useState("");
  const [debouncedQuery, setDebouncedQuery] = useState("");
  const [includeDeleted, setIncludeDeleted] = useState(true);
  const [busy, setBusy] = useState(false);
  const [folderPointInTime, setFolderPointInTime] = useState("");
  const filesRequest = useRef(0);
  const previewRequest = useRef(0);
  const historyRequest = useRef(0);

  useEffect(() => {
    void api.getSettings().then((s) => setIncludeDeleted(s.showDeleted));
  }, [settingsTick]);

  useEffect(() => {
    if ((folderId === null || !folders.some((f) => f.id === folderId)) && folders.length > 0) {
      setFolderId(folders.find((f) => !f.archived)?.id ?? folders[0].id);
    }
  }, [folders, folderId]);

  useEffect(() => {
    setFileId(null);
    setVersions([]);
    setSelectedVersion(null);
    setCompareIds([]);
    setPreview(null);
  }, [folderId]);

  useEffect(() => {
    const timer = window.setTimeout(() => setDebouncedQuery(query), 200);
    return () => window.clearTimeout(timer);
  }, [query]);

  const loadFiles = useCallback(async () => {
    const request = ++filesRequest.current;
    if (folderId === null) {
      setFiles([]);
      return;
    }
    const list = await api.listFiles(folderId, includeDeleted, debouncedQuery);
    if (request === filesRequest.current) {
      setFiles(list);
    }
  }, [folderId, includeDeleted, debouncedQuery]);

  useEffect(() => {
    void loadFiles();
  }, [loadFiles, historyTick]);

  const loadHistory = useCallback(async (id: number, keepSelection: boolean) => {
    const request = ++historyRequest.current;
    const history = await api.getHistory(id);
    if (request !== historyRequest.current) return;
    setVersions(history);
    setSelectedVersion((current) =>
      keepSelection && current !== null && history.some((v) => v.id === current)
        ? current
        : history.length > 0
          ? history[0].id
          : null,
    );
    setCompareIds((current) => current.filter((cid) => history.some((v) => v.id === cid)));
  }, []);

  useEffect(() => {
    if (fileId === null) {
      historyRequest.current++;
      setVersions([]);
      setPreview(null);
      return;
    }
    void loadHistory(fileId, false);
  }, [fileId, loadHistory]);

  useEffect(() => {
    if (fileId !== null) {
      void loadHistory(fileId, true);
    }
  }, [historyTick]);

  useEffect(() => {
    const request = ++previewRequest.current;
    if (selectedVersion === null) {
      setPreview(null);
      return;
    }
    void api
      .getPreview(selectedVersion)
      .then((p) => {
        if (request === previewRequest.current) setPreview(p);
      })
      .catch((e) => notify(String(e), "error"));
  }, [selectedVersion, notify]);

  const currentFolder = useMemo(
    () => folders.find((folder) => folder.id === folderId) ?? null,
    [folders, folderId],
  );
  const currentVersion = versions.find((v) => v.id === selectedVersion) ?? null;
  const currentFile = files.find((f) => f.id === fileId) ?? null;

  async function withBusy(action: () => Promise<void>) {
    setBusy(true);
    try {
      await action();
    } catch (error) {
      notify(String(error), "error");
    } finally {
      setBusy(false);
    }
  }

  async function handleRestoreAs(versionId: number) {
    const suggested = await api.suggestRestorePath(versionId);
    const target = await save({ defaultPath: suggested, title: "Restore version as" });
    if (!target) return;
    const outcome = await api.restoreAs(versionId, target);
    notify(`Restored to ${outcome.path}${outcome.tracked ? " (it will be tracked)" : ""}`);
  }

  async function handleRestoreInPlace(versionId: number) {
    const question =
      currentFile?.status === "deleted"
        ? "Recreate the deleted file with this content?"
        : "Replace the current file with this version? The current content is saved as a version first.";
    if (!window.confirm(question)) {
      return;
    }
    const outcome = await api.restoreVersionInPlace(versionId);
    notify(`Restored ${outcome.path}`);
    await loadFiles();
    if (fileId !== null) await loadHistory(fileId, false);
  }

  async function handleRestoreFolder() {
    if (!currentFolder) return;
    const ts = folderPointInTime ? new Date(folderPointInTime).getTime() : Date.now();
    const target = await open({ directory: true, title: "Choose an empty folder for the restore" });
    if (!target || Array.isArray(target)) return;
    const written = await api.restoreFolderAt(currentFolder.id, ts, target);
    notify(`Restored ${written.length} files into ${target}`);
  }

  function toggleCompare(versionId: number) {
    setCompareIds((current) => {
      if (current.includes(versionId)) return current.filter((id) => id !== versionId);
      return [...current, versionId].slice(-2);
    });
  }

  function orderedCompareIds(): [number, number] | null {
    if (compareIds.length !== 2) return null;
    const [a, b] = compareIds.map((id) => versions.find((v) => v.id === id));
    if (!a || !b) return null;
    return a.ts <= b.ts ? [a.id, b.id] : [b.id, a.id];
  }

  async function openCompare() {
    const ids = orderedCompareIds();
    if (!ids) return;
    setCompare(await api.compare(ids[0], ids[1]));
    setShowCompare(true);
  }

  async function openExternalDiff() {
    const ids = orderedCompareIds();
    if (!ids) return;
    await api.openInDiffTool(ids[0], ids[1]);
  }

  async function togglePin(version: VersionInfo) {
    await api.setPinned(version.id, !version.pinned);
    if (fileId !== null) await loadHistory(fileId, true);
  }

  async function renameLabel(version: VersionInfo) {
    const label = window.prompt("Label for this version", version.label ?? "");
    if (label === null) return;
    await api.setLabel(version.id, label.trim().length > 0 ? label.trim() : null);
    if (fileId !== null) await loadHistory(fileId, true);
  }

  if (folders.length === 0) {
    return (
      <div className="empty-state">
        <h2>No folder is watched yet</h2>
        <p>Add a folder in the configuration tab to start collecting file versions.</p>
      </div>
    );
  }

  return (
    <div className="restore-layout">
      <aside className="panel file-panel">
        <div className="panel-head">
          <select value={folderId ?? ""} onChange={(e) => setFolderId(Number(e.target.value))}>
            {folders.map((folder) => (
              <option key={folder.id} value={folder.id}>
                {folder.path}
                {folder.archived ? " (removed, history only)" : ""}
              </option>
            ))}
          </select>
        </div>
        <div className="panel-toolbar">
          <input type="search" placeholder="Search files" value={query} onChange={(e) => setQuery(e.target.value)} />
          <label className="toggle">
            <input type="checkbox" checked={includeDeleted} onChange={(e) => setIncludeDeleted(e.target.checked)} />
            deleted
          </label>
        </div>
        <div className="panel-body">
          {files.length === 0 ? <p className="muted pad">No files match.</p> : null}
          {files.map((file) => (
            <button
              key={file.id}
              className={`file-row ${file.id === fileId ? "selected" : ""} ${file.status === "deleted" ? "deleted" : ""}`}
              onClick={() => setFileId(file.id)}
            >
              <span className="file-name" title={file.relPath}>
                {file.relPath}
              </span>
              <span className="file-meta">
                <span className="muted">{file.versionCount} versions</span>
                <span className="muted">{formatBytes(file.size)}</span>
                {file.status === "deleted" ? <span className="tag danger">deleted</span> : null}
              </span>
            </button>
          ))}
        </div>
        {currentFolder ? (
          <div className="panel-foot column">
            <input
              type="datetime-local"
              value={folderPointInTime}
              onChange={(e) => setFolderPointInTime(e.target.value)}
              title="Point in time for the folder restore (empty = now)"
            />
            <button className="secondary" disabled={busy} onClick={() => withBusy(handleRestoreFolder)}>
              Restore folder to this moment
            </button>
          </div>
        ) : null}
      </aside>

      <section className="panel history-panel">
        <div className="panel-head">
          <span className="title">History</span>
          {versions.length > 0 ? <span className="muted">{versions.length} versions</span> : null}
        </div>
        <div className="panel-body">
          {fileId === null ? <p className="muted pad">Select a file.</p> : null}
          {fileId !== null && versions.length === 0 ? <p className="muted pad">No history for this file yet.</p> : null}
          {versions.map((version) => (
            <div
              key={version.id}
              className={`version-row ${version.id === selectedVersion ? "selected" : ""} ${
                compareIds.includes(version.id) ? "compared" : ""
              }`}
              onClick={() => setSelectedVersion(version.id)}
            >
              <input
                type="checkbox"
                checked={compareIds.includes(version.id)}
                onClick={(e) => e.stopPropagation()}
                onChange={() => toggleCompare(version.id)}
                title="Select for comparison"
              />
              <div className="version-main">
                <div className="version-top">
                  <span className="version-date">{formatDateTime(version.ts)}</span>
                  <span className={`tag ${version.event}`}>{eventLabel(version.event)}</span>
                  {version.pinned ? <span className="tag pin">pinned</span> : null}
                </div>
                <div className="version-bottom">
                  <span className="muted">{relativeTime(version.ts)}</span>
                  <span className="muted">{formatTime(version.ts)}</span>
                  <span className="muted">{formatBytes(version.size)}</span>
                  {version.isText ? (
                    <span className="delta">
                      <span className="plus">+{version.added}</span>
                      <span className="minus">-{version.removed}</span>
                    </span>
                  ) : null}
                  {version.prevPath ? <span className="muted">was {version.prevPath}</span> : null}
                  {version.label ? <span className="tag label">{version.label}</span> : null}
                </div>
              </div>
            </div>
          ))}
        </div>
        {currentVersion ? (
          <div className="panel-foot column">
            <div className="row wrap">
              <button className="primary" disabled={busy} onClick={() => withBusy(() => handleRestoreAs(currentVersion.id))}>
                {currentVersion.event === "deleted" ? "Restore last content as…" : "Restore as…"}
              </button>
              <button
                className="secondary"
                disabled={busy || currentFolder?.archived}
                onClick={() => withBusy(() => handleRestoreInPlace(currentVersion.id))}
              >
                {currentVersion.event === "deleted" ? "Undelete" : "Restore in place"}
              </button>
              <button className="secondary" disabled={busy} onClick={() => withBusy(() => api.openVersion(currentVersion.id))}>
                Open
              </button>
            </div>
            <div className="row wrap">
              <button className="secondary" disabled={compareIds.length !== 2} onClick={() => withBusy(openCompare)}>
                Compare selected
              </button>
              <button className="secondary" disabled={compareIds.length !== 2} onClick={() => withBusy(openExternalDiff)}>
                External diff tool
              </button>
            </div>
            <div className="row wrap">
              <button className="ghost" onClick={() => withBusy(() => togglePin(currentVersion))}>
                {currentVersion.pinned ? "Unpin version" : "Pin version"}
              </button>
              <button className="ghost" onClick={() => withBusy(() => renameLabel(currentVersion))}>
                Label version
              </button>
              <button
                className="ghost"
                disabled={!currentFile || !currentFolder || currentFile.status === "deleted"}
                onClick={() => {
                  if (currentFile && currentFolder) {
                    void api.revealPath(`${currentFolder.path}\\${currentFile.relPath.replace(/\//g, "\\")}`);
                  }
                }}
              >
                Show in Explorer
              </button>
            </div>
          </div>
        ) : null}
      </section>

      <section className="panel preview-panel">
        <div className="panel-head">
          <span className="title">What changed</span>
          {preview ? (
            <span className="muted">
              {preview.kind} · {formatBytes(preview.version.size)}
            </span>
          ) : null}
        </div>
        <div className="panel-body">
          {preview?.version.event === "deleted" ? (
            <div className="diff-binary">
              <div className="binary-card">
                <div className="binary-title">File deleted</div>
                <p className="muted">
                  {preview.version.relPath} was deleted on {formatDateTime(preview.version.ts)}. Its last content (
                  {formatBytes(preview.version.size)}) is kept: use “Undelete” to put it back or “Restore last content as…”
                  to save a copy.
                </p>
              </div>
            </div>
          ) : preview?.diff ? (
            <DiffView result={preview.diff} />
          ) : preview?.isText ? (
            <div className="diff-binary">
              <div className="binary-card">
                <div className="binary-title">First known version</div>
                <pre className="unified">{preview.text ?? ""}</pre>
              </div>
            </div>
          ) : preview ? (
            <div className="diff-binary">
              <div className="binary-card">
                <div className="binary-title">{preview.kind} file</div>
                <p className="muted">This file type has no text preview, but every version is stored and can be restored.</p>
                <pre className="unified">{preview.version.snippet ?? ""}</pre>
              </div>
            </div>
          ) : (
            <div className="diff-empty">Select a file and a version to see what changed.</div>
          )}
        </div>
      </section>

      {showCompare && compare ? (
        <div className="modal-backdrop" onClick={() => setShowCompare(false)}>
          <div className="modal" onClick={(e) => e.stopPropagation()}>
            <div className="modal-head">
              <span className="title">Compare versions</span>
              <button className="ghost" onClick={() => setShowCompare(false)}>
                close
              </button>
            </div>
            <div className="modal-body">
              <DiffView result={compare} />
            </div>
          </div>
        </div>
      ) : null}
    </div>
  );
}
