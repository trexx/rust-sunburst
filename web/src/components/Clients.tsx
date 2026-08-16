// SPDX-License-Identifier: GPL-2.0-or-later

import { useCallback, useEffect, useState } from "react";
import { api, formatDate } from "../api";
import type { Client, PendingPair } from "../api";

export function Clients({ onChange }: { onChange: () => void }) {
  const [clients, setClients] = useState<Client[]>([]);
  const [pending, setPending] = useState<PendingPair[]>([]);
  const [armedUntil, setArmedUntil] = useState<number | null>(null);
  const [error, setError] = useState<string | null>(null);

  const refresh = useCallback(async () => {
    try {
      setClients(await api.get<Client[]>("/api/clients"));
      setPending(await api.get<PendingPair[]>("/api/pair/pending"));
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  }, []);

  useEffect(() => {
    void refresh();
    // Polled because a pair request arrives over the control channel, not from
    // anything this page did.
    const timer = setInterval(() => void refresh(), 2000);
    return () => clearInterval(timer);
  }, [refresh]);

  async function arm() {
    setError(null);
    try {
      const { expires_at } = await api.post<{ expires_at: number }>("/api/pair/arm");
      setArmedUntil(expires_at);
      await refresh();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  }

  async function revoke(id: number) {
    setError(null);
    try {
      await api.del(`/api/clients/${id}`);
      await refresh();
      onChange();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  }

  return (
    <>
      <section className="card">
        <h2>Pairing</h2>
        <p className="hint">
          Arm pairing here, then start pairing on the TV. The device shows an
          eight-digit PIN; type it in below. The PIN itself never crosses the
          network — both ends derive the same secret from it independently.
        </p>
        <div className="row">
          <button onClick={() => void arm()}>Arm pairing (90s)</button>
          {armedUntil !== null && (
            <span className="hint">
              armed until {formatDate(armedUntil)}
            </span>
          )}
        </div>

        {pending.length > 0 && (
          <table>
            <thead>
              <tr>
                <th>Device</th>
                <th>Model</th>
                <th>ABI</th>
                <th>PIN</th>
              </tr>
            </thead>
            <tbody>
              {pending.map((p) => (
                <PendingRow key={p.id} pending={p} onDone={() => { void refresh(); onChange(); }} />
              ))}
            </tbody>
          </table>
        )}
        {error && <p className="error">{error}</p>}
      </section>

      <section className="card">
        <h2>Paired clients</h2>
        {clients.length === 0 && <p className="hint">Nothing paired yet.</p>}
        {clients.length > 0 && (
          <table>
            <thead>
              <tr>
                <th>Name</th>
                <th>Model</th>
                <th>ABI</th>
                <th>Decoder</th>
                <th>Paired</th>
                <th />
              </tr>
            </thead>
            <tbody>
              {clients.map((c) => (
                <tr key={c.id}>
                  <td>{c.name}</td>
                  <td>{c.model}</td>
                  <td>{c.abi}</td>
                  <td className="hint">
                    {c.quirks.ref_invalidation ? "ref-inval " : ""}
                    {c.quirks.intra_refresh ? "intra-refresh " : ""}
                    {c.quirks.slice_output ? "slices " : ""}
                    {!c.quirks.ref_invalidation &&
                      !c.quirks.intra_refresh &&
                      !c.quirks.slice_output &&
                      "conservative"}
                  </td>
                  {/* Shown so a pairing nobody remembers making is visible. */}
                  <td>{formatDate(c.paired_at)}</td>
                  <td>
                    <button className="danger" onClick={() => void revoke(c.id)}>
                      Revoke
                    </button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </section>
    </>
  );
}

function PendingRow({
  pending,
  onDone,
}: {
  pending: PendingPair;
  onDone: () => void;
}) {
  const [pin, setPin] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [name, setName] = useState(pending.name);

  async function confirm() {
    setError(null);
    try {
      await api.post("/api/pair/confirm", {
        request_id: pending.id,
        pin,
        name: name.trim() || null,
      });
      onDone();
    } catch (e) {
      setPin("");
      setError(e instanceof Error ? e.message : String(e));
    }
  }

  return (
    <tr>
      <td>
        <input value={name} onChange={(e) => setName(e.target.value)} />
      </td>
      <td>{pending.model}</td>
      <td>{pending.abi}</td>
      <td>
        {pending.awaiting_client ? (
          // Typing a PIN before the client's confirmation arrives cannot work,
          // so the input stays disabled rather than failing confusingly.
          <span className="hint">waiting for the device…</span>
        ) : (
          <div className="row">
            <input
              inputMode="numeric"
              placeholder="00000000"
              maxLength={8}
              value={pin}
              onChange={(e) => setPin(e.target.value.replace(/\D/g, ""))}
            />
            <button disabled={pin.length !== 8} onClick={() => void confirm()}>
              Pair
            </button>
          </div>
        )}
        {error && <p className="error">{error}</p>}
      </td>
    </tr>
  );
}
