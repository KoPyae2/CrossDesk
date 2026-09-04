// The shell: one snapshot of backend state, refreshed by an event rather than
// polling, plus the role switch.
//
// Every command returns the new snapshot, so a click updates the UI immediately
// and the 250 ms `crossdesk://state` event carries whatever changed on its own —
// a peer connecting, a latency number, the pointer moving to another machine.

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { ClientView } from "./ClientView";
import { HostView } from "./HostView";
import { LayoutCanvas } from "./LayoutCanvas";
import type { Api, Snapshot } from "./types";
import "./App.css";

export default function App() {
  const [snap, setSnap] = useState<Snapshot | null>(null);
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState("");
  /** Which panel to show while idle, so picking a role does not require the
   *  role to already be running. */
  const [intent, setIntent] = useState<"host" | "client">("host");
  const [renaming, setRenaming] = useState<string | null>(null);
  /** Guards against an in-flight snapshot landing after a newer one. */
  const generation = useRef(0);

  useEffect(() => {
    void invoke<Snapshot>("get_state").then(setSnap);
    const unlisten = listen<Snapshot>("crossdesk://state", (event) => {
      // A command's own reply is newer than any tick already in flight.
      if (generation.current === 0) setSnap(event.payload);
    });
    return () => {
      void unlisten.then((off) => off());
    };
  }, []);

  const run = useCallback(async (cmd: string, args?: Record<string, unknown>) => {
    generation.current += 1;
    setBusy(true);
    setFailure("");
    try {
      setSnap(await invoke<Snapshot>(cmd, args));
    } catch (error) {
      setFailure(String(error));
      // The command failed, but something else may still have changed.
      setSnap(await invoke<Snapshot>("get_state"));
    } finally {
      generation.current -= 1;
      setBusy(false);
    }
  }, []);

  // Drags fire at frame rate, so they get their own path: no `busy` flag (the
  // buttons would flicker) and no snapshot (the whole window would re-render on
  // every pointer sample). The 250 ms state event redraws the wall instead.
  const move = useCallback(async (deviceId: string, x: number, y: number) => {
    try {
      await invoke("move_device", { deviceId, x, y });
    } catch {
      // A move that lost the race with a disconnect is not worth a banner.
    }
  }, []);

  const api: Api = useMemo(() => ({ run, move, busy }), [run, move, busy]);

  if (!snap) {
    return <main className="loading">Starting CrossDesk…</main>;
  }

  // A machine with no capture backend can only follow, so it opens on the client
  // panel and never shows the host one.
  const panel =
    snap.role === "idle" ? (snap.can_host ? intent : "client") : snap.role;

  return (
    <main>
      <nav>
        <div className="brand">
          <img src="/logo.jpeg" className="mark" alt="CrossDesk" />
          <div>
            <strong>CrossDesk</strong>
            <span className="muted">one keyboard, every screen</span>
          </div>
        </div>
        <div className="tabs">
          <button
            className={panel === "host" ? "on" : ""}
            onClick={() => setIntent("host")}
            disabled={snap.role === "client" || !snap.can_host}
            title={
              !snap.can_host
                ? "This platform cannot capture input yet, so it can only follow another machine"
                : snap.role === "client"
                  ? "Disconnect first — a machine cannot host and follow at once"
                  : undefined
            }
          >
            Share this machine
          </button>
          <button
            className={panel === "client" ? "on" : ""}
            onClick={() => setIntent("client")}
            disabled={snap.role === "host"}
            title={
              snap.role === "host"
                ? "Stop hosting first — a machine cannot host and follow at once"
                : undefined
            }
          >
            Follow another
          </button>
        </div>
        <div className="identity">
          {renaming === null ? (
            <button className="ghost" onClick={() => setRenaming(snap.device_name)}>
              {snap.device_name} ✎
            </button>
          ) : (
            <form
              onSubmit={(event) => {
                event.preventDefault();
                void run("set_device_name", { name: renaming });
                setRenaming(null);
              }}
            >
              <input
                autoFocus
                value={renaming}
                onChange={(event) => setRenaming(event.currentTarget.value)}
                onBlur={() => setRenaming(null)}
              />
            </form>
          )}
        </div>
      </nav>

      {failure ? (
        <div className="banner">
          <span>{failure}</span>
          <button onClick={() => setFailure("")}>Dismiss</button>
        </div>
      ) : null}

      <div className="body">
        {snap.role === "host" ? (
          <HostView snap={snap} api={api} />
        ) : panel === "host" ? (
          <IdleHost snap={snap} api={api} />
        ) : (
          <ClientView snap={snap} api={api} />
        )}

        {snap.role === "idle" ? <Preferences snap={snap} api={api} /> : null}
      </div>
    </main>
  );
}

