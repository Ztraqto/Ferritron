use ferritron::{InstanceHandle, MidiKind};
use ferritron::prelude::*;

/// Every block: a controller message goes out, and the plugin moves its own
/// `level` parameter (the host records that as automation). Every 16th block it
/// also notifies the main thread, which writes the block count into the
/// instance's store (work the audio thread must not do: it locks and allocates).
#[derive(Default)]
struct MidiOut {
    blocks: u32,
}

#[ferritron::plugin]
impl Process for MidiOut {
    fn tail(&self) -> Tail {
        Tail::Infinite
    }

    fn init(&mut self, instance: &InstanceHandle) {
        instance.on_notify(|code, handle| handle.store().set("blocks", code.to_string()));
    }

    fn process(&mut self, ctx: &mut Context) {
        self.blocks += 1;
        if self.blocks % 16 == 0 {
            ctx.notify(self.blocks as u64);
        }
        ctx.midi_out(0, 0, MidiKind::Cc { number: 74, value: (self.blocks % 128) as u8 });
        ctx.set_param(&ctx.params().level, (self.blocks % 100) as f64 / 100.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferritron::testing::Harness;

    #[global_allocator]
    static ALLOC: ferritron::testing::AllocGuard = ferritron::testing::AllocGuard;

    #[test]
    fn each_block_sends_a_controller_and_a_parameter_move() {
        let mut h = Harness::<MidiOut>::new(48_000.0, 64);
        let mut buf = [0.0f32; 64];
        assert!(h.process(&mut [&mut buf]));
        assert_eq!(h.midi_out().len(), 1);
        assert_eq!(h.param_edits().len(), 1);
        assert!(h.process(&mut [&mut buf]));
        assert!((h.param("level") - 0.02).abs() < 1e-9);
    }

    #[test]
    fn notify_reaches_the_main_thread_without_allocating() {
        let mut h = Harness::<MidiOut>::new(48_000.0, 64);
        let mut buf = [0.0f32; 64];
        for _ in 0..15 {
            h.process(&mut [&mut buf]);
        }
        // The 16th block notifies; it must not allocate on the audio thread.
        h.process(&mut [&mut buf]);
        h.assert_no_alloc();
        // The shim's poll on the main thread runs the handler; the bit never reaches the shim.
        assert_eq!(h.take_main_requests() & ferritron::abi::REQUEST_NOTIFY, 0);
        assert_eq!(h.instance().store().get_str("blocks").as_deref(), Some("16"));
    }

    #[test]
    fn notify_without_a_polling_shim_uses_the_fallback_thread() {
        let mut h = Harness::<MidiOut>::new(48_000.0, 64);
        let mut buf = [0.0f32; 64];
        for _ in 0..32 {
            h.process(&mut [&mut buf]);
        }
        // Nobody polls: Ferritron's own thread delivers after the grace period. The fallback
        // thread runs every 20 ms, so it may deliver the block-16 notify before block 32 is
        // handled: wait for the final value, up to 3 s, not for the first one.
        let start = std::time::Instant::now();
        while h.instance().store().get_str("blocks").as_deref() != Some("32") && start.elapsed() < std::time::Duration::from_secs(3) {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(h.instance().store().get_str("blocks").as_deref(), Some("32"));
    }
}
