//! The app's own audio-load and voice meters, read without touching the audio
//! thread.
//!
//! Both are values the audio thread already maintains for the app's own
//! monitors.  The SDK remembers the two owning objects when the app builds them
//! and reads the values through `/proc/self/mem`, so a read never faults and
//! never takes a lock the audio thread holds.  Each value is a single aligned
//! word written by the audio thread; at worst a read sees the previous block's
//! value.

use core::sync::atomic::{AtomicUsize, Ordering};

use crate::image;

/// `AudioThread`'s CPU meter: `[audio_thread + 0x180]`.
const AUDIO_THREAD_METER: usize = 0x180;
/// The meter's smoothed load, an `f32` fraction of the block's time budget.
/// `AudioThread::process` stores `0.5 * (elapsed / block_ms) + 0.5 * previous`
/// after every block.
const METER_LOAD: usize = 0x18;

/// `VoiceCountMonitor`'s engine pointer: `[monitor + 0x60]`.
const MONITOR_ENGINE: usize = 0x60;
/// The engine's voice pool: `[engine + 0x598]`.
const ENGINE_VOICES: usize = 0x598;
/// The pool's generator vector: begin and end pointers.
const POOL_BEGIN: usize = 0x70;
/// One generator entry in the pool's vector (1.3.0.12 has one generator).
const GENERATOR_SIZE: usize = 24;
/// The generator's count of playing voices.
const GENERATOR_VOICES: usize = 0x8;
/// More voices than this means the read is not what we think it is.
const MAX_VOICES: u32 = 1024;
/// More generators than this means the read is not what we think it is.
const MAX_GENERATORS: usize = 256;

/// The live `AudioThread`, zero until the app has built it.
static AUDIO_THREAD: AtomicUsize = AtomicUsize::new(0);
/// The live `VoiceCountMonitor`, zero until the app has built it.
static VOICE_MONITOR: AtomicUsize = AtomicUsize::new(0);

/// Remember the audio thread once its constructor has run.
pub(crate) fn remember_audio_thread(thread: usize) {
    AUDIO_THREAD.store(thread, Ordering::Release);
}

/// Remember the voice monitor once its constructor has run.
pub(crate) fn remember_voice_monitor(monitor: usize) {
    VOICE_MONITOR.store(monitor, Ordering::Release);
}

/// The audio load the app measures, as a fraction of the time one audio block
/// may take: 1.0 means a block took its whole budget.
///
/// The app smooths it (half the last block, half the previous value).  `None`
/// until the app has built its audio thread.
#[must_use]
pub fn audio_load() -> Option<f32> {
    let thread = AUDIO_THREAD.load(Ordering::Acquire);
    if thread == 0 {
        return None;
    }
    let meter = image::read_pointer(thread + AUDIO_THREAD_METER)?;
    let load = f32::from_bits(image::read_u32(meter + METER_LOAD)?);
    load_in_range(load)
}

/// Voices playing right now, as the app's own `VoiceCountMonitor` counts them
/// (its `v`): the sum over the engine's generators.  A voice keeps counting
/// through its release tail.
///
/// `None` until the app has built the monitor, or when the pool does not look
/// like a voice pool.
#[must_use]
pub fn voices_playing() -> Option<u32> {
    let monitor = VOICE_MONITOR.load(Ordering::Acquire);
    if monitor == 0 {
        return None;
    }
    let engine = image::read_pointer(monitor + MONITOR_ENGINE)?;
    let pool = image::read_pointer(engine + ENGINE_VOICES)?;
    let begin = image::read_pointer(pool + POOL_BEGIN)?;
    let end = image::read_pointer(pool + POOL_BEGIN + 8)?;
    let bytes = end.checked_sub(begin)?;
    if bytes % GENERATOR_SIZE != 0 || bytes / GENERATOR_SIZE > MAX_GENERATORS {
        return None;
    }
    let mut entries = vec![0_u8; bytes];
    if !image::read(begin, &mut entries) {
        return None;
    }
    count_voices(&entries)
}

/// A load reading the meter could plausibly hold; anything else is a bad read.
fn load_in_range(load: f32) -> Option<f32> {
    (load.is_finite() && (0.0..=64.0).contains(&load)).then_some(load)
}

/// Sum the playing voices over a pool's raw generator entries.
fn count_voices(entries: &[u8]) -> Option<u32> {
    let (generators, rest) = entries.as_chunks::<GENERATOR_SIZE>();
    if !rest.is_empty() {
        return None;
    }
    let mut playing = 0_u32;
    for entry in generators {
        let field = entry.get(GENERATOR_VOICES..GENERATOR_VOICES + 4)?;
        let count = u32::from_le_bytes(field.try_into().ok()?);
        playing = playing.checked_add(count)?;
    }
    (playing <= MAX_VOICES).then_some(playing)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(voices: u32) -> [u8; GENERATOR_SIZE] {
        let mut raw = [0_u8; GENERATOR_SIZE];
        raw[GENERATOR_VOICES..GENERATOR_VOICES + 4].copy_from_slice(&voices.to_le_bytes());
        raw
    }

    #[test]
    fn readings_are_unavailable_before_the_app_builds_its_objects() {
        assert_eq!(audio_load(), None);
        assert_eq!(voices_playing(), None);
    }

    #[test]
    fn voices_sum_over_generators() {
        let entries: Vec<u8> = [entry(1), entry(0), entry(2)].concat();
        assert_eq!(count_voices(&entries), Some(3));
        assert_eq!(count_voices(&[]), Some(0));
    }

    #[test]
    fn a_garbage_pool_is_rejected() {
        assert_eq!(count_voices(&entry(u32::MAX)), None);
        assert_eq!(count_voices(&[0_u8; GENERATOR_SIZE + 1]), None);
    }

    #[test]
    fn load_outside_a_plausible_range_is_rejected() {
        assert_eq!(load_in_range(0.42), Some(0.42));
        assert_eq!(load_in_range(f32::NAN), None);
        assert_eq!(load_in_range(-1.0), None);
        assert_eq!(load_in_range(1e9), None);
    }
}
