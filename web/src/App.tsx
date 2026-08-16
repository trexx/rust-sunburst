// SPDX-License-Identifier: GPL-2.0-or-later

import { useCallback, useEffect, useState } from "react";
import "./App.css";
import { ApiError, api, clearToken, getToken, setToken } from "./api";
import type { Status } from "./api";
import { StatusPanel } from "./components/StatusPanel";
import { Clients } from "./components/Clients";
import { Sessions } from "./components/Sessions";
import { Apps } from "./components/Apps";
import { SettingsPanel } from "./components/SettingsPanel";

// No router: five panels on one machine does not need history integration, and
// the project's hard rule is no router library.
const TABS = ["Status", "Clients", "Sessions", "Apps", "Settings"] as const;
type Tab = (typeof TABS)[number];

export function App() {
  const [authed, setAuthed] = useState(false);
  const [checking, setChecking] = useState(true);
  const [tab, setTab] = useState<Tab>("Status");
  const [status, setStatus] = useState<Status | null>(null);

  const refreshStatus = useCallback(async () => {
    try {
      setStatus(await api.get<Status>("/api/status"));
      setAuthed(true);
    } catch (e) {
      if (e instanceof ApiError && e.isAuth) setAuthed(false);
    } finally {
      setChecking(false);
    }
  }, []);

  useEffect(() => {
    void refreshStatus();
    // Status is cheap and drives the header, so poll it. Panels refresh their
    // own data when shown rather than all of them polling at once.
    const timer = setInterval(() => void refreshStatus(), 5000);
    return () => clearInterval(timer);
  }, [refreshStatus]);

  if (checking) return <div className="loading">Connecting…</div>;
  if (!authed) return <TokenGate onAuthed={() => void refreshStatus()} />;

  return (
    <div className="app">
      <header>
        <h1>Sunburst</h1>
        <nav>
          {TABS.map((t) => (
            <button
              key={t}
              className={t === tab ? "tab active" : "tab"}
              onClick={() => setTab(t)}
            >
              {t}
            </button>
          ))}
        </nav>
        <button
          className="link"
          onClick={() => {
            clearToken();
            setAuthed(false);
          }}
        >
          Forget token
        </button>
      </header>

      {status?.lan_exposed && (
        <p className="banner warning">
          This interface is reachable from the network. The token is the only
          thing standing between it and anyone else on the LAN.
        </p>
      )}

      <main>
        {tab === "Status" && (
          <StatusPanel status={status} onChange={refreshStatus} />
        )}
        {tab === "Clients" && <Clients onChange={refreshStatus} />}
        {tab === "Sessions" && <Sessions />}
        {tab === "Apps" && <Apps status={status} onChange={refreshStatus} />}
        {tab === "Settings" && <SettingsPanel />}
      </main>
    </div>
  );
}

function TokenGate({ onAuthed }: { onAuthed: () => void }) {
  const [value, setValue] = useState(getToken());
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  async function submit(e: React.FormEvent) {
    e.preventDefault();
    setBusy(true);
    setError(null);
    setToken(value.trim());
    try {
      await api.get<Status>("/api/status");
      onAuthed();
    } catch (err) {
      clearToken();
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(false);
    }
  }

  return (
    <form className="gate" onSubmit={(e) => void submit(e)}>
      <h1>Sunburst</h1>
      <p>
        The server prints its token at startup, and it is in{" "}
        <code>config.json</code> beside it.
      </p>
      <input
        type="password"
        placeholder="token"
        value={value}
        autoFocus
        onChange={(e) => setValue(e.target.value)}
      />
      <button type="submit" disabled={busy || value.trim() === ""}>
        {busy ? "Checking…" : "Unlock"}
      </button>
      {error && <p className="error">{error}</p>}
    </form>
  );
}
