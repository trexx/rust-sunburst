// SPDX-License-Identifier: GPL-2.0-or-later

import { useCallback, useEffect, useState } from "react";
import { api, formatDate } from "../api";
import type { Session } from "../api";

export function Sessions() {
  const [sessions, setSessions] = useState<Session[]>([]);
  const [error, setError] = useState<string | null>(null);

  const refresh = useCallback(async () => {
    try {
      setSessions(await api.get<Session[]>("/api/sessions"));
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  }, []);

  useEffect(() => {
    void refresh();
    const timer = setInterval(() => void refresh(), 2000);
    return () => clearInterval(timer);
  }, [refresh]);

  async function disconnect(id: number) {
    setError(null);
    try {
      await api.del(`/api/sessions/${id}`);
      await refresh();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  }

  return (
    <section className="card">
      <h2>Active sessions</h2>
      {sessions.length === 0 && (
        <p className="hint">
          Nothing streaming. The streaming pipeline does not exist yet — this
          list is empty rather than invented.
        </p>
      )}
      {sessions.length > 0 && (
        <table>
          <thead>
            <tr>
              <th>Client</th>
              <th>Codec</th>
              <th>Mode</th>
              <th>Bitrate</th>
              <th>Started</th>
              <th />
            </tr>
          </thead>
          <tbody>
            {sessions.map((s) => (
              <tr key={s.id}>
                <td>{s.client_name}</td>
                <td>{s.codec}</td>
                <td>
                  {s.width}×{s.height} @ {s.fps}
                </td>
                <td>{(s.bitrate_kbps / 1000).toFixed(0)} Mbps</td>
                <td>{formatDate(s.started_at)}</td>
                <td>
                  <button className="danger" onClick={() => void disconnect(s.id)}>
                    Disconnect
                  </button>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
      {error && <p className="error">{error}</p>}
    </section>
  );
}
