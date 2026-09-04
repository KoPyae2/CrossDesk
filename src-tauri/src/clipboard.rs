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

/// Images above this are dropped rather than sent: a screenshot of a 4K desktop
/// is ~33 MB of RGBA, and pushing that through a latency-critical link to save
/// one paste is the wrong trade.
const MAX_IMAGE_BYTES: usize = 16 * 1024 * 1024;

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
            if image.bytes.len() <= MAX_IMAGE_BYTES {
                return Some(Clip::Image {
                    width: image.width as u32,
                    height: image.height as u32,
                    rgba: image.bytes.into_owned(),
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
