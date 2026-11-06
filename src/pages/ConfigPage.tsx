import { useCallback, useEffect, useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import { api } from "../api";
import { RETENTION_DEFAULTS, RetentionSection } from "../components/RetentionSection";
import type { AppSettings, FolderInfo, StorageStats } from "../types";
import { formatBytes } from "../utils";

type Props = {
  folders: FolderInfo[];
  archivedFolders: FolderInfo[];
  settingsTick: number;
  onFoldersChanged: () => void;
  notify: (message: string, kind?: "info" | "error") => void;
};

const emptySettings: AppSettings = {
  ...RETENTION_DEFAULTS,
  dataDir: "",
  defaultMaxFileMb: 128,
  defaultExcludeGlobs: "",
  autostart: false,
  externalDiffTool: "",
  reconcileIntervalMinutes: 10,
  paused: false,
  startMinimized: true,
  showDeleted: true,
};

type FolderDraft = {
  retentionDays: string;
  maxFileMb: string;
  excludeGlobs: string;
  thinning: boolean;
};

function draftOf(folder: FolderInfo): FolderDraft {
  return {
    retentionDays: String(folder.retentionDays),
    maxFileMb: String(folder.maxFileMb),
    excludeGlobs: folder.excludeGlobs,
    thinning: folder.thinning,
  };
}

function FolderCard({
  folder,
  onSaved,
  onRemove,
  notify,
}: {
  folder: FolderInfo;
  onSaved: () => void;
  onRemove: (folder: FolderInfo) => void;
  notify: Props["notify"];
}) {
  const [draft, setDraft] = useState<FolderDraft>(() => draftOf(folder));
  const [saving, setSaving] = useState(false);

  useEffect(() => {
    setDraft(draftOf(folder));
  }, [folder.id, folder.retentionDays, folder.maxFileMb, folder.excludeGlobs, folder.thinning]);

  const retention = Number.parseInt(draft.retentionDays, 10);
  const maxMb = Number.parseInt(draft.maxFileMb, 10);
  const valid = Number.isFinite(retention) && retention >= 1 && Number.isFinite(maxMb) && maxMb >= 1;
  const dirty =
    draft.retentionDays !== String(folder.retentionDays) ||
    draft.maxFileMb !== String(folder.maxFileMb) ||
    draft.excludeGlobs !== folder.excludeGlobs ||
    draft.thinning !== folder.thinning;

  async function send(enabled: boolean, paused: boolean, values: { retention: number; thinning: boolean; globs: string; maxMb: number }) {
    setSaving(true);
    try {
      await api.updateFolder(folder.id, enabled, values.retention, values.thinning, values.globs, values.maxMb, paused);
      onSaved();
    } catch (error) {
      notify(String(error), "error");
    } finally {
      setSaving(false);
    }
  }

  async function toggle(changes: { enabled?: boolean; paused?: boolean }) {
    await send(changes.enabled ?? folder.enabled, changes.paused ?? folder.paused, {
      retention: folder.retentionDays,
      thinning: folder.thinning,
      globs: folder.excludeGlobs,
      maxMb: folder.maxFileMb,
    });
  }

  async function save() {
    if (!valid) {
      notify("Retention and maximum size must be whole numbers of at least 1", "error");
      return;
    }
    if (retention < folder.retentionDays) {
      if (!window.confirm(`Shorten retention from ${folder.retentionDays} to ${retention} days? Older versions will be deleted at the next cleanup.`)) {
        return;
      }
    }
    await send(folder.enabled, folder.paused, {
      retention,
      thinning: draft.thinning,
      globs: draft.excludeGlobs,
      maxMb,
    });
  }

  return (
    <div className="folder-card">
      <div className="folder-title">
        <label className="toggle">
          <input type="checkbox" checked={folder.enabled} disabled={saving} onChange={(e) => toggle({ enabled: e.target.checked })} />
          monitored
        </label>
        <span className="folder-path" title={folder.path}>
          {folder.path}
        </span>
        <button className="ghost danger" onClick={() => onRemove(folder)}>
          remove
        </button>
      </div>
      <div className="folder-grid">
        <label>
          <span>Keep versions for (days, this folder)</span>
          <input
            type="number"
            min={1}
            value={draft.retentionDays}
            onChange={(e) => setDraft({ ...draft, retentionDays: e.target.value })}
          />
        </label>
        <label>
          <span>Maximum file size (MB)</span>
          <input type="number" min={1} value={draft.maxFileMb} onChange={(e) => setDraft({ ...draft, maxFileMb: e.target.value })} />
        </label>
        <label className="checkbox">
          <input type="checkbox" checked={draft.thinning} onChange={(e) => setDraft({ ...draft, thinning: e.target.checked })} />
          <span>Smart thinning (this folder)</span>
        </label>
        <label className="checkbox">
          <input type="checkbox" checked={folder.paused} disabled={saving} onChange={(e) => toggle({ paused: e.target.checked })} />
          <span>Pause this folder</span>
        </label>
        <label className="wide">
          <span>Ignored patterns (one per line, e.g. **/*.log)</span>
          <textarea rows={2} value={draft.excludeGlobs} onChange={(e) => setDraft({ ...draft, excludeGlobs: e.target.value })} />
        </label>
      </div>
      <div className="folder-stats muted row wrap">
        <span>
          {folder.fileCount} files · {folder.versionCount} versions · {formatBytes(folder.storedBytes)} stored of{" "}
          {formatBytes(folder.rawBytes)} original
        </span>
        {dirty ? (
          <span className="row">
            <button className="primary" disabled={saving || !valid} onClick={() => save()}>
              Save folder
            </button>
            <button className="ghost" disabled={saving} onClick={() => setDraft(draftOf(folder))}>
              discard
            </button>
          </span>
        ) : null}
      </div>
    </div>
  );
}

export function ConfigPage({ folders, archivedFolders, settingsTick, onFoldersChanged, notify }: Props) {
  const [settings, setSettings] = useState<AppSettings>(emptySettings);
  const [stats, setStats] = useState<StorageStats | null>(null);
  const [excludes, setExcludes] = useState<string[]>([]);
  const [busy, setBusy] = useState(false);

  const load = useCallback(async () => {
    const current = await api.getSettings();
    setSettings({ ...emptySettings, ...current });
    setExcludes(await api.defaultExcludes());
    setStats(await api.getStats());
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  useEffect(() => {
    if (settingsTick === 0) return;
    void api.getSettings().then((current) =>
      setSettings((local) => ({ ...local, paused: current.paused, autostart: current.autostart })),
    );
  }, [settingsTick]);

  async function run(action: () => Promise<void>) {
    setBusy(true);
    try {
      await action();
    } catch (error) {
      notify(String(error), "error");
    } finally {
      setBusy(false);
    }
  }

  async function addFolder() {
    const picked = await open({ directory: true, multiple: false, title: "Choose a folder to watch" });
    if (!picked || Array.isArray(picked)) return;
    await api.addFolder(
      picked,
      settings.defaultRetentionDays,
      settings.defaultThinning,
      settings.defaultExcludeGlobs,
      settings.defaultMaxFileMb,
    );
    notify(`Watching ${picked}`);
    onFoldersChanged();
    await load();
  }

  async function removeFolder(folder: FolderInfo) {
    if (!window.confirm(`Stop monitoring ${folder.path}?`)) return;
    const deleteHistory = window.confirm(
      `Also delete the stored history of this folder?\n\nOK = delete history permanently\nCancel = keep the history (still restorable from the Restore tab${
        settings.cleanRemovedFolders ? "; retention rules still apply" : ""
      })`,
    );
    if (deleteHistory && !window.confirm("Really delete all stored versions of this folder? This cannot be undone.")) {
      return;
    }
    await api.removeFolder(folder.id, deleteHistory);
    notify(deleteHistory ? "Folder and history removed" : "Folder removed, history kept");
    onFoldersChanged();
    await load();
  }

  async function resumeFolder(folder: FolderInfo) {
    await api.resumeFolder(folder.id);
    notify(`Watching ${folder.path} again`);
    onFoldersChanged();
    await load();
  }

  async function deleteArchived(folder: FolderInfo) {
    if (
      !window.confirm(
        `Permanently delete all ${folder.versionCount} stored versions of ${folder.path}? This cannot be undone.`,
      )
    ) {
      return;
    }
    await api.removeFolder(folder.id, true);
    notify("History deleted");
    onFoldersChanged();
    await load();
  }

  async function saveSettings() {
    const saved = await api.saveSettings(settings);
    setSettings({ ...emptySettings, ...saved, autostart: settings.autostart });
    notify("Settings saved");
  }

  async function togglePause(paused: boolean) {
    await api.setPaused(paused);
    setSettings((current) => ({ ...current, paused }));
    notify(paused ? "Monitoring paused" : "Monitoring resumed");
  }

  async function toggleAutostart(enabled: boolean) {
    const actual = await api.setAutostart(enabled);
    setSettings((current) => ({ ...current, autostart: actual }));
    notify(actual ? "The app will start with Windows" : "Autostart disabled");
  }

  return (
    <div className="config-layout">
      <section className="card">
        <div className="card-head">
          <h2>Watched folders</h2>
          <button className="primary" onClick={() => run(addFolder)} disabled={busy}>
            Add folder
          </button>
        </div>
        {folders.length === 0 ? <p className="muted pad">No folder is watched yet.</p> : null}
        <div className="folder-list">
          {folders.map((folder) => (
            <FolderCard
              key={folder.id}
              folder={folder}
              notify={notify}
              onSaved={onFoldersChanged}
              onRemove={(f) => void run(() => removeFolder(f))}
            />
          ))}
        </div>
      </section>

      {archivedFolders.length > 0 ? (
        <section className="card">
          <div className="card-head">
            <h2>Removed folders (history only)</h2>
          </div>
          <p className="muted pad">
            These folders are no longer watched. Their history is kept and restorable from the Restore tab.{" "}
            {settings.cleanRemovedFolders
              ? "Retention rules still apply to them (see Retention)."
              : "It is not cleaned up unless you enable that in the Retention section."}
          </p>
          <div className="folder-list">
            {archivedFolders.map((folder) => (
              <div className="folder-card" key={folder.id}>
                <div className="folder-title">
                  <span className="folder-path" title={folder.path}>
                    {folder.path}
                  </span>
                  <button className="secondary" disabled={busy} onClick={() => run(() => resumeFolder(folder))}>
                    Resume watching
                  </button>
                  <button className="ghost danger" disabled={busy} onClick={() => run(() => deleteArchived(folder))}>
                    Delete history permanently
                  </button>
                </div>
                <div className="folder-stats muted">
                  {folder.fileCount} files · {folder.versionCount} versions · {formatBytes(folder.storedBytes)} stored ·
                  keeps {folder.retentionDays} days
                </div>
              </div>
            ))}
          </div>
        </section>
      ) : null}

      <RetentionSection
        settings={settings}
        onChange={setSettings}
        onSave={saveSettings}
        onFoldersChanged={() => {
          onFoldersChanged();
          void load();
        }}
        notify={notify}
      />

      <section className="card">
        <div className="card-head">
          <h2>Application</h2>
        </div>
        <div className="folder-grid pad">
          <label className="checkbox">
            <input type="checkbox" checked={settings.paused} disabled={busy} onChange={(e) => run(() => togglePause(e.target.checked))} />
            <span>Pause all monitoring</span>
          </label>
          <label className="checkbox">
            <input
              type="checkbox"
              checked={settings.autostart}
              disabled={busy}
              onChange={(e) => run(() => toggleAutostart(e.target.checked))}
            />
            <span>Start with Windows</span>
          </label>
          <label className="checkbox">
            <input
              type="checkbox"
              checked={settings.startMinimized}
              onChange={(e) => setSettings({ ...settings, startMinimized: e.target.checked })}
            />
            <span>Start hidden in the tray</span>
          </label>
          <label className="checkbox">
            <input
              type="checkbox"
              checked={settings.showDeleted}
              onChange={(e) => setSettings({ ...settings, showDeleted: e.target.checked })}
            />
            <span>Show deleted files in Restore by default</span>
          </label>
          <label>
            <span>Default maximum file size (MB)</span>
            <input
              type="number"
              min={1}
              value={settings.defaultMaxFileMb}
              onChange={(e) => setSettings({ ...settings, defaultMaxFileMb: Math.max(1, Number(e.target.value) || 1) })}
            />
          </label>
          <label>
            <span>Re-scan interval (minutes)</span>
            <input
              type="number"
              min={1}
              value={settings.reconcileIntervalMinutes}
              onChange={(e) =>
                setSettings({ ...settings, reconcileIntervalMinutes: Math.max(1, Number(e.target.value) || 1) })
              }
            />
          </label>
          <label className="wide">
            <span>Default ignored patterns for new folders (one per line)</span>
            <textarea
              rows={2}
              value={settings.defaultExcludeGlobs}
              onChange={(e) => setSettings({ ...settings, defaultExcludeGlobs: e.target.value })}
            />
          </label>
          <label className="wide">
            <span>External diff tool (full path to the .exe)</span>
            <input
              type="text"
              placeholder="C:\\Program Files\\WinMerge\\WinMergeU.exe"
              value={settings.externalDiffTool}
              onChange={(e) => setSettings({ ...settings, externalDiffTool: e.target.value })}
            />
          </label>
          <label className="wide">
            <span>History location</span>
            <input type="text" value={settings.dataDir} readOnly />
          </label>
        </div>
        <div className="card-foot">
          <button className="primary" onClick={() => run(saveSettings)} disabled={busy}>
            Save settings
          </button>
        </div>
      </section>

      <section className="card">
        <div className="card-head">
          <h2>Storage</h2>
        </div>
        {stats ? (
          <div className="stat-grid">
            <div>
              <span className="label">Folders</span>
              <strong>{stats.folders}</strong>
            </div>
            <div>
              <span className="label">Files</span>
              <strong>{stats.files}</strong>
            </div>
            <div>
              <span className="label">Versions</span>
              <strong>{stats.versions}</strong>
            </div>
            <div>
              <span className="label">Blobs</span>
              <strong>{stats.blobs}</strong>
            </div>
            <div>
              <span className="label">Original size</span>
              <strong>{formatBytes(stats.rawBytes)}</strong>
            </div>
            <div>
              <span className="label">Stored size</span>
              <strong>{formatBytes(stats.storedBytes)}</strong>
            </div>
            <div>
              <span className="label">Database</span>
              <strong>{formatBytes(stats.dbBytes)}</strong>
            </div>
          </div>
        ) : null}
        <div className="card-foot wrap">
          <button
            className="secondary"
            disabled={busy}
            onClick={() =>
              run(async () => {
                await api.snapshotAll();
                notify("Snapshot started in the background");
              })
            }
          >
            Snapshot all folders now
          </button>
          <button
            className="secondary"
            disabled={busy}
            onClick={() =>
              run(async () => {
                await api.rescanAll();
                notify("Re-scan started in the background");
              })
            }
          >
            Re-scan all folders
          </button>
          <button
            className="secondary"
            disabled={busy}
            onClick={() =>
              run(async () => {
                const problems = await api.checkIntegrity();
                if (problems.length === 0) {
                  notify("The history store is consistent");
                } else {
                  notify(problems.slice(0, 5).join("; ") + (problems.length > 5 ? ` (+${problems.length - 5} more)` : ""), "error");
                }
              })
            }
          >
            Check integrity
          </button>
          <button className="ghost" disabled={busy} onClick={() => run(load)}>
            Refresh
          </button>
        </div>
      </section>

      <section className="card">
        <div className="card-head">
          <h2>Ignored by default</h2>
        </div>
        <p className="muted pad">These patterns are never tracked. Add your own per folder in the folder card above.</p>
        <pre className="unified pad">{excludes.join("\n")}</pre>
      </section>
    </div>
  );
}
