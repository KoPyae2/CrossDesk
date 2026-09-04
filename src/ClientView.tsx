// Connecting to a host, and what the connection is doing once it is up.
//
// The scan is a one-shot broadcast that waits ~700 ms for answers, so it is a
// button rather than something that runs continuously — a background scan on a
// latency-sensitive app is noise on the wire for nothing.

import { useCallback, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { Api, Found, Probe, Snapshot } from "./types";

/** What macOS has to say about replaying input, and the two things that actually
 *  resolve it.
 *
 *  Advisory, never blocking. `AXIsProcessTrusted` can report a grant that no
 *  longer matches the running binary — a rebuilt unsigned build has a new code
 *  signature and the ticked entry belongs to the old one — so a UI that refuses to
 *  connect on its word leaves the user stuck with a permission they have visibly
 *  granted. **Test pointer** posts a real movement and reads the position back,
 *  which is the one answer the OS cannot get wrong. */
function InputAccess({ snap, api }: { snap: Snapshot; api: Api }) {
  const [probe, setProbe] = useState<Probe | null>(null);
  const [testing, setTesting] = useState(false);

  const test = async () => {
    setTesting(true);
    try {
      setProbe(await invoke<Probe>("test_input"));
    } catch (error) {
      setProbe({ moved: false, access: snap.input_access, detail: String(error) });
    } finally {
      setTesting(false);
    }
  };

  // A grant made in System Settings clears the warning on its own via the state
  // tick, which also invalidates any earlier failed probe.
  useEffect(() => {
    setProbe(null);
  }, [snap.input_access]);

  if (snap.input_access === "not_needed") return null;

  const granted = snap.input_access === "granted";
  // The probe outranks the OS: movement is proof, and a granted claim that could
  // not move is exactly the case this panel exists for.
  const working = probe ? probe.moved : granted;

  return (
    <div className={working ? "notice" : "notice bad"}>
      <p>
        {probe
          ? probe.detail
          : granted
            ? "macOS has granted Accessibility, so this machine can be driven by the host. If the pointer still does not move, press Test pointer — the grant may belong to an older build."
            : "macOS has not granted CrossDesk the Accessibility permission, so it cannot move this machine's mouse or type on it. Press Grant permission, allow CrossDesk, then come back — this screen notices by itself."}
      </p>
      <div className="row">
        <button onClick={() => void test()} disabled={testing}>
          {testing ? "Testing…" : "Test pointer"}
        </button>
        <button
          className={working ? "" : "primary"}
          onClick={() => api.run("request_input_access")}
          disabled={api.busy}
        >
          {granted ? "Open System Settings" : "Grant permission"}
        </button>
      </div>
    </div>
  );
}

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
            <dd>
              {snap.clipboard_sync
                ? snap.clipboard_images
                  ? "text & images"
                  : "text only"
                : "off"}
            </dd>
          </div>
          {connection.attempts > 0 ? (
            <div>
              <dt>Retries</dt>
              <dd>{connection.attempts}</dd>
            </div>
          ) : null}
        </dl>
        {connection.message ? <p className="warn">{connection.message}</p> : null}
        <InputAccess snap={snap} api={api} />
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

      <InputAccess snap={snap} api={api} />

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
