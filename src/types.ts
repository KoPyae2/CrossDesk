// Mirrors the structs the Rust side serialises. Field names are snake_case
// because serde sends them exactly as they are declared.

export type Role = "idle" | "host" | "client";

export interface Display {
  id: number;
  name: string;
  x: number;
  y: number;
  width: number;
  height: number;
  scale: number;
  primary: boolean;
}

export interface LayoutDevice {
  id: string;
  name: string;
  is_host: boolean;
  connected: boolean;
  x: number;
  y: number;
  width: number;
  height: number;
  displays: Display[];
}

export interface LayoutView {
  devices: LayoutDevice[];
  /** Device that currently owns the pointer. */
  owner: string;
}

export interface PeerView {
  id: string;
  name: string;
  os: string;
  latency_ms: number | null;
  displays: Display[];
}

export interface Connection {
  connected: boolean;
  host_id: string;
  host_name: string;
  host_os: string;
  address: string;
  /** True while the host has the pointer on this machine. */
  controlled: boolean;
  message: string;
  attempts: number;
}

export interface Snapshot {
  role: Role;
  device_name: string;
  device_id: string;
  pairing_code: string;
  accepting: boolean;
  clipboard_sync: boolean;
  clipboard_images: boolean;
  port: number;
  /** False where this machine has no input-capture backend, so it can only be a
   *  client. Today that means anything other than Windows. */
  can_host: boolean;
  displays: Display[];
  layout: LayoutView | null;
  peers: PeerView[];
  connection: Connection | null;
}

/** A host that answered a discovery query. */
export interface Found {
  id: string;
  name: string;
  os: string;
  address: string;
  port: number;
  needs_code: boolean;
}

/** The command surface, handed down to the panels. */
export interface Api {
  /** Runs a command and adopts the snapshot it returns. */
  run: (cmd: string, args?: Record<string, unknown>) => Promise<void>;
  /** Layout drags only: no busy flag and no snapshot, so a drag at pointer rate
   *  neither disables the controls nor re-renders the whole window. */
  move: (deviceId: string, x: number, y: number) => Promise<void>;
  busy: boolean;
}
