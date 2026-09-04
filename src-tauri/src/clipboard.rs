//! Clipboard sharing.
//!
//! There is no clipboard-change notification worth having across three
//! operating systems, so this polls. A hash of the content is compared instead
//! of the content itself, which keeps the common "nothing changed" case down to
//! one clipboard read and one 32-byte compare.
//!
//! The loop-suppression matters more than it looks: writing a received clip
//! into the local clipboard would otherwise be seen as a local change on the
//! next tick and echoed straight back.

use std::time::Duration;

use tokio::sync::mpsc::error::TryRecvError;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use crate::protocol::Clip;

/// Fast enough to feel instant after Ctrl+C, slow enough to be free.
const POLL: Duration = Duration::from_millis(300);

/// Images larger than this are downscaled to ensure they fit within
/// protocol frame limits while never being dropped.
const MAX_IMAGE_BYTES: usize = 24 * 1024 * 1024;

/// Downsamples an RGBA image by a factor of 2 in both dimensions using box averaging.
/// This allows Retina and 4K screenshots (~25MB - 60MB raw RGBA) to safely fit
/// within the frame limit without sacrificing visual clarity or being dropped.
fn downscale_2x(width: u32, height: u32, rgba: &[u8]) -> (u32, u32, Vec<u8>) {
    let new_w = width / 2;
    let new_h = height / 2;
    if new_w == 0 || new_h == 0 {
        return (width, height, rgba.to_vec());
    }
    let mut out = Vec::with_capacity((new_w * new_h * 4) as usize);
    let stride = (width * 4) as usize;

    for ny in 0..new_h {
        let row0 = (ny * 2) as usize * stride;
        let row1 = row0 + stride;
        for nx in 0..new_w {
            let col0 = (nx * 2) as usize * 4;
            let col1 = col0 + 4;

            let p0 = row0 + col0;
            let p1 = row0 + col1;
            let p2 = row1 + col0;
            let p3 = row1 + col1;

            let r = ((rgba[p0] as u32 + rgba[p1] as u32 + rgba[p2] as u32 + rgba[p3] as u32 + 2) / 4) as u8;
            let g = ((rgba[p0 + 1] as u32 + rgba[p1 + 1] as u32 + rgba[p2 + 1] as u32 + rgba[p3 + 1] as u32 + 2) / 4) as u8;
            let b = ((rgba[p0 + 2] as u32 + rgba[p1 + 2] as u32 + rgba[p2 + 2] as u32 + rgba[p3 + 2] as u32 + 2) / 4) as u8;
            let a = ((rgba[p0 + 3] as u32 + rgba[p1 + 3] as u32 + rgba[p2 + 3] as u32 + rgba[p3 + 3] as u32 + 2) / 4) as u8;

            out.push(r);
            out.push(g);
            out.push(b);
            out.push(a);
        }
    }
    (new_w, new_h, out)
}

/// Owns the polling thread. Holding this handle keeps the thread alive.
pub struct Clipboard {
    apply: UnboundedSender<Clip>,
}

impl Clipboard {
    /// Starts watching. `images` turns picture sync on; text is always synced.
    ///
    /// Returns the handle plus a stream of local changes to send to peers.
    /// Failing to open the clipboard is not fatal — the app is still a perfectly
    /// good KVM without it — so the thread just exits and the stream stays quiet.
    pub fn start(images: bool) -> (Clipboard, UnboundedReceiver<Clip>) {
        let (change_tx, change_rx) = unbounded_channel();
        let (apply_tx, mut apply_rx) = unbounded_channel::<Clip>();

        std::thread::Builder::new()
            .name("crossdesk-clipboard".into())
            .spawn(move || {
                // On Windows the clipboard is a single global object that may
                // only be open on one thread, so all of this stays here.
                let Ok(mut board) = arboard::Clipboard::new() else {
                    return;
                };
                let mut last = Fingerprint::None;

                loop {
                    // Apply anything that arrived from a peer first, so its
                    // fingerprint is what the poll below compares against and
                    // the change is not echoed back to where it came from.
                    let mut applied = false;
                    loop {
                        match apply_rx.try_recv() {
                            Ok(clip) => {
                                if write(&mut board, &clip).is_ok() {
                                    last = fingerprint_of(&clip);
                                    applied = true;
                                }
                            }
                            Err(TryRecvError::Empty) => break,
                            // The handle is gone: this role has stopped, so
                            // stop polling rather than leaving a thread behind.
                            Err(TryRecvError::Disconnected) => return,
                        }
                    }
                    if applied {
                        // Some platforms take a moment to make the write
                        // visible; skip one tick rather than read a stale value.
                        std::thread::sleep(POLL);
                        continue;
                    }

                    if let Some(clip) = read(&mut board, images) {
                        let print = fingerprint_of(&clip);
                        if print != last {
                            last = print;
                            if change_tx.send(clip).is_err() {
                                return;
                            }
                        }
                    }

                    std::thread::sleep(POLL);
                }
            })
            .ok();

        (Clipboard { apply: apply_tx }, change_rx)
    }

