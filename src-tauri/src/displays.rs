//! Monitor enumeration.
//!
//! Layout changes (docking a laptop, unplugging a screen) are picked up by
//! polling: it costs a single cheap syscall a second and avoids a platform
//! specific display-change event path on three operating systems.

use crate::protocol::Display;

pub fn enumerate() -> Vec<Display> {
    let mut displays: Vec<Display> = display_info::DisplayInfo::all()
        .unwrap_or_default()
        .into_iter()
        .map(|d| Display {
            id: d.id,
            name: if d.friendly_name.is_empty() {
                d.name
            } else {
                d.friendly_name
            },
            x: d.x,
            y: d.y,
            width: d.width,
            height: d.height,
            scale: d.scale_factor,
            primary: d.is_primary,
        })
        .collect();

    // Primary first, then left-to-right: gives the UI a stable order to draw.
    displays.sort_by_key(|d| (!d.primary, d.x, d.y));

    if displays.is_empty() {
        // Headless or a platform we could not query. A single virtual screen
        // keeps the layout maths well defined instead of dividing by zero.
        displays.push(Display {
            id: 0,
            name: "Display".into(),
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            scale: 1.0,
            primary: true,
        });
    }

    displays
}
