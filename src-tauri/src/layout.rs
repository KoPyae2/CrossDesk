//! Display layout and cursor routing.
//!
//! Every device reports its monitors in its own virtual-desktop pixels. The
//! host places each device's bounding box at an offset inside one shared "wall"
//! coordinate space, so a single cursor position can be expressed globally and
//! translated back into whichever device currently owns the pointer.
//!
//! The host does all of this maths; clients only ever receive final absolute
//! coordinates in their own pixels.

use serde::{Deserialize, Serialize};

use crate::protocol::{DeviceId, Display};

/// Gap left between devices when auto-arranging, in wall pixels. It has to be
/// zero: motion that lands in a gap is kept on the current device, so a gap
/// between two devices is a wall the pointer can never cross.
const AUTO_GAP: i32 = 0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub fn right(&self) -> i32 {
        self.x + self.w
    }

    pub fn bottom(&self) -> i32 {
        self.y + self.h
    }

    pub fn contains(&self, px: i32, py: i32) -> bool {
        px >= self.x && px < self.right() && py >= self.y && py < self.bottom()
    }

    fn union(self, other: Rect) -> Rect {
        let x = self.x.min(other.x);
        let y = self.y.min(other.y);
        Rect {
            x,
            y,
            w: self.right().max(other.right()) - x,
            h: self.bottom().max(other.bottom()) - y,
        }
    }

    /// Nearest point inside this rect, used when a crossing lands in the gap
    /// between two monitors.
    fn clamp(&self, px: i32, py: i32) -> (i32, i32) {
        (
            px.clamp(self.x, self.right() - 1),
            py.clamp(self.y, self.bottom() - 1),
        )
    }

    fn distance_sq(&self, px: i32, py: i32) -> i64 {
        let (cx, cy) = self.clamp(px, py);
        let dx = (px - cx) as i64;
        let dy = (py - cy) as i64;
        dx * dx + dy * dy
    }
}

/// One device as placed on the wall.
#[derive(Clone, Debug)]
pub struct Device {
    pub id: DeviceId,
    pub name: String,
    /// Where this device's local origin sits in wall space.
    pub offset: (i32, i32),
    pub displays: Vec<Display>,
    /// False for a device that is remembered but not currently connected. Such
    /// a device still draws in the layout editor and can still be dragged, but
    /// the pointer must not be able to cross onto it.
    pub online: bool,
    /// Bounding box of `displays`, in the device's own local pixels.
    local_bounds: Rect,
}

impl Device {
    pub fn new(id: DeviceId, name: String, displays: Vec<Display>) -> Self {
        let local_bounds = bounds_of(&displays);
        Self {
            id,
            name,
            offset: (0, 0),
            displays,
            online: true,
            local_bounds,
        }
    }

    pub fn set_displays(&mut self, displays: Vec<Display>) {
        self.local_bounds = bounds_of(&displays);
        self.displays = displays;
    }

    /// Bounding box in wall space.
    pub fn wall_bounds(&self) -> Rect {
        Rect {
            x: self.local_bounds.x + self.offset.0,
            y: self.local_bounds.y + self.offset.1,
            w: self.local_bounds.w,
            h: self.local_bounds.h,
        }
    }

    /// Individual monitors in wall space. Used instead of the bounding box for
    /// hit-testing so an L-shaped setup does not swallow the cursor in its gap.
    pub fn wall_displays(&self) -> impl Iterator<Item = Rect> + '_ {
        self.displays.iter().map(move |d| Rect {
            x: d.x + self.offset.0,
            y: d.y + self.offset.1,
            w: d.width as i32,
            h: d.height as i32,
        })
    }

    pub fn covers(&self, px: i32, py: i32) -> bool {
        self.wall_displays().any(|r| r.contains(px, py))
    }

    pub fn to_local(&self, px: i32, py: i32) -> (i32, i32) {
        (px - self.offset.0, py - self.offset.1)
    }

    /// Closest point on this device to a wall position, snapped onto a real
    /// monitor. Used when entering a device or recovering from a gap.
    pub fn snap(&self, px: i32, py: i32) -> (i32, i32) {
        let best = self
            .wall_displays()
            .min_by_key(|r| r.distance_sq(px, py))
            .unwrap_or_else(|| self.wall_bounds());
        best.clamp(px, py)
    }
}

