// The draggable display wall.
//
// Devices are drawn in wall coordinates scaled to fit the available box, so the
// preview is a true map of where the edges are rather than a diagram. Dragging
// converts back to wall pixels and sends the offset to the host, which is the
// only place that decides anything.
//
// Two details make the drag feel right. The transform is frozen while a drag is
// in flight — otherwise moving a device outward would grow the wall, rescale the
// view, and shift the device out from under the pointer that is moving it. And
// the drag uses pointer capture, so letting go outside the window or off the
// canvas still ends the gesture instead of stranding the device.

import { useEffect, useMemo, useRef, useState } from "react";
import type { PointerEvent as ReactPointerEvent } from "react";
import type { Api, LayoutDevice, LayoutView } from "./types";

/** Space left around the wall inside the canvas, in screen pixels. */
const PADDING = 28;

/** Wall pixels to screen pixels, and where the wall's origin sits. */
interface View {
  scale: number;
  offsetX: number;
  offsetY: number;
}

interface DragState {
  id: string;
  pointerId: number;
  /** The view as it was when the drag began; see the note above. */
  view: View;
  /** Grab point inside the device box, in wall pixels, so the box does not jump
   *  to sit under the pointer. */
  grabX: number;
  grabY: number;
  /** The device's local origin. The UI is given each device's wall *bounds*, but
   *  the backend stores an *offset*; the two differ by exactly this for a device
   *  whose top-left monitor is not at its own local origin. */
  originX: number;
  originY: number;
  /** Where the box is drawn right now. The authoritative position only arrives
   *  with the next snapshot, up to a tick later. */
  x: number;
  y: number;
}

