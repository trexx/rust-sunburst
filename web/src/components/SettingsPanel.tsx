// SPDX-License-Identifier: GPL-2.0-or-later

import { useCallback, useEffect, useState } from "react";
import { api, setToken } from "../api";
import type { Settings } from "../api";

export function SettingsPanel() {
  const [settings, setSettings] = useState<Settings | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [saved, setSaved] = useState(false);

  const refresh = useCallback(async () => {
    try {
      setSettings(await api.get<Settings>("/api/config"));
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  }, []);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  async function save() {
    if (!settings) return;
    setError(null);
    setSaved(false);
    try {
      const applied = await api.put<Settings>("/api/config", settings);
      setSettings(applied);
      // Changing the token takes effect immediately on the server, so the
      // browser has to follow or the very next request is a 401.
      setToken(applied.web.token);
      setSaved(true);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  }

  if (!settings) return <p>Loading…</p>;

  const lanExposed = !(
    settings.web.bind.startsWith("127.") || settings.web.bind === "::1"
  );

  return (
    <>
      <section className="card">
        <h2>Web interface</h2>
        <label>
          Bind address
          <input
            value={settings.web.bind}
            onChange={(e) =>
              setSettings({ ...settings, web: { ...settings.web, bind: e.target.value } })
            }
          />
        </label>
        <label>
          Port
          <input
            inputMode="numeric"
            value={settings.web.port}
            onChange={(e) =>
              setSettings({
                ...settings,
                web: { ...settings.web, port: Number(e.target.value) || 0 },
              })
            }
          />
        </label>
        <label>
          Token
          <input
            value={settings.web.token}
            onChange={(e) =>
              setSettings({ ...settings, web: { ...settings.web, token: e.target.value } })
            }
          />
        </label>

        {lanExposed && (
          <p className="banner warning">
            A non-loopback address exposes this interface to the LAN. There is no
            TLS: the token is a shared-secret gate, not confidentiality on the
            wire. The server refuses to start with an empty token here.
          </p>
        )}

        <label>
          Assets directory
          <input
            value={settings.web.assets_dir}
            onChange={(e) =>
              setSettings({
                ...settings,
                web: { ...settings.web, assets_dir: e.target.value },
              })
            }
          />
        </label>
      </section>

      <section className="card">
        <h2>Stream defaults</h2>
        <p className="hint">
          Applied unless an application overrides them.
        </p>
        <label>
          UDP port
          <input
            inputMode="numeric"
            value={settings.stream.port}
            onChange={(e) =>
              setSettings({
                ...settings,
                stream: { ...settings.stream, port: Number(e.target.value) || 0 },
              })
            }
          />
        </label>
        <label>
          Bitrate (kbps)
          <input
            inputMode="numeric"
            value={settings.stream.bitrate_kbps}
            onChange={(e) =>
              setSettings({
                ...settings,
                stream: {
                  ...settings.stream,
                  bitrate_kbps: Number(e.target.value) || 0,
                },
              })
            }
          />
        </label>
        <label>
          Codec
          <select
            value={settings.stream.codec}
            onChange={(e) =>
              setSettings({
                ...settings,
                stream: { ...settings.stream, codec: e.target.value },
              })
            }
          >
            <option value="auto">Auto</option>
            <option value="hevc">HEVC</option>
            <option value="av1">AV1</option>
          </select>
        </label>
        <p className="hint">
          Auto is the only setting that works for both devices: the Shield has no
          AV1 decoder and the Homatics' HEVC decoder is broken.
        </p>
      </section>

      <div className="row">
        <button onClick={() => void save()}>Save</button>
        {saved && <span className="hint">Saved.</span>}
      </div>
      {error && <p className="error">{error}</p>}
    </>
  );
}