/** The pre-flight view for the host role: what will be shared, and the code. */
function IdleHost({ snap, api }: { snap: Snapshot; api: Api }) {
  // Reuse the wall canvas for a one-device preview, so the idle screen shows the
  // same picture the layout editor will.
  const preview = useMemo(() => {
    const bounds = boundsOf(snap.displays);
    return {
      owner: snap.device_id,
      devices: [
        {
          id: snap.device_id,
          name: snap.device_name,
          is_host: true,
          connected: true,
          ...bounds,
          displays: snap.displays,
        },
      ],
    };
  }, [snap.displays, snap.device_id, snap.device_name]);

  return (
    <>
      <section className="card">
        <header>
          <h2>Share this machine</h2>
        </header>
        <p className="big">
          This machine's keyboard, mouse and clipboard, on every device you pair.
        </p>
        <p className="code">{snap.pairing_code}</p>
        <p className="muted">
          Start hosting, then enter this code on the other device. Port {snap.port}
          , this network only — nothing leaves your LAN.
        </p>
        <div className="row">
          <button className="primary" onClick={() => api.run("start_host")} disabled={api.busy}>
            Start hosting
          </button>
          <button onClick={() => api.run("regenerate_code")} disabled={api.busy}>
            New code
          </button>
        </div>
      </section>

      <section className="card grow">
        <header>
          <h2>
            This machine's {snap.displays.length === 1 ? "display" : "displays"}
          </h2>
          <span className="pill">{snap.displays.length}</span>
        </header>
        <LayoutCanvas layout={preview} api={api} />
        <p className="muted">
          Detected automatically. Paired devices appear alongside, and you drag
          them to match how they really sit on your desk.
        </p>
      </section>
    </>
  );
}

/** Settings that only take effect when a role starts, so they live on the idle
 *  screen where changing them is free. */
function Preferences({ snap, api }: { snap: Snapshot; api: Api }) {
  return (
    <section className="card">
      <header>
        <h2>Clipboard</h2>
      </header>
      <label className="check">
        <input
          type="checkbox"
          checked={snap.clipboard_sync}
          onChange={(event) => api.run("set_clipboard", { sync: event.currentTarget.checked })}
        />
        Share copied text between devices
      </label>
      <label className="check">
        <input
          type="checkbox"
          checked={snap.clipboard_images}
          disabled={!snap.clipboard_sync}
          onChange={(event) => api.run("set_clipboard", { images: event.currentTarget.checked })}
        />
        Include images
        <span className="muted">slower; a screenshot is megabytes, not bytes</span>
      </label>
      <p className="muted">
        Device id {snap.device_id.slice(0, 8)} · CrossDesk works on your local
        network only, with no account and no relay.
      </p>
    </section>
  );
}

/** Bounding box of a set of monitors, in their own coordinate space. */
function boundsOf(displays: Snapshot["displays"]) {
  if (displays.length === 0) return { x: 0, y: 0, width: 1920, height: 1080 };
  const x = Math.min(...displays.map((d) => d.x));
  const y = Math.min(...displays.map((d) => d.y));
  return {
    x,
    y,
    width: Math.max(...displays.map((d) => d.x + d.width)) - x,
    height: Math.max(...displays.map((d) => d.y + d.height)) - y,
  };
}
