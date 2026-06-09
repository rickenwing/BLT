import { useCallback, useEffect, useState } from "react";
import { api, formatSpeed, on, RosterEntry } from "../lib/api";

export default function People() {
  const [roster, setRoster] = useState<RosterEntry[]>([]);

  const load = useCallback(async () => {
    setRoster(await api.roster());
  }, []);

  useEffect(() => {
    void load();
    const un = on("roster-changed", load);
    return () => {
      void un.then((u) => u());
    };
  }, [load]);

  return (
    <>
      <h1>People</h1>
      <div className="panel">
        {roster.length === 0 ? (
          <span className="dim">Nobody else is connected right now.</span>
        ) : (
          <table>
            <thead>
              <tr>
                <th>Name</th>
                <th>Machine</th>
                <th>Doing</th>
                <th>Seed speed</th>
                <th></th>
              </tr>
            </thead>
            <tbody>
              {roster.map((r) => (
                <tr key={r.client_id}>
                  <td>
                    <strong>{r.display_name || r.client_id}</strong>
                  </td>
                  <td className="dim">{r.machine_name}</td>
                  <td>{r.activity}</td>
                  <td>{r.throughput_bps != null ? formatSpeed(r.throughput_bps) : "—"}</td>
                  <td>
                    {r.server_only && (
                      <span className="badge warn" title="P2P unreachable — downloads come from the server only">
                        server-only
                      </span>
                    )}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </div>
      <p className="dim">
        Speeds are measured from real transfers — no benchmarks, no Wi-Fi band
        guessing. “server-only” means that client’s P2P probe failed (AP
        isolation or a firewall); they still download fine from the server.
      </p>
    </>
  );
}
