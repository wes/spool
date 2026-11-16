//! An extension that does not stop.
//!
//! The host promises that a call runs under a deadline and that an extension
//! which loops forever is interrupted rather than waited for. This is what that
//! promise is tested against.

use spool_extension::{CommandContext, Effect, Extension, export_extension};

struct Runaway;

impl Extension for Runaway {
    fn run_command(_id: String, _context: CommandContext) -> Vec<Effect> {
        // Deliberately unreachable-from. `spin_loop` keeps the loop from being
        // optimized into something without a backedge, which is where the
        // engine checks the epoch — an empty loop that compiled away would
        // make this test pass for the wrong reason.
        let mut spins: u64 = 0;
        loop {
            spins = spins.wrapping_add(1);
            std::hint::spin_loop();
            std::hint::black_box(spins);
        }
    }
}

export_extension!(Runaway);
