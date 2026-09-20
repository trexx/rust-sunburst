// SPDX-License-Identifier: GPL-2.0-or-later

import { useCallback, useEffect, useState } from "react";
import { api } from "../api";
import type { AppEntry, Status } from "../api";

const BLANK: AppEntry = {
  id: 0,
  name: "",
  exe: "",
  args: [],
  working_dir: null,
  prep: [],
  overrides: {
    bitrate_kbps: null,
    codec: null,
    width: null,
    height: null,
    fps: null,
    preset: null,
  },
};

export function Apps({
  status,
  onChange,
}: {
  status: Status | null;
  onChange: () => void;
}) {
  const [apps, setApps] = useState<AppEntry[]>([]);
  const [editing, setEditing] = useState<AppEntry | null>(null);
  const [error, setError] = useState<string | null>(null);

  const refresh = useCallback(async () => {
    try {
      setApps(await api.get<AppEntry[]>("/api/apps"));
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  }, []);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  async function act(fn: () => Promise<unknown>) {
    setError(null);
    try {
      await fn();
      await refresh();
      onChange();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  }

  const running = status?.running_app ?? null;

  return (
    <>
      <section className="card">
        <h2>Applications</h2>
        <p className="hint">
          Entries are added by hand — nothing is scanned. Big Picture is an
          ordinary entry: <code>steam://open/bigpicture</code>.
        </p>

        {apps.length === 0 && <p className="hint">Nothing configured yet.</p>}
        {apps.length > 0 && (
          <table>
            <thead>
              <tr>
                <th>Name</th>
                <th>Command</th>
                <th />
              </tr>
            </thead>
            <tbody>
              {apps.map((a) => (
                <tr key={a.id}>
                  <td>
                    {a.name}
                    {running?.app_id === a.id && (
                      <span className="hint"> — running (pid {running.pid})</span>
                    )}
                  </td>
                  <td>
                    <code>{a.exe}</code>
                  </td>
                  <td className="row">
                    <button
                      onClick={() => void act(() => api.post(`/api/apps/${a.id}/launch`))}
                      disabled={running !== null}
                    >
                      Launch
                    </button>
                    <button onClick={() => setEditing(a)}>Edit</button>
                    <button
                      className="danger"
                      onClick={() => void act(() => api.del(`/api/apps/${a.id}`))}
                    >
                      Delete
                    </button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}

        <div className="row">
          <button onClick={() => setEditing({ ...BLANK })}>Add application</button>
          {running && (
            <button
              className="danger"
              onClick={() => void act(() => api.post("/api/apps/terminate"))}
            >
              Terminate running app
            </button>
          )}
        </div>
        {error && <p className="error">{error}</p>}
      </section>

      {editing && (
        <Editor
          entry={editing}
          onCancel={() => setEditing(null)}
          onSave={async (entry) => {
            await act(() =>
              entry.id === 0 && !apps.some((a) => a.id === entry.id)
                ? api.post("/api/apps", entry)
                : api.put(`/api/apps/${entry.id}`, entry),
            );
            setEditing(null);
          }}
        />
      )}
    </>
  );
}

function Editor({
  entry,
  onSave,
  onCancel,
}: {
  entry: AppEntry;
  onSave: (entry: AppEntry) => Promise<void>;
  onCancel: () => void;
}) {
  const [draft, setDraft] = useState<AppEntry>(entry);
  const [argsText, setArgsText] = useState(entry.args.join(" "));

  function set<K extends keyof AppEntry>(key: K, value: AppEntry[K]) {
    setDraft((d) => ({ ...d, [key]: value }));
  }

  return (
    <section className="card">
      <h2>{entry.name === "" ? "New application" : `Editing ${entry.name}`}</h2>

      <label>
        Name
        <input value={draft.name} onChange={(e) => set("name", e.target.value)} />
      </label>

      <label>
        Executable or URI
        <input
          value={draft.exe}
          placeholder="steam://open/bigpicture"
          onChange={(e) => set("exe", e.target.value)}
        />
      </label>

      <label>
        Arguments
        <input
          value={argsText}
          onChange={(e) => {
            setArgsText(e.target.value);
            set(
              "args",
              e.target.value.split(" ").filter((a) => a !== ""),
            );
          }}
        />
      </label>

      <label>
        Working directory
        <input
          value={draft.working_dir ?? ""}
          onChange={(e) => set("working_dir", e.target.value || null)}
        />
      </label>

      <p className="hint">
        Overrides apply only while this app is the running one; blank inherits
        the stream defaults.
      </p>
      <label>
        Codec override
        <select
          value={draft.overrides.codec ?? ""}
          onChange={(e) =>
            set("overrides", {
              ...draft.overrides,
              codec: e.target.value === "" ? null : e.target.value,
            })
          }
        >
          <option value="">Inherit default</option>
          <option value="auto">Auto</option>
          <option value="hevc">HEVC (Main10)</option>
          <option value="av1">AV1 (Main10)</option>
          <option value="h264">H.264 (8-bit SDR)</option>
        </select>
      </label>
      <label>
        NVENC preset override
        <select
          value={draft.overrides.preset ?? ""}
          onChange={(e) =>
            set("overrides", {
              ...draft.overrides,
              preset: e.target.value === "" ? null : Number(e.target.value),
            })
          }
        >
          <option value="">Inherit default</option>
          <option value={1}>P1 — fastest</option>
          <option value={2}>P2</option>
          <option value={3}>P3</option>
          <option value={4}>P4 — highest quality</option>
        </select>
      </label>
      <label>
        Bitrate override (kbps)
        <input
          inputMode="numeric"
          value={draft.overrides.bitrate_kbps ?? ""}
          onChange={(e) =>
            set("overrides", {
              ...draft.overrides,
              bitrate_kbps: e.target.value === "" ? null : Number(e.target.value),
            })
          }
        />
      </label>

      <p className="hint">
        Preparation commands run before launch and are undone in reverse order
        after the app exits — resolution changes and HDR toggles belong here.
      </p>
      {draft.prep.map((p, i) => (
        <div className="row" key={i}>
          <input
            placeholder="run before"
            value={p.run}
            onChange={(e) => {
              const prep = [...draft.prep];
              prep[i] = { ...p, run: e.target.value };
              set("prep", prep);
            }}
          />
          <input
            placeholder="undo after (optional)"
            value={p.undo ?? ""}
            onChange={(e) => {
              const prep = [...draft.prep];
              prep[i] = { ...p, undo: e.target.value || null };
              set("prep", prep);
            }}
          />
          <button
            className="danger"
            onClick={() => set("prep", draft.prep.filter((_, j) => j !== i))}
          >
            Remove
          </button>
        </div>
      ))}
      <button onClick={() => set("prep", [...draft.prep, { run: "", undo: null }])}>
        Add preparation command
      </button>

      <div className="row">
        <button onClick={() => void onSave(draft)} disabled={draft.name.trim() === ""}>
          Save
        </button>
        <button onClick={onCancel}>Cancel</button>
      </div>
    </section>
  );
}
