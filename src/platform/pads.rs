//! Pad occupancy as the app's own pad finder sees it.
//!
//! Extract needs a free pad, and the mod has to know how many there are before
//! and after it runs an extraction: the difference is the only reliable signal
//! that the app actually assigned the sample.

use crate::image;

/// The app's "count free pads" entry, as an offset into the program object's
/// vtable call.
const FREE_PAD: usize = 0x286_dd00;
/// The app's "is this pad state busy" entry.
const PAD_BUSY: usize = 0x280_47b0;
/// `[program + 0x2808]` is the first pad state, `+0x2810` the last.
const PAD_STATES: usize = 0x2808;
/// The app never has more pads than this.
const MAX_PADS: usize = 128;
/// `[surface + 0x2808]` is the first pad state, `+0x2810` the last.
const SURFACE_PADS: usize = 0x2808;
const SURFACE_PADS_END: usize = 0x2810;
/// `[pad state + 0x22e8]` is the layer list, `[layers + 0x4a8]` its sample.
const STATE_LAYERS: usize = 0x22e8;
const LAYER_SAMPLE: usize = 0x4a8;
/// `InstrumentLayer::active`, confirmed in the firmware's native serializer.
const LAYER_ACTIVE: usize = 0xb18;

/// The app's count of free pads.
///
/// # Safety
///
/// `program` must be the live program object on its owning UI thread.
#[must_use]
pub unsafe fn free_count(program: usize) -> i32 {
    // SAFETY: `base + FREE_PAD` is the entry the app itself calls with its
    // program object, and the caller guarantees the object.
    unsafe { image::int_of_pointer(image::base() + FREE_PAD, program) }
}

/// Whether a known program has at least `required` free pads.
///
/// An unknown program (`None` or zero) cannot be proven to have room, so the
/// answer is `false`; the caller reports the same "need N empty pads" message.
///
/// # Safety
///
/// A supplied program must be the live program object on its owning UI thread.
#[must_use]
pub unsafe fn has_room(program: Option<usize>, required: usize) -> bool {
    program
        .filter(|program| *program != 0)
        // SAFETY: the caller guarantees the live program object.
        .is_some_and(|program| unsafe { free_estimate(program) } >= required)
}

/// How many pads the app's pad states say are free.
///
/// The same two checks the native finder uses: a null pad state, or a pad state
/// the app reports as not busy.
///
/// # Safety
///
/// `program` must be the live program object on its owning UI thread.
#[must_use]
pub unsafe fn free_estimate(program: usize) -> usize {
    let Some(first) = image::read_pointer(program + PAD_STATES) else {
        return 0;
    };
    let Some(last) = image::read_pointer(program + PAD_STATES + 8) else {
        return 0;
    };
    if first == 0 || last < first {
        return 0;
    }
    let count = ((last - first) / 8).min(MAX_PADS);
    let mut free = 0;
    for index in 0..count {
        let Some(state) = image::read_pointer(first + index * 8) else {
            break;
        };
        if state == 0 {
            free += 1;
            continue;
        }
        // SAFETY: the caller supplied the live program, and `state` is read out
        // of its own pad table; `base + PAD_BUSY` is the app's own predicate.
        let busy = unsafe { image::int_of_pointer(image::base() + PAD_BUSY, state) };
        if busy == 0 {
            free += 1;
        }
    }
    free
}

/// The layer object at a pad, read through the surface's own tables.
fn layer_for_pad(surface: usize, pad: i32) -> Option<usize> {
    if surface == 0 {
        return None;
    }
    let array = image::read_pointer(surface.checked_add(SURFACE_PADS)?)?;
    let end = image::read_pointer(surface.checked_add(SURFACE_PADS_END)?)?;
    if array == 0 || end < array {
        return None;
    }
    let index = usize::try_from(pad).ok()?;
    if index >= MAX_PADS || (end - array) / 8 <= index {
        return None;
    }
    let state = image::read_pointer(array.checked_add(index.checked_mul(8)?)?)?;
    if state == 0 {
        return None;
    }
    image::read_pointer(state.checked_add(STATE_LAYERS)?)
}

