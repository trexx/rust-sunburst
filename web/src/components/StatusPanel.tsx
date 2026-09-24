// SPDX-License-Identifier: GPL-2.0-or-later

import { useCallback, useEffect, useState } from "react";
import { ApiError, api, formatDuration, formatNs } from "../api";
import type { Metrics, Status } from "../api";

export function StatusPanel({
  status,
  onChange,
}: {
  status: Status | null;
  onChange: () => void;
}) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  async function act(fn: () => Promise<unknown>) {
    setBusy(true);
    setError(null);
    try {
      await fn();
      onChange();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  }

  if (!status) return <p>No status.</p>;

  return (
    <>
      <section className="card">
        <h2>Server</h2>
        <dl className="facts">
          <dt>Version</dt>
          <dd>{status.version}</dd>
          <dt>Uptime</dt>
          <dd>{formatDuration(status.uptime_secs)}</dd>
          <dt>Process</dt>
          <dd>{status.pid}</dd>
          <dt>Elevated</dt>
          <dd>
            {status.elevated ? "yes" : "no"}
            {!status.elevated && (
              // Worth saying here rather than leaving to be discovered as "input
              // does nothing in one game".
              <span className="hint">
                {" "}
                — input cannot reach elevated windows past UIPI
              </span>
            )}
          </dd>
          <dt>Other encoders</dt>
          <dd>
            {status.other_encoders === null ? (
              <span className="hint">unknown — NVML could not be asked</span>
            ) : status.other_encoders.length === 0 ? (
              "none"
            ) : (
              <>
                <strong className="error">
                  {status.other_encoders
                    .map(
                      (e) =>
                        `${e.process || "unknown"} (pid ${e.pid}, ${e.width}×${e.height})`,
                    )
                    .join(", ")}
                </strong>
                {/* Said here because the symptom never points at the cause:
                    jittery encode times read as our bug. */}
                <span className="hint">
                  {" "}
                  — shares the GPU's one encoder, so frame times will jitter. Close
                  it, or turn off ShadowPlay / Instant Replay.
                </span>
              </>
            )}
          </dd>
          <dt>Paired clients</dt>
          <dd>{status.paired_clients}</dd>
          <dt>Sessions</dt>
          <dd>{status.sessions}</dd>
        </dl>
      </section>

      <section className="card">
        <h2>Start-up</h2>
        <p>
          Autostart:{" "}
          <strong>
            {status.autostart === null
              ? "unknown"
              : status.autostart
                ? "on"
                : "off"}
          </strong>
          {status.autostart === null && (
            <span className="hint"> — the scheduled task could not be read</span>
          )}
        </p>
        <div className="row">
          <button
            disabled={busy}
            onClick={() =>
              void act(() =>
                api.post("/api/autostart", { enabled: !status.autostart }),
              )
            }
          >
            {status.autostart ? "Disable autostart" : "Enable autostart"}
          </button>
          <button disabled={busy} onClick={() => void act(() => api.post("/api/restart"))}>
            Restart server
          </button>
        </div>
        <p className="hint">
          There is no service. The server runs in your logged-in session, because
          capture and input both require it — so with nobody logged in there is
          no server and no web UI.
        </p>
        {error && <p className="error">{error}</p>}
      </section>

      <MetricsCard />
    </>
  );
}

function MetricsCard() {
  const [metrics, setMetrics] = useState<Metrics | null>(null);
  const [unavailable, setUnavailable] = useState<string | null>(null);

  const refresh = useCallback(async () => {
    try {
      setMetrics(await api.get<Metrics>("/api/metrics"));
      setUnavailable(null);
    } catch (e) {
      setMetrics(null);
      setUnavailable(e instanceof ApiError ? e.message : String(e));
    }
  }, []);

  useEffect(() => {
    void refresh();
    const timer = setInterval(() => void refresh(), 2000);
    return () => clearInterval(timer);
  }, [refresh]);

  return (
    <section className="card">
      <h2>Pipeline</h2>
      {unavailable && <p className="hint">{unavailable}</p>}

      {metrics?.lossy && (
        // The warning `Report::is_lossy` exists to carry. A thinned percentile
        // table is the metric that misleads, and this is not the place to be
        // quiet about it.
        <p className="banner warning">
          {metrics.dropped_samples} samples dropped
          {metrics.unregistered_threads > 0 &&
            `, ${metrics.unregistered_threads} threads unregistered`}
          . These percentiles were computed over what survived — do not quote
          them.
          {metrics.dropped_by_thread.length > 0 && (
            <> Worst: {metrics.dropped_by_thread.map(([n, c]) => `${n} (${c})`).join(", ")}.</>
          )}
        </p>
      )}

      {metrics && metrics.stages.length > 0 && (
        <table>
          <thead>
            <tr>
              <th>Stage</th>
              <th>Count</th>
              <th>p50</th>
              <th>p95</th>
              <th>p99</th>
              <th>max</th>
            </tr>
          </thead>
          <tbody>
            {metrics.stages.map((s) => (
              <tr key={s.stage}>
                <td>{s.stage}</td>
                <td>{s.count}</td>
                <td>{formatNs(s.p50_ns)}</td>
                <td>{formatNs(s.p95_ns)}</td>
                <td>{formatNs(s.p99_ns)}</td>
                <td>{formatNs(s.max_ns)}</td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </section>
  );
}