fn bounds_of(displays: &[Display]) -> Rect {
    let mut iter = displays.iter().map(|d| Rect {
        x: d.x,
        y: d.y,
        w: d.width as i32,
        h: d.height as i32,
    });
    match iter.next() {
        Some(first) => iter.fold(first, Rect::union),
        // A peer that reports nothing still needs a usable placeholder.
        None => Rect {
            x: 0,
            y: 0,
            w: 1920,
            h: 1080,
        },
    }
}

/// Serialisable view of the wall, for the layout editor in the UI.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct LayoutView {
    pub devices: Vec<LayoutDevice>,
    /// Hex id of the device that currently owns the pointer.
    pub owner: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct LayoutDevice {
    pub id: String,
    pub name: String,
    pub is_host: bool,
    pub connected: bool,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub displays: Vec<Display>,
}

/// The set of devices sharing one pointer, plus who currently owns it.
pub struct Wall {
    pub devices: Vec<Device>,
    /// Index into `devices`; the host is always index 0.
    owner: usize,
    /// Current pointer position in wall space.
    cursor: (i32, i32),
    /// While set, the pointer cannot leave the device that owns it. The host
    /// sets this during a drag: letting go of a mouse button on a different
    /// machine from where it was pressed is never what the user meant.
    locked: bool,
}

/// What the host should do after feeding motion into the wall.
#[derive(Debug, PartialEq, Eq)]
pub enum Motion {
    /// Pointer stayed on the host; let the real cursor move normally.
    Local { x: i32, y: i32 },
    /// Pointer stayed on the same remote device.
    Remote { device: usize, x: i32, y: i32 },
    /// Pointer crossed onto a different device.
    Switch { device: usize, x: i32, y: i32 },
}

impl Wall {
    pub fn new(host: Device) -> Self {
        let cursor = host.snap(0, 0);
        Self {
            devices: vec![host],
            owner: 0,
            cursor,
            locked: false,
        }
    }

    pub fn host(&self) -> &Device {
        &self.devices[0]
    }

    pub fn owner(&self) -> usize {
        self.owner
    }

    pub fn is_local(&self) -> bool {
        self.owner == 0
    }

    pub fn index_of(&self, id: &DeviceId) -> Option<usize> {
        self.devices.iter().position(|d| &d.id == id)
    }

    /// Adds a device, or refreshes an existing one, and returns its index.
    /// A device that was placed before keeps its offset across reconnects.
    pub fn upsert(&mut self, id: DeviceId, name: String, displays: Vec<Display>) -> usize {
        if let Some(idx) = self.index_of(&id) {
            let device = &mut self.devices[idx];
            device.name = name;
            device.online = true;
            device.set_displays(displays);
            return idx;
        }
        let mut device = Device::new(id, name, displays);
        device.offset = self.next_free_offset(&device);
        self.devices.push(device);
        self.devices.len() - 1
    }

    /// Marks a device connected or not. A device that goes offline while it owns
    /// the pointer hands it straight back, so a client dropping off the WiFi
    /// cannot take the keyboard with it.
    pub fn set_online(&mut self, idx: usize, online: bool) {
        if idx == 0 || idx >= self.devices.len() {
            return;
        }
        self.devices[idx].online = online;
        if !online && self.owner == idx {
            self.take_local();
        }
    }

    /// Places a new device immediately to the right of everything placed so
    /// far, vertically centred against the host — the arrangement people
    /// expect before they drag anything.
    fn next_free_offset(&self, incoming: &Device) -> (i32, i32) {
        let occupied = self
            .devices
            .iter()
            .map(|d| d.wall_bounds())
            .reduce(Rect::union);
        let Some(occupied) = occupied else {
            return (0, 0);
        };
        let host = self.host().wall_bounds();
        let incoming_bounds = incoming.local_bounds;
        (
            occupied.right() + AUTO_GAP - incoming_bounds.x,
            host.y + (host.h - incoming_bounds.h) / 2 - incoming_bounds.y,
        )
    }

    pub fn remove(&mut self, idx: usize) {
        if idx == 0 || idx >= self.devices.len() {
            return;
        }
        self.devices.remove(idx);
        if self.owner == idx {
            // Whoever had the pointer just went away; take it back.
            self.take_local();
        } else if self.owner > idx {
            self.owner -= 1;
        }
    }

    pub fn set_offset(&mut self, idx: usize, x: i32, y: i32) {
        if let Some(device) = self.devices.get_mut(idx) {
            device.offset = (x, y);
        }
    }

