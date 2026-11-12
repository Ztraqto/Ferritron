//! The mask atlas evicts its least recently used shelves when it is full (N1). A frame whose own
//! masks fit must not start over: before eviction, a full atlas restarted the whole frame.
use fng::prepare_only::Prepare;
use fng::*;

#[test]
fn a_full_atlas_evicts_old_shelves_instead_of_restarting() {
    let images = ImageStore::new();
    let opts = RenderOptions::new((1920, 1080));
    let mut prep = Prepare::new();
    let mut scene = Scene::new();
    let mut most_masks = 0;
    // 12 frames of 50 distinct 300 x 300 masks: 600 masks, about 54 million pixels, against an
    // atlas of at most 4096 x 4096 (about 186 such masks)
    for f in 0..12 {
        scene.clear();
        for i in 0..50 {
            let mut p = Path::new();
            let w = 300.0 + (f * 50 + i) as f32 * 0.01;
            p.rect(Rect::new(10.0, 10.0, w, 300.0));
            scene.fill_path(&p, FillRule::NonZero, Color::WHITE);
        }
        let st = prep.plan(&scene, &images, &opts);
        assert_eq!(st.restarts, 0, "frame {f} started over: {st:?}");
        assert_eq!(st.dropped, 0, "frame {f} dropped masks: {st:?}");
        assert_eq!(st.instances, 50, "frame {f} draws all its masks");
        most_masks = most_masks.max(st.atlas_masks);
    }
    assert!(most_masks < 300, "old masks were evicted (most kept at once: {most_masks})");
}
