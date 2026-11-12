//! A gain whose web editor shows what `window.ferritron` gives a page besides the parameters:
//! the system theme, settings kept across sessions, a context menu, file dialogs, and the
//! modifier keys, wheel deltas and pointer lock the browser provides. `ui/index.html` prints
//! each result on the page; `ferritron-testhost --gui` screenshots it.

use ferritron::prelude::*;

#[derive(Default)]
struct WebExtras;

#[ferritron::plugin]
impl Process for WebExtras {
    fn process(&mut self, ctx: &mut Context) {
        let gain = db_to_gain(ctx.params().gain.value());
        ctx.io().for_each_sample(|s| *s *= gain);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferritron::testing::Harness;

    #[test]
    fn it_is_a_gain() {
        let mut h = Harness::<WebExtras>::new(48_000.0, 256);
        h.set_param("gain", -6.0);
        let mut buf = [1.0f32; 256];
        for _ in 0..4 {
            buf.fill(1.0);
            assert!(h.process(&mut [&mut buf]));
        }
        assert!((buf[255] - 0.501).abs() < 1e-3);
    }

    #[test]
    fn the_page_uses_every_feature_it_shows() {
        let page = include_str!("../ui/index.html");
        for call in ["ferritron.theme", "ferritron.onThemeChange", "ferritron.settings.set", "ferritron.settings.onChange", "ferritron.contextMenu", "ferritron.openFile", "ferritron.saveFile", "requestPointerLock", "addEventListener(\"wheel\""] {
            assert!(page.contains(call), "the page should show {call}");
        }
    }
}