    /// Re-runs the default left-to-right arrangement for every non-host device.
    pub fn auto_arrange(&mut self) {
        let host_bounds = self.host().wall_bounds();
        let mut cursor_x = host_bounds.right() + AUTO_GAP;
        for idx in 1..self.devices.len() {
            let local = self.devices[idx].local_bounds;
            self.devices[idx].offset = (
                cursor_x - local.x,
                host_bounds.y + (host_bounds.h - local.h) / 2 - local.y,
            );
            cursor_x += local.w + AUTO_GAP;
        }
    }

    /// Snaps every device's edges to nearby neighbours so dragging in the UI
    /// still produces a seamless wall.
    ///
    /// The two axes are resolved independently. A device dropped roughly to the
    /// right of another one usually wants both: its left edge against the
    /// neighbour's right edge, and its top edge lined up with the neighbour's.
    /// Picking a single best shift would do one and leave the other a few pixels
    /// out, which is exactly the gap the pointer cannot cross.
    pub fn snap_edges(&mut self, threshold: i32) {
        for idx in 1..self.devices.len() {
            let mine = self.devices[idx].wall_bounds();
            let mut best_dx: Option<i32> = None;
            let mut best_dy: Option<i32> = None;

            for (other_idx, other) in self.devices.iter().enumerate() {
                if other_idx == idx {
                    continue;
                }
                let theirs = other.wall_bounds();
                // Butt against either of their vertical edges, or line up with
                // their left edge for a device sitting above or below.
                for dx in [
                    theirs.right() - mine.x,
                    theirs.x - mine.right(),
                    theirs.x - mine.x,
                ] {
                    if dx.abs() <= threshold && best_dx.map_or(true, |b| dx.abs() < b.abs()) {
                        best_dx = Some(dx);
                    }
                }
                for dy in [
                    theirs.bottom() - mine.y,
                    theirs.y - mine.bottom(),
                    theirs.y - mine.y,
                ] {
                    if dy.abs() <= threshold && best_dy.map_or(true, |b| dy.abs() < b.abs()) {
                        best_dy = Some(dy);
                    }
                }
            }

            let device = &mut self.devices[idx];
            device.offset = (
                device.offset.0 + best_dx.unwrap_or(0),
                device.offset.1 + best_dy.unwrap_or(0),
            );
        }
    }

    /// Forces ownership (used when a device disconnects or the user presses the
    /// "return to this machine" hotkey).
    pub fn take_local(&mut self) {
        self.owner = 0;
        self.locked = false;
        self.cursor = self.host().snap(self.cursor.0, self.cursor.1);
    }

    /// Pins the pointer to whichever device owns it. Held while any mouse button
    /// is down so a drag cannot be torn in half by an edge crossing.
    pub fn set_locked(&mut self, locked: bool) {
        self.locked = locked;
    }

    /// Called with the host's real cursor position while control is local, to
    /// keep the wall position in sync with whatever else moved the pointer.
    pub fn sync_local(&mut self, local_x: i32, local_y: i32) {
        if self.owner == 0 {
            let host = self.host();
            self.cursor = (local_x + host.offset.0, local_y + host.offset.1);
        }
    }

    /// Feeds relative pointer motion into the wall and reports where it landed.
    ///
    /// Motion is applied in wall space, so crossing a device boundary is just a
    /// containment test — there is no special "edge" state to get stuck in.
    pub fn move_by(&mut self, dx: i32, dy: i32) -> Motion {
        let (mut nx, mut ny) = (self.cursor.0 + dx, self.cursor.1 + dy);

        let candidate = if self.locked {
            // Mid-drag: stay put whatever the maths says.
            None
        } else {
            self.devices
                .iter()
                .position(|d| d.online && d.covers(nx, ny))
        };

        let target = match candidate {
            Some(idx) => idx,
            None => {
                // Landed in a gap, off the far edge, or locked mid-drag: keep
                // the pointer on its current device, which is what a
                // single-machine desktop does.
                let owner = self.owner;
                let snapped = self.devices[owner].snap(nx, ny);
                nx = snapped.0;
                ny = snapped.1;
                owner
            }
        };

        self.cursor = (nx, ny);
        let (lx, ly) = self.devices[target].to_local(nx, ny);

        if target != self.owner {
            self.owner = target;
            Motion::Switch {
                device: target,
                x: lx,
                y: ly,
            }
        } else if target == 0 {
            Motion::Local { x: lx, y: ly }
        } else {
            Motion::Remote {
                device: target,
                x: lx,
                y: ly,
            }
        }
    }

    /// Local coordinates of the current pointer on the device that owns it.
    pub fn owner_local(&self) -> (i32, i32) {
        self.devices[self.owner].to_local(self.cursor.0, self.cursor.1)
    }

