//! Run a function on the app's UI (message) thread.
//!
//! Buttons, pads and the encoder reach a mod on the control-surface thread, but
//! the app changes its screens only on the message thread.  The stock
//! control-surface behaviour bridges the two with its `ThreadQueues`: the
//! SHIFT + pad handler (`0x14ad170`) pushes a small type-erased job,
//! `{invoker, manager, data[2]}`, onto the UI queue, and the message thread
//! runs it on its next turn.  [`post`] pushes a job the same way, on the same
//! queue, with the app's own trivial manager.
//!
//! The job's first data word is the behaviour's lifetime token, as in the
//! stock jobs: its count (`token + 8`) is raised before the push and dropped
//! once the job ran (or the push failed), so the behaviour knows a job of its
//! own is still queued.

use core::sync::atomic::{AtomicI32, Ordering};

use crate::image;

/// `behaviour + 0x18` is its lifetime token; `[token]` is the app's
/// `ThreadQueues`.
const BEHAVIOUR_TOKEN: usize = 0x18;
/// The token's count of jobs in flight, an `i32`.
const TOKEN_COUNT: usize = 0x8;
/// `ThreadQueues + 0x30` is the UI thread's queue.
const UI_QUEUE: usize = 0x30;
/// `[queue + 0x180]` is the `pthread_key_t` of a thread's own producer queue.
const QUEUE_KEY: usize = 0x180;
/// The queue's push: vtable slot 2, `(queue, &job, token) -> bool`.
const PUSH_SLOT: usize = 0x10;
/// The app's manager for a trivially copyable 16-byte job (op 0 copies).
pub const TRIVIAL_MANAGER: usize = 0x14a_a3f0;

/// One queued job as the app lays it out.
#[repr(C)]
struct Job {
    invoke: unsafe extern "C" fn(*mut [usize; 2]),
    manage: usize,
    data: [usize; 2],
}

/// The invoker the UI thread calls: run the posted function, then release the
/// behaviour's token as the stock invokers do.
///
/// # Safety
///
/// The queue calls this once with the job's data, copied from [`post`].
unsafe extern "C" fn invoke(data: *mut [usize; 2]) {
    if data.is_null() {
        return;
    }
    // SAFETY: the queue hands back the two words `post` stored.
    let [token, job] = unsafe { *data };
    if job != 0 {
        // SAFETY: `post` stored a `fn()` here.
        let job: fn() = unsafe { core::mem::transmute::<usize, fn()>(job) };
        job();
    }
    release(token);
}

/// Drop one job from the token's count.
fn release(token: usize) {
    if token != 0 {
        // SAFETY: the token is the behaviour's, which lives for the process;
        // the app updates this count atomically.
        let count = unsafe { AtomicI32::from_ptr((token + TOKEN_COUNT) as *mut i32) };
        count.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Run `job` on the app's UI thread on its next turn.
///
/// Returns `false` when the app has not built its control-surface behaviour
/// yet or its queue refused the job; `job` then never runs.  Any thread may
/// call it; the stock app posts from the control-surface thread the same way.
#[must_use]
pub fn post(job: fn()) -> bool {
    let behaviour = super::shortcut::behaviour();
    if behaviour == 0 || image::base() == 0 {
        return false;
    }
    let token = behaviour + BEHAVIOUR_TOKEN;
    let Some(queues) = image::read_pointer(token) else {
        return false;
    };
    let queue = queues + UI_QUEUE;
    let Some(key) = image::read_u32(queue + QUEUE_KEY) else {
        return false;
    };
    // SAFETY: the key is the app's own `pthread_key_t`.
    let own = unsafe { libc::pthread_getspecific(key.into()) } as usize;
    let target = if own == 0 { queue } else { own };
    let Some(entry) = image::read_pointer(target)
        .and_then(|vtable| image::read_pointer(vtable + PUSH_SLOT))
        .filter(|entry| *entry != 0)
    else {
        return false;
    };
    let mut item = Job {
        invoke,
        manage: image::base() + TRIVIAL_MANAGER,
        data: [token, job as usize],
    };
    // SAFETY: as in `release`; raised before the job can run.
    let count = unsafe { AtomicI32::from_ptr((token + TOKEN_COUNT) as *mut i32) };
    count.fetch_add(1, Ordering::AcqRel);
    #[cfg(target_arch = "aarch64")]
    {
        let push: unsafe extern "C" fn(usize, *mut Job, usize) -> bool =
            // SAFETY: slot 2 of the queue's vtable is its push, as the stock
            // SHIFT + pad handler calls it.
            unsafe { core::mem::transmute::<usize, _>(entry) };
        // SAFETY: the queue copies the job through the app's own manager
        // before this returns; the token is the behaviour's, as in the stock
        // call.
        let accepted = unsafe { push(target, &raw mut item, token & !3) };
        if !accepted {
            release(token);
        }
        accepted
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        let _ = (entry, &mut item);
        release(token);
        false
    }
}

#[cfg(test)]
mod tests {
    use super::{TRIVIAL_MANAGER, post};

    fn nothing() {}

    #[test]
    fn nothing_is_posted_before_the_app_builds_its_behaviour() {
        assert!(!post(nothing));
        assert_eq!(TRIVIAL_MANAGER, 0x14a_a3f0);
    }
}
