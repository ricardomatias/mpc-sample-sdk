//! The stock SHIFT + pad shortcuts, run without the chord.
//!
//! With SHIFT held, a pad press is a page shortcut: PAD 8 opens MIDI Config,
//! PAD 16 the Project page, and so on.  The app routes each one to a single
//! member of its control-surface behaviour object, which checks the current
//! page and posts the page change to the UI thread's queue.  The SDK records
//! that object when the app builds it, so a mod can run a shortcut exactly as
//! the chord would, for instance after it has taken the chord for itself.

use core::sync::atomic::{AtomicUsize, Ordering};

use crate::image;

/// `AC50ControlSurfaceBehaviour`'s SHIFT + pad handler: `(this, pad)`.
const SHIFT_PAD: usize = 0x014a_d170;

/// The live `AC50ControlSurfaceBehaviour`, zero until the app has built it.
static BEHAVIOUR: AtomicUsize = AtomicUsize::new(0);

/// PAD 8 (from zero), the stock MIDI Config shortcut.
pub const MIDI_SETTINGS_PAD: u8 = 7;

/// Remember the behaviour object once its constructor has run.
pub(crate) fn remember_behaviour(behaviour: usize) {
    BEHAVIOUR.store(behaviour, Ordering::Release);
}

/// Whether the app has built the object the shortcuts run on.
#[must_use]
pub fn ready() -> bool {
    BEHAVIOUR.load(Ordering::Acquire) != 0
}

/// Run the stock SHIFT + pad shortcut for `pad` (0 to 15, PAD 1 is 0).
///
/// Returns `false` when the app has not built its control-surface behaviour
/// yet or `pad` is out of range.  Like the chord, it only queues the page
/// change: the page opens on the UI thread's next turn, and the app ignores it
/// where the chord would be ignored.
#[must_use]
pub fn shift_pad(pad: u8) -> bool {
    let behaviour = BEHAVIOUR.load(Ordering::Acquire);
    if behaviour == 0 || pad > 15 {
        return false;
    }
    #[cfg(target_arch = "aarch64")]
    {
        let address = image::base() + SHIFT_PAD;
        // SAFETY: `SHIFT_PAD` is the 1.3.0.12 handler, verified by the loader's
        // firmware words; `behaviour` is the object the app built and keeps for
        // its lifetime.  The handler posts to a queue, so any thread may call it.
        let handler: unsafe extern "C" fn(usize, u32) =
            unsafe { core::mem::transmute::<usize, _>(address) };
        // SAFETY: as above.
        unsafe { handler(behaviour, u32::from(pad)) };
        true
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        let _ = (image::base(), SHIFT_PAD);
        false
    }
}

/// Open the stock MIDI Config page, as SHIFT + PAD 8 does.
#[must_use]
pub fn open_midi_settings() -> bool {
    shift_pad(MIDI_SETTINGS_PAD)
}