    /// Puts a peer's clipboard content into the local one.
    pub fn apply(&self, clip: Clip) {
        let _ = self.apply.send(clip);
    }
}

/// Cheap identity for a clipboard payload.
#[derive(PartialEq, Eq)]
enum Fingerprint {
    None,
    Hash([u8; 32]),
}

fn fingerprint_of(clip: &Clip) -> Fingerprint {
    let mut hasher = blake3::Hasher::new();
    match clip {
        Clip::Text(text) => {
            hasher.update(b"t");
            hasher.update(text.as_bytes());
        }
        Clip::Image {
            width,
            height,
            rgba,
        } => {
            hasher.update(b"i");
            hasher.update(&width.to_le_bytes());
            hasher.update(&height.to_le_bytes());
            hasher.update(rgba);
        }
    }
    Fingerprint::Hash(*hasher.finalize().as_bytes())
}

fn read(board: &mut arboard::Clipboard, images: bool) -> Option<Clip> {
    // When images are enabled, check for image data first. On macOS and in
    // modern browsers, copying an image or screenshot often places both an image
    // bitmap and a textual title/URL on the pasteboard. Checking image first prevents
    // the text metadata from shadowing the actual image payload.
    if images {
        if let Ok(image) = board.get_image() {
            let mut width = image.width as u32;
            let mut height = image.height as u32;
            let mut rgba = image.bytes.into_owned();

            // Iteratively downscale large images (e.g. Retina screenshots) so they fit safely within frame limits
            while rgba.len() > MAX_IMAGE_BYTES && width > 1 && height > 1 {
                let (new_w, new_h, new_rgba) = downscale_2x(width, height, &rgba);
                width = new_w;
                height = new_h;
                rgba = new_rgba;
            }

            if rgba.len() <= MAX_IMAGE_BYTES && width > 0 && height > 0 {
                return Some(Clip::Image {
                    width,
                    height,
                    rgba,
                });
            }
        }
    }
    // Plain text: read whenever there is no image (or when images are disabled).
    if let Ok(text) = board.get_text() {
        if !text.is_empty() {
            return Some(Clip::Text(text));
        }
    }
    None
}

fn write(board: &mut arboard::Clipboard, clip: &Clip) -> Result<(), arboard::Error> {
    match clip {
        Clip::Text(text) => board.set_text(text.clone()),
        Clip::Image {
            width,
            height,
            rgba,
        } => board.set_image(arboard::ImageData {
            width: *width as usize,
            height: *height as usize,
            bytes: std::borrow::Cow::Borrowed(rgba),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_downscale_2x_halves_dimensions_and_averages_pixels() {
        // 4x4 image, RGBA
        let width = 4;
        let height = 4;
        let mut rgba = vec![0u8; (width * height * 4) as usize];
        // Fill top-left 2x2 with 100, top-right 2x2 with 200
        for ny in 0..2 {
            for nx in 0..2 {
                let idx = (ny * width + nx) as usize * 4;
                rgba[idx] = 100;
                rgba[idx + 1] = 100;
                rgba[idx + 2] = 100;
                rgba[idx + 3] = 255;
            }
        }
        for ny in 0..2 {
            for nx in 2..4 {
                let idx = (ny * width + nx) as usize * 4;
                rgba[idx] = 200;
                rgba[idx + 1] = 200;
                rgba[idx + 2] = 200;
                rgba[idx + 3] = 255;
            }
        }

        let (new_w, new_h, out) = downscale_2x(width, height, &rgba);
        assert_eq!(new_w, 2);
        assert_eq!(new_h, 2);
        assert_eq!(out.len(), 2 * 2 * 4);
        // Top-left pixel should be 100
        assert_eq!(out[0], 100);
        assert_eq!(out[1], 100);
        assert_eq!(out[2], 100);
        assert_eq!(out[3], 255);
        // Top-right pixel should be 200
        assert_eq!(out[4], 200);
        assert_eq!(out[5], 200);
        assert_eq!(out[6], 200);
        assert_eq!(out[7], 255);
    }
}
