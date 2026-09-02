// Connecting to a host, and what the connection is doing once it is up.
//
// The scan is a one-shot broadcast that waits ~700 ms for answers, so it is a
// button rather than something that runs continuously — a background scan on a
// latency-sensitive app is noise on the wire for nothing.

import { useCallback, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { Api, Found, Snapshot } from "./types";

export function ClientView({ snap, api }: { snap: Snapshot; api: Api }) {
  const [hosts, setHosts] = useState<Found[]>([]);
  const [scanning, setScanning] = useState(false);
  const [picked, setPicked] = useState<Found | null>(null);
  const [address, setAddress] = useState("");
  const [code, setCode] = useState("");

  const scan = useCallback(async () => {
    setScanning(true);
    try {
      setHosts(await invoke<Found[]>("scan_hosts"));
    } finally {
      setScanning(false);
    }
  }, []);

  // One scan on arrival: the common case is that the host is already running.
  useEffect(() => {
    if (snap.role === "idle") void scan();
  }, [snap.role, scan]);

  const connection = snap.connection;

  if (snap.role === "client" && connection) {
    return (
      <section className="card">
        <header>
          <h2>{connection.host_name || connection.address}</h2>
          <span className={connection.connected ? "pill on" : "pill warn"}>
            {connection.connected ? "connected" : "reconnecting"}
          </span>
        </header>
        <p className="big">
          {connection.controlled
            ? "The host is driving this machine."
            : connection.connected
              ? "Ready. Move the host's pointer onto this screen."
              : "Trying to reach the host…"}
        </p>
        <dl className="facts">
          <div>
            <dt>Host</dt>
            <dd>
              {connection.host_os || "—"} · {connection.address}
            </dd>
          </div>
          <div>
            <dt>Clipboard</dt>
            <dd>{snap.clipboard_sync ? "shared" : "off"}</dd>
          </div>
          {connection.attempts > 0 ? (
            <div>
              <dt>Retries</dt>
              <dd>{connection.attempts}</dd>
            </div>
          ) : null}
        </dl>
        {connection.message ? <p className="warn">{connection.message}</p> : null}
        <div className="row">
          <button className="danger" onClick={() => api.run("stop")} disabled={api.busy}>
            Disconnect
          </button>
        </div>
      </section>
    );
  }

  const connect = () => {
    // A discovered host carries its id, which lets an already-paired device
    // reconnect on its stored key instead of the code.
    void api.run("start_client", {
      address: picked ? picked.address : address,
      port: picked?.port,
      hostId: picked?.id,
      code: code.trim() || null,
    });
  };

  const target = picked ? picked.address : address.trim();

  return (
    <section className="card">
      <header>
        <h2>Connect to a host</h2>
        <button onClick={() => void scan()} disabled={scanning}>
          {scanning ? "Scanning…" : "Scan again"}
        </button>
      </header>

      {hosts.length > 0 ? (
        <ul className="hosts">
          {hosts.map((host) => (
            <li key={host.id}>
              <button
                className={picked?.id === host.id ? "picked" : ""}
                onClick={() => {
                  setPicked(host);
                  setAddress(host.address);
                }}
              >
                <strong>{host.name}</strong>
                <span className="muted">
                  {host.os} · {host.address}
                </span>
                {host.needs_code ? <span className="pill on">pairing open</span> : null}
              </button>
            </li>
          ))}
        </ul>
      ) : (
        <p className="muted">
          {scanning
            ? "Looking for hosts on this network…"
            : "No hosts answered. Make sure the other machine is hosting and both are on the same WiFi, or type its address below."}
        </p>
      )}

      <form
        className="fields"
        onSubmit={(event) => {
          event.preventDefault();
          connect();
        }}
      >
        <label>
          Host address
          <input
            value={address}
            placeholder="192.168.1.20"
            onChange={(event) => {
              setAddress(event.currentTarget.value);
              // Typing an address means the picked host no longer applies.
              setPicked(null);
            }}
          />
        </label>
        <label>
          Pairing code
          <input
            value={code}
            placeholder="from the host's screen"
            autoCapitalize="characters"
            onChange={(event) => setCode(event.currentTarget.value)}
          />
        </label>
        <button type="submit" className="primary" disabled={api.busy || !target}>
          Connect
        </button>
      </form>
      <p className="muted">
        The code is only needed the first time. After that this device has its own
        key and reconnects by itself.
      </p>
      {snap.can_host ? null : (
        <p className="muted">
          This machine can follow but not host: capturing a keyboard and mouse
          system-wide needs platform-specific code, and only Windows has it so
          far. Host from a Windows machine and every other device can join.
        </p>
      )}
    </section>
  );
}
