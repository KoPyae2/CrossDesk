// Host role UI: the pairing code, the wall, and who is connected.

import { LayoutCanvas } from "./LayoutCanvas";
import type { Api, Snapshot } from "./types";

export function HostView({ snap, api }: { snap: Snapshot; api: Api }) {
  return (
    <>
      <section className="card">
        <header>
          <h2>Pairing code</h2>
          <span className={snap.accepting ? "pill on" : "pill"}>
            {snap.accepting ? "open to new devices" : "closed"}
          </span>
        </header>
        <p className="code">{snap.pairing_code}</p>
        <p className="muted">
          Type this on the other device together with this machine's address.
          Listening on port {snap.port}. Devices that have paired once reconnect
          on their own, so you can close the window afterwards.
        </p>
        <div className="row">
          <button onClick={() => api.run("regenerate_code")} disabled={api.busy}>
            New code
          </button>
          <button
            onClick={() => api.run("set_accepting", { accepting: !snap.accepting })}
            disabled={api.busy}
          >
            {snap.accepting ? "Stop accepting" : "Accept new devices"}
          </button>
          <button className="danger" onClick={() => api.run("stop")} disabled={api.busy}>
            Stop hosting
          </button>
        </div>
      </section>

      <section className="card grow">
        <header>
          <h2>Display layout</h2>
          {snap.layout && !isLocal(snap) ? (
            <span className="pill on">pointer is away</span>
          ) : null}
        </header>
        {snap.layout ? <LayoutCanvas layout={snap.layout} api={api} /> : null}
        <p className="muted">
          Drag a device to say where it sits. Push the mouse off that edge and it
          keeps going onto that screen. <kbd>Ctrl</kbd>+<kbd>Alt</kbd>+
          <kbd>Home</kbd> brings the pointer straight back here.
        </p>
        <div className="row">
          <button onClick={() => api.run("snap_layout")} disabled={api.busy}>
            Snap to edges
          </button>
          <button onClick={() => api.run("auto_arrange")} disabled={api.busy}>
            Auto arrange
          </button>
          <button onClick={() => api.run("recall_pointer")} disabled={api.busy}>
            Bring pointer back
          </button>
        </div>
      </section>

      <section className="card">
        <header>
          <h2>Connected devices</h2>
          <span className="pill">{snap.peers.length}</span>
        </header>
        {snap.peers.length === 0 ? (
          <p className="muted">
            Nothing connected yet. Start CrossDesk on another device, choose
            Connect, and enter the code above.
          </p>
        ) : (
          <ul className="peers">
            {snap.peers.map((peer) => (
              <li key={peer.id}>
                <div>
                  <strong>{peer.name}</strong>
                  <span className="muted">
                    {peer.os} · {peer.displays.length}{" "}
                    {peer.displays.length === 1 ? "display" : "displays"}
                  </span>
                </div>
                <span className="latency">
                  {peer.latency_ms === null ? "—" : `${peer.latency_ms} ms`}
                </span>
                <button
                  className="danger"
                  onClick={() => api.run("forget_device", { deviceId: peer.id })}
                  disabled={api.busy}
                  title="Drop this device and forget its key"
                >
                  Forget
                </button>
              </li>
            ))}
          </ul>
        )}
      </section>
    </>
  );
}

/** True while the pointer is on this machine. */
function isLocal(snap: Snapshot) {
  const layout = snap.layout;
  if (!layout) return true;
  return layout.devices.some((d) => d.is_host && d.id === layout.owner);
}
