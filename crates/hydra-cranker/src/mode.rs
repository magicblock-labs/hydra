//! Runtime selection of the target Hydra program.
//!
//! The cranker is a single binary that can drive either the base-layer program
//! or the ephemeral-rollup program; the choice is made once at startup from the
//! `--ephemeral` flag (not a compile-time feature). The base and ephemeral
//! cranks differ in their program ID, their `Close` account layout, and their
//! funding model (ephemeral cranks hold zero lamports), so the hot paths consult
//! this module rather than threading a flag through every call.

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use solana_pubkey::Pubkey;

use hydra_api::{consts, instruction as ix};

const MIN_SLOT_SAMPLE: Duration = Duration::from_millis(1);
const MIN_SLOT_SAMPLES: u8 = 3;

static SLOT_TIMING: OnceLock<Mutex<SlotTiming>> = OnceLock::new();
static EPHEMERAL: OnceLock<bool> = OnceLock::new();

/// Record the selected mode. Call once, before any watcher or the trigger loop
/// starts. Later calls are ignored.
pub fn init(ephemeral: bool) {
    let _ = EPHEMERAL.set(ephemeral);
}

/// Whether the cranker targets the ephemeral-rollup program.
pub fn is_ephemeral() -> bool {
    EPHEMERAL.get().copied().unwrap_or(false)
}

/// The program ID the cranker watches and submits to.
pub fn program_id() -> Pubkey {
    if is_ephemeral() {
        ix::EPHEMERAL_PROGRAM_ID
    } else {
        ix::BASE_PROGRAM_ID
    }
}

#[derive(Default)]
struct SlotTiming {
    last_boundary: Option<(u64, Instant)>,
    slot_duration: Option<Duration>,
    samples: u8,
}

impl SlotTiming {
    fn observe(&mut self, slot: u64, now: Instant) {
        if let Some((previous_slot, previous_at)) = self.last_boundary {
            if slot > previous_slot {
                let slot_delta = (slot - previous_slot).min(u32::MAX as u64) as u32;
                let sample = now.duration_since(previous_at) / slot_delta;
                if sample >= MIN_SLOT_SAMPLE {
                    self.slot_duration = Some(match self.slot_duration {
                        Some(duration) => (duration * 3 + sample) / 4,
                        None => sample,
                    });
                    self.samples = self.samples.saturating_add(1);
                } else {
                    self.slot_duration = None;
                    self.samples = 0;
                }
                self.last_boundary = Some((slot, now));
            }
        } else {
            self.last_boundary = Some((slot, now));
        }
    }

    fn ready(&self) -> bool {
        self.samples >= MIN_SLOT_SAMPLES
    }

    fn duration(&self) -> Duration {
        if self.ready() {
            self.slot_duration.unwrap_or(nominal_slot_duration())
        } else {
            nominal_slot_duration()
        }
    }
}

fn slot_timing() -> &'static Mutex<SlotTiming> {
    SLOT_TIMING.get_or_init(|| Mutex::new(SlotTiming::default()))
}

/// Record a slot observation so wall-clock slot duration can be measured.
pub fn observe_slot(slot: u64, at: Instant) {
    slot_timing()
        .lock()
        .expect("slot timing poisoned")
        .observe(slot, at);
}

/// Whether enough slot-boundary samples have been collected to trust the
/// measured duration over the mode's nominal fallback.
pub fn slot_duration_ready() -> bool {
    slot_timing().lock().expect("slot timing poisoned").ready()
}

/// Measured milliseconds-per-slot, or the nominal fallback if the stream
/// has not produced enough samples yet.
pub fn slot_duration() -> Duration {
    slot_timing()
        .lock()
        .expect("slot timing poisoned")
        .duration()
}

fn nominal_slot_duration() -> Duration {
    if is_ephemeral() {
        Duration::from_millis(consts::ephemeral::SLOT_FREQUENCY_MS)
    } else {
        Duration::from_millis(consts::base::SLOT_FREQUENCY_MS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn three_400ms_advances_measure_400ms() {
        let mut t = SlotTiming::default();
        let t0 = Instant::now();
        t.observe(10, t0);
        assert!(!t.ready());
        t.observe(11, t0 + Duration::from_millis(400));
        t.observe(12, t0 + Duration::from_millis(800));
        t.observe(13, t0 + Duration::from_millis(1200));
        assert!(t.ready());
        assert_eq!(t.duration(), Duration::from_millis(400));
    }

    #[test]
    fn skipped_slots_are_divided_into_the_sample() {
        let mut t = SlotTiming::default();
        let t0 = Instant::now();
        t.observe(10, t0);
        t.observe(12, t0 + Duration::from_millis(100));
        t.observe(14, t0 + Duration::from_millis(200));
        t.observe(16, t0 + Duration::from_millis(300));
        assert!(t.ready());
        assert_eq!(t.duration(), Duration::from_millis(50));
    }

    #[test]
    fn sub_millisecond_samples_are_rejected() {
        let mut t = SlotTiming::default();
        let t0 = Instant::now();
        t.observe(10, t0);
        t.observe(11, t0);
        assert!(!t.ready());
        assert_eq!(t.slot_duration, None);
        assert_eq!(t.samples, 0);
    }
}
