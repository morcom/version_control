import { useState } from "react";
import { api } from "../api";
import type { AppSettings, RetentionPreview } from "../types";
import { formatBytes } from "../utils";

type Props = {
  settings: AppSettings;
  onChange: (settings: AppSettings) => void;
  onSave: () => Promise<void>;
  onFoldersChanged: () => void;
  notify: (message: string, kind?: "info" | "error") => void;
};

export const RETENTION_DEFAULTS = {
  defaultRetentionDays: 90,
  defaultThinning: true,
  keepAllHours: 24,
  hourlyUntilDays: 7,
  dailyUntilDays: 30,
  maxVersionsPerFile: 0,
  cleanupIntervalHours: 1,
  cleanRemovedFolders: false,
} satisfies Partial<AppSettings>;

function NumberField({
  label,
  hint,
  value,
  min,
  onChange,
}: {
  label: string;
  hint?: string;
  value: number;
  min: number;
  onChange: (value: number) => void;
}) {
  return (
    <label>
      <span>{label}</span>
      <input
        type="number"
        min={min}
        value={Number.isFinite(value) ? value : min}
        onChange={(e) => onChange(Math.max(min, Number(e.target.value) || min))}
      />
      {hint ? <small className="hint">{hint}</small> : null}
    </label>
  );
}

export function RetentionSection({ settings, onChange, onSave, onFoldersChanged, notify }: Props) {
  const [preview, setPreview] = useState<RetentionPreview | null>(null);
  const [busy, setBusy] = useState(false);

  const set = <K extends keyof AppSettings>(key: K, value: AppSettings[K]) => {
    setPreview(null);
    onChange({ ...settings, [key]: value });
  };

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

  const retention = settings.defaultRetentionDays;
  const tiers = settings.defaultThinning
    ? [
        { label: "Every save", until: `${settings.keepAllHours} h` },
        { label: "1 per hour", until: `${settings.hourlyUntilDays} d` },
        { label: "1 per day", until: `${settings.dailyUntilDays} d` },
        { label: "1 per week", until: `${retention} d` },
      ]
    : [{ label: "Every save", until: `${retention} d` }];

  return (
    <section className="card">
      <div className="card-head">
        <h2>Retention</h2>
        <button
          className="ghost"
          onClick={() => onChange({ ...settings, ...RETENTION_DEFAULTS })}
          title="Restore the default retention values"
        >
          reset to defaults
        </button>
      </div>

      <div className="pad muted">
        Versions older than the retention period are deleted. The newest version of each file and pinned versions
        are always kept. Smart thinning keeps every save for a while and then fewer versions as they get older.
      </div>

      <div className="timeline">
        {tiers.map((tier, index) => (
          <div className="tier" key={tier.label} style={{ flex: index === 0 ? 1 : 1.4 }}>
            <span className="tier-label">{tier.label}</span>
            <span className="tier-until">until {tier.until}</span>
          </div>
        ))}
        <div className="tier expired">
          <span className="tier-label">Deleted</span>
          <span className="tier-until">after {retention} d</span>
        </div>
      </div>

      <div className="folder-grid pad">
        <NumberField
          label="Keep versions for (days)"
          hint="Default for new folders"
          value={settings.defaultRetentionDays}
          min={1}
          onChange={(v) => set("defaultRetentionDays", v)}
        />
        <label className="checkbox">
          <input
            type="checkbox"
            checked={settings.defaultThinning}
            onChange={(e) => set("defaultThinning", e.target.checked)}
          />
          <span>Smart thinning (default for new folders)</span>
        </label>
        <NumberField
          label="Keep every save for (hours)"
          value={settings.keepAllHours}
          min={0}
          onChange={(v) => set("keepAllHours", v)}
        />
        <NumberField
          label="Then one per hour until (days)"
          value={settings.hourlyUntilDays}
          min={0}
          onChange={(v) => set("hourlyUntilDays", v)}
        />
        <NumberField
          label="Then one per day until (days)"
          hint="Older versions: one per week"
          value={settings.dailyUntilDays}
          min={0}
          onChange={(v) => set("dailyUntilDays", v)}
        />
        <NumberField
          label="Maximum versions per file"
          hint="0 = unlimited"
          value={settings.maxVersionsPerFile}
          min={0}
          onChange={(v) => set("maxVersionsPerFile", v)}
        />
        <NumberField
          label="Run cleanup every (hours)"
          value={settings.cleanupIntervalHours}
          min={1}
          onChange={(v) => set("cleanupIntervalHours", v)}
        />
        <label className="checkbox wide">
          <input
            type="checkbox"
            checked={settings.cleanRemovedFolders}
            onChange={(e) => {
              if (
                e.target.checked &&
                !window.confirm(
                  "Also apply retention to removed folders? Their older versions will then be deleted at the next cleanup, like for watched folders.",
                )
              ) {
                return;
              }
              set("cleanRemovedFolders", e.target.checked);
            }}
          />
          <span>Also clean up the history of removed folders (off: their history is kept untouched)</span>
        </label>
      </div>

      {preview ? (
        <div className="pad">
          <div className="preview-box">
            <strong>
              {preview.removableVersions} of {preview.totalVersions} versions would be removed (
              {formatBytes(preview.removableBytes)} freed)
            </strong>
            {preview.folders.map((folder) => (
              <div key={folder.folderId} className="muted preview-line">
                <span className="folder-path">{folder.path}</span>
                <span>
                  {folder.removableVersions} / {folder.totalVersions}
                </span>
              </div>
            ))}
          </div>
        </div>
      ) : null}

      <div className="card-foot wrap">
        <button className="primary" disabled={busy} onClick={() => run(onSave)}>
          Save retention
        </button>
        <button
          className="secondary"
          disabled={busy}
          onClick={() =>
            run(async () => {
              await onSave();
              setPreview(await api.previewCleanup());
            })
          }
        >
          Preview cleanup
        </button>
        <button
          className="secondary"
          disabled={busy}
          onClick={() =>
            run(async () => {
              if (
                !window.confirm(
                  `Set ${settings.defaultRetentionDays} days and smart thinning ${
                    settings.defaultThinning ? "on" : "off"
                  } for all watched folders?`,
                )
              ) {
                return;
              }
              await api.applyRetentionToAll(settings.defaultRetentionDays, settings.defaultThinning);
              onFoldersChanged();
              notify("Retention applied to all folders");
            })
          }
        >
          Apply to all folders
        </button>
        <button
          className="ghost danger"
          disabled={busy}
          onClick={() =>
            run(async () => {
              await onSave();
              const report = await api.runCleanup();
              setPreview(null);
              notify(`Cleanup removed ${report.removedVersions} versions and freed ${formatBytes(report.freedBytes)}`);
              onFoldersChanged();
            })
          }
        >
          Run cleanup now
        </button>
      </div>
    </section>
  );
}