/// The sample a pad holds, read through the surface's own tables.
#[must_use]
pub fn sample_for_pad(surface: usize, pad: i32) -> Option<usize> {
    let layer = layer_for_pad(surface, pad)?;
    image::read_pointer(layer.checked_add(LAYER_SAMPLE)?).filter(|sample| *sample != 0)
}

/// Enable or disable a populated pad's sample layer.
///
/// Returns `false` if the pad table, layer, or memory write is unavailable. The
/// update is visible to pad playback immediately and is saved with the project.
///
/// # Safety
///
/// `surface` must be the live control surface on its owning UI thread, and the
/// caller must have exclusive access to the layer while it is written.
#[must_use]
pub unsafe fn set_layer_active(surface: usize, pad: i32, active: bool) -> bool {
    let Some(layer) = layer_for_pad(surface, pad) else {
        return false;
    };
    let Some(sample_address) = layer.checked_add(LAYER_SAMPLE) else {
        return false;
    };
    if image::read_pointer(sample_address).is_none_or(|sample| sample == 0) {
        return false;
    }
    let Some(address) = layer.checked_add(LAYER_ACTIVE) else {
        return false;
    };
    let expected = [u8::from(active)];
    // SAFETY: the caller guarantees the live layer and exclusive access.
    if !unsafe { image::write(address, &expected) } {
        return false;
    }
    let mut actual = [0];
    image::read(address, &mut actual) && actual == expected
}

/// SAMPLE SELECT's button code.
pub const SAMPLE_SELECT: u32 = 36;

/// Makes a pad pressed while SAMPLE SELECT is held the current pad without
/// sounding it.
///
/// A pad press reaches the app twice: first on its control-surface path
/// ([`crate::Mod::on_surface`]), which makes the pad current, then on its note
/// path ([`crate::Mod::on_pad`]), which sounds it and sends it to MIDI OUT.
/// The stock SAMPLE + pad chord never calls the note path; this does the same
/// for SAMPLE SELECT + pad by consuming the note path's press, and the release
/// that belongs to it, while forwarding the surface event untouched.  Nothing
/// is called in the app: a silenced pad costs less than a sounded one.
///
/// The mod owns one and asks it about every pad event:
///
/// ```
/// use mpc_sample_sdk::platform::pads::SilentSelect;
/// use mpc_sample_sdk::{Mod, PadEvent, PadFlow};
///
/// #[derive(Default)]
/// struct Picker {
///     silent: SilentSelect,
/// }
///
/// impl Mod for Picker {
///     fn on_pad(&mut self, event: PadEvent) -> PadFlow {
///         self.silent.on_pad(&event, mpc_sample_sdk::event::sample_select())
///     }
/// }
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SilentSelect {
    /// Pads whose press was consumed and whose release is still due.
    pending: u128,
}

impl SilentSelect {
    /// No pad silenced.
    #[must_use]
    pub const fn new() -> Self {
        Self { pending: 0 }
    }

    /// Decide one note-path pad event.
    ///
    /// `sample_select_held` is whether SAMPLE SELECT is down
    /// ([`crate::event::sample_select`]).  A press while it is held is
    /// consumed, and so is that pad's next release, even if SAMPLE SELECT was
    /// let go in between.  Everything else is forwarded.
    pub fn on_pad(&mut self, event: &crate::PadEvent, sample_select_held: bool) -> crate::PadFlow {
        let Some(bit) = pad_bit(event.pad) else {
            return crate::PadFlow::Forward;
        };
        match event.action {
            crate::PadAction::Press if sample_select_held => {
                self.pending |= bit;
                crate::PadFlow::Consume
            }
            crate::PadAction::Release if self.pending & bit != 0 => {
                self.pending &= !bit;
                crate::PadFlow::Consume
            }
            crate::PadAction::Press | crate::PadAction::Release => crate::PadFlow::Forward,
        }
    }

    /// Whether `pad` was selected silently and its release is still due.
    #[must_use]
    pub fn is_silenced(&self, pad: i32) -> bool {
        pad_bit(pad).is_some_and(|bit| self.pending & bit != 0)
    }
}

