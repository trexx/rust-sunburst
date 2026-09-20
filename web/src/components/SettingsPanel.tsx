// SPDX-License-Identifier: GPL-2.0-or-later

import { useCallback, useEffect, useState } from "react";
import { api, setToken } from "../api";
import type {
  AudioDevices,
  InputSettings,
  Settings,
  StreamSettings,
} from "../api";

export function SettingsPanel() {
  const [settings, setSettings] = useState<Settings | null>(null);
  const [devices, setDevices] = useState<string[]>([]);
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

  useEffect(() => {
    // Best-effort: the picker degrades to a free-text-preserving dropdown if
    // enumeration is unavailable (a non-Windows host, or the Fake in tests).
    api
      .get<AudioDevices>("/api/audio-devices")
      .then((d) => setDevices(d.devices))
      .catch(() => setDevices([]));
  }, []);

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

  const s = settings;
  const patchWeb = (p: Partial<Settings["web"]>) =>
    setSettings({ ...s, web: { ...s.web, ...p } });
  const patchStream = (p: Partial<StreamSettings>) =>
    setSettings({ ...s, stream: { ...s.stream, ...p } });
  const patchInput = (p: Partial<InputSettings>) =>
    setSettings({ ...s, input: { ...s.input, ...p } });

  const lanExposed = !(
    s.web.bind.startsWith("127.") || s.web.bind === "::1"
  );

  return (
    <>
      <section className="card">
        <h2>Web interface</h2>
        <label>
          Bind address
          <input value={s.web.bind} onChange={(e) => patchWeb({ bind: e.target.value })} />
        </label>
        <label>
          Port
          <input
            inputMode="numeric"
            value={s.web.port}
            onChange={(e) => patchWeb({ port: Number(e.target.value) || 0 })}
          />
        </label>
        <label>
          Token
          <input value={s.web.token} onChange={(e) => patchWeb({ token: e.target.value })} />
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
            value={s.web.assets_dir}
            onChange={(e) => patchWeb({ assets_dir: e.target.value })}
          />
        </label>
      </section>

      <section className="card">
        <h2>Video</h2>
        <p className="hint">Applied unless an application overrides them.</p>
        <label>
          UDP port
          <input
            inputMode="numeric"
            value={s.stream.port}
            onChange={(e) => patchStream({ port: Number(e.target.value) || 0 })}
          />
        </label>
        <label>
          Codec
          <select
            value={s.stream.codec}
            onChange={(e) =>
              patchStream({ codec: e.target.value as StreamSettings["codec"] })
            }
          >
            <option value="auto">Auto</option>
            <option value="hevc">HEVC (Main10)</option>
            <option value="av1">AV1 (Main10)</option>
            <option value="h264">H.264 (8-bit SDR)</option>
          </select>
        </label>
        <p className="hint">
          Auto is the only setting that works for both devices: the Shield has no
          AV1 decoder and the Homatics' HEVC decoder is broken. H.264 is 8-bit
          SDR — lower latency, no HDR.
        </p>
        <label>
          Bitrate (kbps)
          <input
            inputMode="numeric"
            value={s.stream.bitrate_kbps}
            onChange={(e) => patchStream({ bitrate_kbps: Number(e.target.value) || 0 })}
          />
        </label>
        <label className="check">
          <input
            type="checkbox"
            checked={s.stream.hdr}
            onChange={(e) => patchStream({ hdr: e.target.checked })}
          />
          Stream HDR (ignored for H.264, which is always SDR)
        </label>
        <label className="check">
          <input
            type="checkbox"
            checked={s.stream.match_resolution}
            onChange={(e) => patchStream({ match_resolution: e.target.checked })}
          />
          Match the client's resolution (switches the physical display mode)
        </label>
        <label className="check">
          <input
            type="checkbox"
            checked={s.stream.virtual_display}
            onChange={(e) => patchStream({ virtual_display: e.target.checked })}
          />
          Use a virtual display (MikeTheTech VDD, if installed — headless)
        </label>
      </section>

      <section className="card">
        <h2>Audio</h2>
        <label className="check">
          <input
            type="checkbox"
            checked={s.stream.audio}
            onChange={(e) => patchStream({ audio: e.target.checked })}
          />
          Stream game audio (WASAPI loopback + Opus)
        </label>
        <label>
          Capture device
          <select
            value={s.stream.audio_device ?? ""}
            onChange={(e) => patchStream({ audio_device: e.target.value || null })}
          >
            <option value="">System default endpoint (host audible)</option>
            {devices.map((d) => (
              <option key={d} value={d}>
                {d}
              </option>
            ))}
            {s.stream.audio_device &&
              !devices.includes(s.stream.audio_device) && (
                <option value={s.stream.audio_device}>
                  {s.stream.audio_device} (not currently present)
                </option>
              )}
          </select>
        </label>
        <p className="hint">
          "Steam Streaming Speakers" silences the host while the client still
          gets audio. Matched by name substring; the endpoint should be 48 kHz.
        </p>
        <label>
          Pad headset mic → virtual microphone
          <select
            value={s.stream.mic_device ?? ""}
            onChange={(e) => patchStream({ mic_device: e.target.value || null })}
          >
            <option value="">Off (no pad microphone)</option>
            {devices.map((d) => (
              <option key={d} value={d}>
                {d}
              </option>
            ))}
            {s.stream.mic_device && !devices.includes(s.stream.mic_device) && (
              <option value={s.stream.mic_device}>
                {s.stream.mic_device} (not currently present)
              </option>
            )}
          </select>
        </label>
        <p className="hint">
          Plays an Xbox pad's headset mic into a consumed virtual microphone
          (install one via Steam Remote Play — "Steam Streaming Microphone").
          Games read it as a mic input. Off renders the pad mic nowhere.
        </p>
        <label>
          Opus bitrate (kbps)
          <input
            inputMode="numeric"
            value={s.stream.audio_bitrate_kbps}
            onChange={(e) =>
              patchStream({ audio_bitrate_kbps: Number(e.target.value) || 0 })
            }
          />
        </label>
      </section>

      <section className="card">
        <h2>Input</h2>
        <label>
          Mouse sensitivity (1.0 = 1:1)
          <input
            inputMode="decimal"
            value={s.input.mouse_sensitivity}
            onChange={(e) =>
              patchInput({ mouse_sensitivity: Number(e.target.value) || 0 })
            }
          />
        </label>
        <label>
          Gamepad deadzone (0–1, on top of the client's)
          <input
            inputMode="decimal"
            value={s.input.gamepad_deadzone}
            onChange={(e) =>
              patchInput({ gamepad_deadzone: Number(e.target.value) || 0 })
            }
          />
        </label>
        <label className="check">
          <input
            type="checkbox"
            checked={s.input.disable_epp}
            onChange={(e) => patchInput({ disable_epp: e.target.checked })}
          />
          Turn Enhanced Pointer Precision off while streaming (restored after)
        </label>
      </section>

      <section className="card">
        <details className="advanced">
          <summary>Advanced encoder &amp; capture</summary>
          <p className="hint">
            Defaults reproduce the fixed low-latency behaviour. The preset stays
            inside <code>ULTRA_LOW_LATENCY</code>; there is no UHQ escape here.
          </p>

          <h3>Encoder</h3>
          <label>
            NVENC preset (1 = P1 fastest … 4 = P4)
            <select
              value={s.stream.preset}
              onChange={(e) => patchStream({ preset: Number(e.target.value) })}
            >
              <option value={1}>P1 — fastest</option>
              <option value={2}>P2</option>
              <option value={3}>P3</option>
              <option value={4}>P4 — highest quality</option>
            </select>
          </label>
          <label>
            Rate control
            <select
              value={s.stream.rate_control}
              onChange={(e) =>
                patchStream({
                  rate_control: e.target.value as StreamSettings["rate_control"],
                })
              }
            >
              <option value="cbr">CBR — steady wire load</option>
              <option value="vbr">VBR — cheaper on static frames</option>
            </select>
          </label>
          <label>
            Minimum bitrate (kbps)
            <input
              inputMode="numeric"
              value={s.stream.min_bitrate_kbps}
              onChange={(e) =>
                patchStream({ min_bitrate_kbps: Number(e.target.value) || 0 })
              }
            />
          </label>
          <label>
            Maximum bitrate (kbps, 0 = use the bitrate above)
            <input
              inputMode="numeric"
              value={s.stream.max_bitrate_kbps}
              onChange={(e) =>
                patchStream({ max_bitrate_kbps: Number(e.target.value) || 0 })
              }
            />
          </label>
          <label>
            Slices / tiles-per-axis (0 = codec default)
            <input
              inputMode="numeric"
              value={s.stream.slices}
              onChange={(e) => patchStream({ slices: Number(e.target.value) || 0 })}
            />
          </label>
          <label>
            Forced IDR period (frames, 0 = infinite GOP)
            <input
              inputMode="numeric"
              value={s.stream.idr_period}
              onChange={(e) => patchStream({ idr_period: Number(e.target.value) || 0 })}
            />
          </label>
          <label>
            DPB depth (reference frames)
            <input
              inputMode="numeric"
              value={s.stream.dpb_depth}
              onChange={(e) => patchStream({ dpb_depth: Number(e.target.value) || 0 })}
            />
          </label>
          <label>
            Frame-rate cap (0 = follow the client's refresh)
            <input
              inputMode="numeric"
              value={s.stream.fps_cap}
              onChange={(e) => patchStream({ fps_cap: Number(e.target.value) || 0 })}
            />
          </label>

          <h3>Capture</h3>
          <label>
            Backend
            <select
              value={s.stream.capture_backend}
              onChange={(e) =>
                patchStream({
                  capture_backend:
                    e.target.value as StreamSettings["capture_backend"],
                })
              }
            >
              <option value="auto">Auto — WGC on Win11, DDA on Win10</option>
              <option value="wgc">WGC (Windows Graphics Capture)</option>
              <option value="dda">DDA (Desktop Duplication)</option>
              <option value="nvfbc">NvFBC (resilience only — see docs)</option>
            </select>
          </label>
          <label>
            Capture output index (blank = primary monitor)
            <input
              inputMode="numeric"
              value={s.stream.capture_output ?? ""}
              onChange={(e) =>
                patchStream({
                  capture_output: e.target.value === "" ? null : Number(e.target.value),
                })
              }
            />
          </label>

          <h3>Audio codec</h3>
          <label>
            Opus frame duration
            <select
              value={s.stream.audio_frame_us}
              onChange={(e) => patchStream({ audio_frame_us: Number(e.target.value) })}
            >
              <option value={2500}>2.5 ms — lowest latency</option>
              <option value={5000}>5 ms (default)</option>
              <option value={10000}>10 ms</option>
              <option value={20000}>20 ms — least overhead</option>
            </select>
          </label>
          <label className="check">
            <input
              type="checkbox"
              checked={s.stream.audio_fec}
              onChange={(e) => patchStream({ audio_fec: e.target.checked })}
            />
            Opus in-band forward error correction
          </label>
          <label>
            Opus complexity (0–10)
            <input
              inputMode="numeric"
              value={s.stream.audio_complexity}
              onChange={(e) =>
                patchStream({ audio_complexity: Number(e.target.value) || 0 })
              }
            />
          </label>
        </details>
      </section>

      <div className="row">
        <button onClick={() => void save()}>Save</button>
        {saved && <span className="hint">Saved.</span>}
      </div>
      {error && <p className="error">{error}</p>}
    </>
  );
}
