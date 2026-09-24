// SPDX-License-Identifier: GPL-2.0-or-later

import { useCallback, useEffect, useRef, useState } from "react";
import { ART_MAX_BYTES, api } from "../api";
import type { AppEntry, ArtInfo, Status } from "../api";

/** Box art is shown on the TV as a 2:3 tile; nothing bigger is worth sending. */
const ART_MAX_W = 600;
const ART_MAX_H = 900;

/** Draw `file` no larger than the tile needs and encode it small enough for the
 *  server: WebP (JPEG where the browser cannot encode WebP), stepping the
 *  quality down until it fits. The server never decodes an image; this is the
 *  one place one is resized. */
async function prepareArt(file: File): Promise<Blob> {
  const bitmap = await createImageBitmap(file);
  const scale = Math.min(1, ART_MAX_W / bitmap.width, ART_MAX_H / bitmap.height);
  const canvas = document.createElement("canvas");
  canvas.width = Math.max(1, Math.round(bitmap.width * scale));
  canvas.height = Math.max(1, Math.round(bitmap.height * scale));
  const ctx = canvas.getContext("2d");
  if (!ctx) throw new Error("this browser cannot draw to a canvas");
  ctx.drawImage(bitmap, 0, 0, canvas.width, canvas.height);
  bitmap.close();

  const encode = (type: string, quality: number) =>
    new Promise<Blob | null>((resolve) => canvas.toBlob(resolve, type, quality));
  for (const quality of [0.85, 0.75, 0.6, 0.45, 0.3]) {
    let blob = await encode("image/webp", quality);
    // A browser that cannot write WebP hands back PNG instead; use JPEG.
    if (!blob || blob.type !== "image/webp") blob = await encode("image/jpeg", quality);
    if (blob && blob.size <= ART_MAX_BYTES) return blob;
  }
  throw new Error("could not make this image small enough");
}

const BLANK: AppEntry = {
  id: 0,
  name: "",
  exe: "",
  args: [],
  working_dir: null,
  prep: [],
  wait_process: null,
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
  // Thumbnails by app id, as object URLs; the digest says when to refetch. The
  // ref is the cache `refresh` reads (a stable callback would otherwise see
  // only the first, empty map); the state is what renders.
  const cache = useRef(new Map<number, { digest: string; url: string }>());
  const [thumbs, setThumbs] = useState(cache.current);
  const artFor = useRef<number | null>(null);
  const picker = useRef<HTMLInputElement>(null);

  const refresh = useCallback(async () => {
    try {
      setApps(await api.get<AppEntry[]>("/api/apps"));
      const art = await api.get<ArtInfo[]>("/api/art");
      const next = new Map<number, { digest: string; url: string }>();
      for (const a of art) {
        const known = cache.current.get(a.app_id);
        if (known && known.digest === a.digest) {
          next.set(a.app_id, known);
          continue;
        }
        const blob = await api.getBlob(`/api/apps/${a.app_id}/art`);
        if (blob) next.set(a.app_id, { digest: a.digest, url: URL.createObjectURL(blob) });
      }
      for (const [id, t] of cache.current) {
        if (next.get(id) !== t) URL.revokeObjectURL(t.url);
      }
      cache.current = next;
      setThumbs(next);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  }, []);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  // Object URLs hold their image until revoked.
  useEffect(
    () => () => {
      for (const t of cache.current.values()) URL.revokeObjectURL(t.url);
    },
    [],
  );

  async function uploadArt(file: File | undefined) {
    const id = artFor.current;
    if (!file || id === null) return;
    await act(async () => api.putBlob(`/api/apps/${id}/art`, await prepareArt(file)));
  }

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
  const chooseArt = (id: number) => {
    artFor.current = id;
    picker.current?.click();
  };
  // An untracked app (a URI with nothing to watch) is replaced by the next
  // launch, so it does not disable the buttons.
  const blocking = running !== null && running.tracking !== "untracked";

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
                <th>Art</th>
                <th>Name</th>
                <th>Command</th>
                <th />
              </tr>
            </thead>
            <tbody>
              {apps.map((a) => (
                <tr key={a.id}>
                  <td>
                    {thumbs.has(a.id) ? (
                      <img className="thumb" src={thumbs.get(a.id)!.url} alt="" />
                    ) : (
                      <div className="thumb" />
                    )}
                  </td>
                  <td>
                    {a.name}
                    {running?.app_id === a.id && (
                      <span className="hint"> — {describeRunning(running)}</span>
                    )}
                  </td>
                  <td>
                    <code>{a.exe}</code>
                  </td>
                  <td className="row">
                    <button
                      onClick={() => void act(() => api.post(`/api/apps/${a.id}/launch`))}
                      disabled={blocking}
                    >
                      Launch
                    </button>
                    <button onClick={() => setEditing(a)}>Edit</button>
                    <button onClick={() => chooseArt(a.id)}>
                      {thumbs.has(a.id) ? "Replace art" : "Add art"}
                    </button>
                    {thumbs.has(a.id) && (
                      <button
                        onClick={() => void act(() => api.del(`/api/apps/${a.id}/art`))}
                      >
                        Remove art
                      </button>
                    )}
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

        <input
          ref={picker}
          type="file"
          accept="image/*"
          hidden
          onChange={(e) => {
            void uploadArt(e.target.files?.[0]);
            e.target.value = "";
          }}
        />
        <p className="hint">
          Box art shows on the TV's app grid. Any image works: it is scaled to at
          most {ART_MAX_W}×{ART_MAX_H} and compressed in the browser before it is
          sent.
        </p>

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

function describeRunning(r: {
  pid: number;
  tracking: string;
  starting: boolean;
}): string {
  if (r.starting) return "starting (waiting for its process)";
  switch (r.tracking) {
    case "untracked":
      return "launched (not followed; the next launch replaces it)";
    case "process":
      return "running (following its process)";
    default:
      return `running (pid ${r.pid} and everything it started)`;
  }
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
  const [argsText, setArgsText] = useState(entry.args.join("\n"));

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
        Arguments, one per line
        <textarea
          rows={3}
          value={argsText}
          placeholder={"-windowed\n--profile=C:\\Games\\My Profile"}
          onChange={(e) => {
            setArgsText(e.target.value);
            // One argument per line maps 1:1 onto the list the server passes
            // to CreateProcess, so an argument with spaces needs no quoting.
            set(
              "args",
              e.target.value.split(/\r?\n/).filter((a) => a !== ""),
            );
          }}
        />
      </label>

      <label>
        Wait for process
        <input
          value={draft.wait_process ?? ""}
          placeholder="Game.exe"
          onChange={(e) => set("wait_process", e.target.value.trim() || null)}
        />
      </label>
      <p className="hint">
        For an entry that hands off to the game rather than being it — a{" "}
        <code>steam://rungameid/…</code> or Epic URI, or a launcher that starts
        the game outside its own processes. The app counts as running while
        this process does. Leave blank for a game started directly, which is
        followed through everything it starts, and for Big Picture.
      </p>

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
