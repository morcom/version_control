import { useCallback, useEffect, useRef, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { api } from "./api";
import { ConfigPage } from "./pages/ConfigPage";
import { RestorePage } from "./pages/RestorePage";
import type { FolderInfo } from "./types";
import "./App.css";

type Tab = "restore" | "config";
type Notice = { id: number; message: string; kind: "info" | "error" };

function App() {
  const [tab, setTab] = useState<Tab>("restore");
  const [allFolders, setAllFolders] = useState<FolderInfo[]>([]);
  const [notices, setNotices] = useState<Notice[]>([]);
  const [historyTick, setHistoryTick] = useState(0);
  const [settingsTick, setSettingsTick] = useState(0);
  const refreshTimer = useRef<number | undefined>(undefined);

  const refreshFolders = useCallback(async () => {
    setAllFolders(await api.listFolders());
  }, []);

  useEffect(() => {
    void refreshFolders();
  }, [refreshFolders]);

  useEffect(() => {
    let missedWhileHidden = false;
    const refresh = () => {
      void refreshFolders();
      setHistoryTick((n) => n + 1);
    };
    const stopHistory = listen("history-changed", () => {
      if (document.visibilityState === "hidden") {
        missedWhileHidden = true;
        return;
      }
      window.clearTimeout(refreshTimer.current);
      refreshTimer.current = window.setTimeout(refresh, 1000);
    });
    const onVisible = () => {
      if (document.visibilityState === "visible" && missedWhileHidden) {
        missedWhileHidden = false;
        refresh();
      }
    };
    document.addEventListener("visibilitychange", onVisible);
    const stopSettings = listen("settings-changed", () => setSettingsTick((n) => n + 1));
    return () => {
      document.removeEventListener("visibilitychange", onVisible);
      window.clearTimeout(refreshTimer.current);
      void stopHistory.then((stop) => stop());
      void stopSettings.then((stop) => stop());
    };
  }, [refreshFolders]);

  const notify = useCallback((message: string, kind: "info" | "error" = "info") => {
    const id = Date.now() + Math.random();
    setNotices((current) => [...current, { id, message, kind }]);
    window.setTimeout(() => {
      setNotices((current) => current.filter((notice) => notice.id !== id));
    }, 6000);
  }, []);

  const folders = allFolders.filter((f) => !f.archived);

  return (
    <div className="app">
      <header className="topbar">
        <div className="brand">
          <span className="logo">◍</span>
          <div>
            <div className="brand-title">File History Tracker</div>
            <div className="brand-sub">Automatic version control for your folders</div>
          </div>
        </div>
        <nav className="tabs">
          <button className={tab === "restore" ? "active" : ""} onClick={() => setTab("restore")}>
            Restore
          </button>
          <button className={tab === "config" ? "active" : ""} onClick={() => setTab("config")}>
            Configuration
          </button>
        </nav>
        <div className="topbar-right">
          <span className="muted">{folders.length} folders watched</span>
        </div>
      </header>

      <main className="content">
        {tab === "restore" ? (
          <RestorePage folders={allFolders} historyTick={historyTick} settingsTick={settingsTick} notify={notify} />
        ) : (
          <ConfigPage
            folders={folders}
            archivedFolders={allFolders.filter((f) => f.archived)}
            settingsTick={settingsTick}
            onFoldersChanged={refreshFolders}
            notify={notify}
          />
        )}
      </main>

      <div className="notices">
        {notices.map((notice) => (
          <div key={notice.id} className={`notice ${notice.kind}`}>
            {notice.message}
          </div>
        ))}
      </div>
    </div>
  );
}

export default App;
