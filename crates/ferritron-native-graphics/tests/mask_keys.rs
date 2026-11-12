//! N2: the key of a path mask does not depend on the whole part of its position, nor on the
//! size of the target. The same shape drawn at whole-pixel offsets shares one mask, and a
//! resized target reuses it.
use fng::prepare_only::Prepare;
use fng::*;

fn disc() -> Path {
    let mut p = Path::new();
    p.circle(Point::new(10.25, 10.25), 7.3);
    p
}

#[test]
fn one_mask_serves_every_whole_pixel_position() {
    let images = ImageStore::new();
    let opts = RenderOptions::new((800, 400));
    let mut prep = Prepare::new();
    let mut scene = Scene::new();
    let shape = disc();
    for i in 0..100 {
        let x = (i % 10) as f32 * 37.0;
        let y = (i / 10) as f32 * 31.0;
        scene.push_transform(Affine::translate(x, y));
        scene.fill_path(&shape, FillRule::NonZero, Color::WHITE);
        scene.pop_transform();
    }
    let st = prep.plan(&scene, &images, &opts);
    assert_eq!(st.instances, 100, "every copy is drawn: {st:?}");
    assert_eq!(st.rasterized, 1, "one mask for 100 positions: {st:?}");
    assert_eq!(st.atlas_masks, 1);
}

#[test]
fn a_quarter_pixel_offset_is_a_different_mask() {
    let images = ImageStore::new();
    let opts = RenderOptions::new((400, 400));
    let mut prep = Prepare::new();
    let mut scene = Scene::new();
    let shape = disc();
    for x in [0.0, 0.25, 0.5, 0.75] {
        scene.push_transform(Affine::translate(x, 0.0));
        scene.fill_path(&shape, FillRule::NonZero, Color::WHITE);
        scene.pop_transform();
    }
    let st = prep.plan(&scene, &images, &opts);
    assert_eq!(st.rasterized, 4, "each quarter pixel has its own mask: {st:?}");
}

#[test]
fn a_resized_target_reuses_the_mask() {
    let images = ImageStore::new();
    let mut prep = Prepare::new();
    let mut scene = Scene::new();
    scene.push_transform(Affine::translate(120.0, 40.0));
    scene.fill_path(&disc(), FillRule::NonZero, Color::WHITE);
    scene.pop_transform();
    let first = prep.plan(&scene, &images, &RenderOptions::new((800, 400)));
    assert_eq!(first.rasterized, 1);
    let second = prep.plan(&scene, &images, &RenderOptions::new((1024, 768)));
    assert_eq!(second.rasterized, 0, "the mask is not drawn again: {second:?}");
    assert_eq!(second.instances, 1);
}

#[test]
fn a_shape_wider_than_the_atlas_is_cut_to_the_target() {
    let images = ImageStore::new();
    let opts = RenderOptions::new((300, 200));
    let mut prep = Prepare::new();
    let mut scene = Scene::new();
    let mut p = Path::new();
    p.rect(Rect::new(-5000.0, 10.0, 9000.0, 50.0));
    scene.fill_path(&p, FillRule::NonZero, Color::WHITE);
    let st = prep.plan(&scene, &images, &opts);
    assert_eq!(st.instances, 1, "{st:?}");
    assert_eq!(st.dropped, 0, "{st:?}");
}