export function LayoutCanvas({ layout, api }: { layout: LayoutView; api: Api }) {
  const boxRef = useRef<HTMLDivElement | null>(null);
  const [size, setSize] = useState({ width: 0, height: 0 });
  const [drag, setDrag] = useState<DragState | null>(null);
  /** Latest position waiting for a frame, so a fast pointer cannot queue
   *  hundreds of commands. */
  const queued = useRef<{ x: number; y: number } | null>(null);
  const frame = useRef<number | null>(null);

  // The canvas is sized by CSS, so its pixel size has to be observed.
  useEffect(() => {
    const box = boxRef.current;
    if (!box) return;
    const observer = new ResizeObserver(([entry]) => {
      const { width, height } = entry.contentRect;
      setSize({ width, height });
    });
    observer.observe(box);
    return () => observer.disconnect();
  }, []);

  useEffect(
    () => () => {
      if (frame.current !== null) cancelAnimationFrame(frame.current);
    },
    [],
  );

  const fitted = useMemo(
    () => fit(layout.devices, size),
    [layout.devices, size],
  );
  const view = drag ? drag.view : fitted;

  // Draw the dragged device where the pointer has it; everything else comes
  // straight from the snapshot.
  const devices = useMemo(
    () =>
      layout.devices.map((d) =>
        drag && d.id === drag.id ? { ...d, x: drag.x, y: drag.y } : d,
      ),
    [layout.devices, drag],
  );

  /** Coalesces moves to one per frame. */
  const send = (id: string, x: number, y: number) => {
    queued.current = { x, y };
    if (frame.current !== null) return;
    frame.current = requestAnimationFrame(() => {
      frame.current = null;
      const next = queued.current;
      queued.current = null;
      if (next) void api.move(id, next.x, next.y);
    });
  };

  const onPointerDown = (event: ReactPointerEvent, device: LayoutDevice) => {
    // The host is the wall's origin; everything else is placed relative to it.
    if (device.is_host || !view) return;
    const box = boxRef.current;
    if (!box) return;
    event.preventDefault();
    event.currentTarget.setPointerCapture(event.pointerId);
    const rect = box.getBoundingClientRect();
    const origin = localOrigin(device);
    setDrag({
      id: device.id,
      pointerId: event.pointerId,
      view,
      grabX: (event.clientX - rect.left - view.offsetX) / view.scale - device.x,
      grabY: (event.clientY - rect.top - view.offsetY) / view.scale - device.y,
      originX: origin.x,
      originY: origin.y,
      x: device.x,
      y: device.y,
    });
  };

  const onPointerMove = (event: ReactPointerEvent) => {
    if (!drag || event.pointerId !== drag.pointerId) return;
    const box = boxRef.current;
    if (!box) return;
    const rect = box.getBoundingClientRect();
    const x = Math.round(
      (event.clientX - rect.left - drag.view.offsetX) / drag.view.scale - drag.grabX,
    );
    const y = Math.round(
      (event.clientY - rect.top - drag.view.offsetY) / drag.view.scale - drag.grabY,
    );
    if (x === drag.x && y === drag.y) return;
    setDrag({ ...drag, x, y });
    send(drag.id, x - drag.originX, y - drag.originY);
  };

  const onPointerUp = async (event: ReactPointerEvent) => {
    if (!drag || event.pointerId !== drag.pointerId) return;
    setDrag(null);
    // Don't leave the final position waiting on a frame that may never render.
    if (frame.current !== null) {
      cancelAnimationFrame(frame.current);
      frame.current = null;
    }
    const last = queued.current;
    queued.current = null;
    if (last) await api.move(drag.id, last.x, last.y);
    // One disk write per gesture, not per frame.
    await api.run("commit_layout");
  };

  return (
    <div className="canvas" ref={boxRef}>
      {view === null
        ? null
        : devices.map((device) => {
            const origin = localOrigin(device);
            return (
              <div
                key={device.id}
                className={[
                  "device",
                  device.is_host ? "is-host" : "",
                  device.connected ? "" : "offline",
                  device.id === layout.owner ? "owner" : "",
                  drag?.id === device.id ? "dragging" : "",
                ]
                  .filter(Boolean)
                  .join(" ")}
                style={{
                  left: device.x * view.scale + view.offsetX,
                  top: device.y * view.scale + view.offsetY,
                  width: device.width * view.scale,
                  height: device.height * view.scale,
                }}
                onPointerDown={(event) => onPointerDown(event, device)}
                onPointerMove={onPointerMove}
                onPointerUp={(event) => void onPointerUp(event)}
                onPointerCancel={(event) => void onPointerUp(event)}
                title={
                  device.is_host
                    ? "This machine. The wall is arranged around it."
                    : "Drag to place this device"
                }
              >
                {/* Monitors individually, so an L-shaped setup reads right. */}
                {device.displays.map((monitor, index) => (
                  <div
                    key={`${monitor.id}-${index}`}
                    className="monitor"
                    style={{
                      left: (monitor.x - origin.x) * view.scale,
                      top: (monitor.y - origin.y) * view.scale,
                      width: monitor.width * view.scale,
                      height: monitor.height * view.scale,
                    }}
                  />
                ))}
                <span className="tag">
                  {device.name}
                  {device.is_host ? " · this machine" : ""}
                  {device.connected ? "" : " · offline"}
                </span>
                <span className="res">
                  {device.width}×{device.height}
                </span>
              </div>
            );
          })}
      {devices.length === 1 ? (
        <p className="hint">
          Connect a device and it will show up here, ready to drag into place.
        </p>
      ) : null}
    </div>
  );
}

/** Scale and offset that fit every device inside the box, centred. */
function fit(devices: LayoutDevice[], size: { width: number; height: number }): View | null {
  if (devices.length === 0 || size.width === 0 || size.height === 0) return null;
  let minX = Infinity;
  let minY = Infinity;
  let maxX = -Infinity;
  let maxY = -Infinity;
  for (const d of devices) {
    minX = Math.min(minX, d.x);
    minY = Math.min(minY, d.y);
    maxX = Math.max(maxX, d.x + d.width);
    maxY = Math.max(maxY, d.y + d.height);
  }
  const wallW = Math.max(maxX - minX, 1);
  const wallH = Math.max(maxY - minY, 1);
  const usableW = Math.max(size.width - PADDING * 2, 1);
  const usableH = Math.max(size.height - PADDING * 2, 1);
  // Never scale up: one small display blown up to fill a large canvas leaves no
  // room for the devices that are about to appear next to it.
  const scale = Math.min(usableW / wallW, usableH / wallH, 1);
  return {
    scale,
    offsetX: (size.width - wallW * scale) / 2 - minX * scale,
    offsetY: (size.height - wallH * scale) / 2 - minY * scale,
  };
}

/** Top-left of the device's own monitor arrangement, in its local pixels. */
function localOrigin(device: LayoutDevice) {
  if (device.displays.length === 0) return { x: 0, y: 0 };
  return {
    x: Math.min(...device.displays.map((d) => d.x)),
    y: Math.min(...device.displays.map((d) => d.y)),
  };
}