/// The bit for a pad index the app reports, or `None` out of range.
fn pad_bit(pad: i32) -> Option<u128> {
    let index = u32::try_from(pad).ok().filter(|index| *index < 128)?;
    Some(1_u128 << index)
}

#[cfg(test)]
mod tests {
    use super::{FREE_PAD, MAX_PADS, PAD_BUSY, PAD_STATES, SilentSelect, has_room};
    use crate::{PadAction, PadEvent, PadFlow};

    fn pad(action: PadAction, pad: i32) -> PadEvent {
        PadEvent {
            action,
            input: 0,
            source: 0,
            pad,
            properties: 0,
            velocity: if action == PadAction::Press { 0.8 } else { 0.0 },
        }
    }

    #[test]
    fn a_pad_pressed_under_sample_select_is_silent_until_its_release() {
        let mut silent = SilentSelect::new();
        assert_eq!(
            silent.on_pad(&pad(PadAction::Press, 4), true),
            PadFlow::Consume
        );
        assert!(silent.is_silenced(4));
        assert_eq!(
            silent.on_pad(&pad(PadAction::Release, 4), true),
            PadFlow::Consume
        );
        assert!(!silent.is_silenced(4));
        assert_eq!(silent, SilentSelect::new());
    }

    #[test]
    fn the_release_stays_silent_after_sample_select_is_let_go() {
        let mut silent = SilentSelect::new();
        assert_eq!(
            silent.on_pad(&pad(PadAction::Press, 0), true),
            PadFlow::Consume
        );
        assert_eq!(
            silent.on_pad(&pad(PadAction::Release, 0), false),
            PadFlow::Consume
        );
        // The next press is an ordinary one again.
        assert_eq!(
            silent.on_pad(&pad(PadAction::Press, 0), false),
            PadFlow::Forward
        );
        assert_eq!(
            silent.on_pad(&pad(PadAction::Release, 0), false),
            PadFlow::Forward
        );
    }

    #[test]
    fn pads_without_sample_select_sound_as_usual() {
        let mut silent = SilentSelect::new();
        assert_eq!(
            silent.on_pad(&pad(PadAction::Press, 2), false),
            PadFlow::Forward
        );
        // A release of a pad pressed before SAMPLE SELECT went down sounds off.
        assert_eq!(
            silent.on_pad(&pad(PadAction::Release, 2), true),
            PadFlow::Forward
        );
    }

    #[test]
    fn each_pad_is_tracked_on_its_own() {
        let mut silent = SilentSelect::new();
        assert_eq!(
            silent.on_pad(&pad(PadAction::Press, 1), false),
            PadFlow::Forward
        );
        assert_eq!(
            silent.on_pad(&pad(PadAction::Press, 15), true),
            PadFlow::Consume
        );
        assert_eq!(
            silent.on_pad(&pad(PadAction::Release, 1), true),
            PadFlow::Forward
        );
        assert_eq!(
            silent.on_pad(&pad(PadAction::Release, 15), false),
            PadFlow::Consume
        );
    }

    #[test]
    fn out_of_range_pads_are_forwarded() {
        let mut silent = SilentSelect::new();
        assert_eq!(
            silent.on_pad(&pad(PadAction::Press, -1), true),
            PadFlow::Forward
        );
        assert_eq!(
            silent.on_pad(&pad(PadAction::Press, 128), true),
            PadFlow::Forward
        );
        assert_eq!(
            silent.on_pad(&pad(PadAction::Press, 127), true),
            PadFlow::Consume
        );
        assert!(!silent.is_silenced(-1));
    }

    #[test]
    fn an_unknown_program_has_no_proven_room() {
        // SAFETY: no program is handed to native code in either case.
        unsafe {
            assert!(!has_room(None, 1));
            assert!(!has_room(Some(0), 1));
        }
    }

    #[test]
    fn the_offsets_are_the_documented_ones() {
        assert_eq!(FREE_PAD, 0x286_dd00);
        assert_eq!(PAD_BUSY, 0x280_47b0);
        assert_eq!(PAD_STATES, 0x2808);
        assert_eq!(MAX_PADS, 128);
    }
}