    pub fn view(&self) -> LayoutView {
        LayoutView {
            owner: crate::protocol::id_to_hex(&self.devices[self.owner].id),
            devices: self
                .devices
                .iter()
                .enumerate()
                .map(|(idx, d)| {
                    let bounds = d.wall_bounds();
                    LayoutDevice {
                        id: crate::protocol::id_to_hex(&d.id),
                        name: d.name.clone(),
                        is_host: idx == 0,
                        connected: idx == 0 || d.online,
                        x: bounds.x,
                        y: bounds.y,
                        width: bounds.w,
                        height: bounds.h,
                        displays: d.displays.clone(),
                    }
                })
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn display(x: i32, y: i32, w: u32, h: u32) -> Display {
        Display {
            id: 0,
            name: "test".into(),
            x,
            y,
            width: w,
            height: h,
            scale: 1.0,
            primary: true,
        }
    }

    fn wall() -> Wall {
        let host = Device::new([1u8; 16], "host".into(), vec![display(0, 0, 1920, 1080)]);
        let mut wall = Wall::new(host);
        wall.upsert([2u8; 16], "client".into(), vec![display(0, 0, 1280, 720)]);
        wall
    }

    #[test]
    fn new_device_lands_to_the_right() {
        let wall = wall();
        assert_eq!(wall.devices[1].wall_bounds().x, 1920);
    }

    #[test]
    fn crossing_the_right_edge_switches_device() {
        let mut wall = wall();
        wall.sync_local(1919, 500);
        assert_eq!(
            wall.move_by(5, 0),
            Motion::Switch {
                device: 1,
                x: 4,
                y: 500 - wall.devices[1].offset.1
            }
        );
        assert!(!wall.is_local());
    }

    #[test]
    fn moving_back_returns_control_to_the_host() {
        let mut wall = wall();
        wall.sync_local(1919, 500);
        wall.move_by(5, 0);
        let back = wall.move_by(-10, 0);
        assert!(matches!(back, Motion::Switch { device: 0, .. }));
        assert!(wall.is_local());
    }

    #[test]
    fn motion_into_a_gap_stays_on_the_current_device() {
        let mut wall = wall();
        wall.sync_local(100, 100);
        // Straight up, off the top of every monitor.
        assert!(matches!(wall.move_by(0, -400), Motion::Local { y: 0, .. }));
        assert!(wall.is_local());
    }

    #[test]
    fn a_disconnecting_device_hands_the_pointer_back() {
        let mut wall = wall();
        wall.sync_local(1919, 500);
        wall.move_by(5, 0);
        wall.remove(1);
        assert!(wall.is_local());
        assert_eq!(wall.devices.len(), 1);
    }

    #[test]
    fn an_offline_device_cannot_be_crossed_onto() {
        let mut wall = wall();
        wall.set_online(1, false);
        wall.sync_local(1919, 500);
        // The far edge is now the end of the wall, so the pointer stops there
        // instead of landing on a device that cannot hear about it.
        assert!(matches!(wall.move_by(5, 0), Motion::Local { x: 1919, .. }));
        assert!(wall.is_local());
    }

    #[test]
    fn a_drag_pins_the_pointer_to_one_device() {
        let mut wall = wall();
        wall.sync_local(1919, 500);
        wall.set_locked(true);
        assert!(matches!(wall.move_by(50, 0), Motion::Local { .. }));
        assert!(wall.is_local());
        // Releasing the button lets the next sample cross as usual.
        wall.set_locked(false);
        assert!(matches!(wall.move_by(50, 0), Motion::Switch { device: 1, .. }));
    }

    #[test]
    fn snapping_closes_the_gap_on_both_axes() {
        let mut wall = wall();
        // Dropped down and to the right of the host, close but not touching.
        wall.set_offset(1, 1970, 60);
        wall.snap_edges(120);
        let bounds = wall.devices[1].wall_bounds();
        // Left edge against the host's right edge, tops aligned. A gap on either
        // axis would be a boundary the pointer could never cross.
        assert_eq!((bounds.x, bounds.y), (1920, 0));
    }

    #[test]
    fn a_device_that_reconnects_keeps_its_place() {
        let mut wall = wall();
        wall.set_offset(1, 500, 1080);
        wall.set_online(1, false);
        let idx = wall.upsert([2u8; 16], "client".into(), vec![display(0, 0, 1280, 720)]);
        assert_eq!(idx, 1);
        assert_eq!(wall.devices[1].offset, (500, 1080));
        assert!(wall.devices[1].online);
    }
}
