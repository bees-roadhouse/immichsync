// Assertions on the macOS icon art that build.rs generates (issue #119).
//
// build.rs and this test share one module, build/macos_icon.rs, so what is
// tested here is exactly what lands in AppIcon.iconset and the menu-bar
// template frames. Pure pixel work: runs on every OS in CI.

#[allow(dead_code)]
#[path = "../build/macos_icon.rs"]
mod macos_icon;

use image::RgbaImage;
use macos_icon::*;

fn logo() -> RgbaImage {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/icon-source.webp");
    let src = image::open(path).expect("load icon source").to_rgba8();
    crop_logo(&key_out_background(&src))
}

fn alpha(img: &RgbaImage, x: u32, y: u32) -> u8 {
    img.get_pixel(x, y).0[3]
}

fn luma(img: &RgbaImage, x: u32, y: u32) -> f32 {
    let p = img.get_pixel(x, y).0;
    0.299 * p[0] as f32 + 0.587 * p[1] as f32 + 0.114 * p[2] as f32
}

/// A logo pixel, as opposed to the white-ish tile behind it: clearly coloured
/// or clearly dark.
fn is_logo_pixel(img: &RgbaImage, x: u32, y: u32) -> bool {
    let p = img.get_pixel(x, y).0;
    let max = p[0].max(p[1]).max(p[2]) as i32;
    let min = p[0].min(p[1]).min(p[2]) as i32;
    p[3] > 200 && (max - min > 60 || max < 120)
}

#[test]
fn master_is_1024_with_transparent_canvas_corners() {
    let m = app_icon_master(&logo());
    assert_eq!(m.dimensions(), (1024, 1024));
    for (x, y) in [
        (0, 0),
        (1023, 0),
        (0, 1023),
        (1023, 1023),
        (512, 20),
        (20, 512),
    ] {
        assert_eq!(
            alpha(&m, x, y),
            0,
            "canvas at ({x},{y}) must be transparent"
        );
    }
}

#[test]
fn tile_is_824_centred_with_a_100px_margin() {
    let m = app_icon_master(&logo());
    // Edges of the tile along the centre lines: opaque just inside 100..924,
    // not tile-opaque just outside (only shadow can be there).
    for (x, y) in [(512, 103), (103, 512), (920, 512), (512, 920)] {
        assert_eq!(alpha(&m, x, y), 255, "inside the tile at ({x},{y})");
    }
    for (x, y) in [(512, 96), (96, 512), (927, 512)] {
        assert!(alpha(&m, x, y) < 128, "outside the tile at ({x},{y})");
    }
}

#[test]
fn corners_are_a_superellipse_not_a_circular_arc() {
    let m = app_icon_master(&logo());
    // On the diagonal a superellipse with n=5 and half-width 412 reaches
    // 412 * 2^(-1/5) = 358.7 px from the centre. A circular-arc rounded rect
    // with Apple's ~22% corner radius reaches only ~346; a squircle's corner
    // sits further out. Top-right corner, away from the downward shadow.
    assert!(
        alpha(&m, 512 + 354, 512 - 354) > 200,
        "inside the squircle corner"
    );
    assert!(
        alpha(&m, 512 + 366, 512 - 366) < 60,
        "outside the squircle corner"
    );
}

#[test]
fn drop_shadow_sits_below_the_tile() {
    let m = app_icon_master(&logo());
    let below = m.get_pixel(512, 935).0;
    let above = m.get_pixel(512, 89).0;
    assert!(
        below[3] > 10 && below[3] < 160,
        "soft shadow below, got {}",
        below[3]
    );
    assert!(
        below[0] < 60 && below[1] < 60 && below[2] < 60,
        "shadow is dark"
    );
    assert!(above[3] < below[3], "shadow is offset downward");
}

#[test]
fn tile_background_is_light_with_a_top_to_bottom_gradient() {
    let m = app_icon_master(&logo());
    let top = luma(&m, 512, 120);
    let bottom = luma(&m, 512, 905);
    assert!(top > 245.0, "top of tile is near white, got {top}");
    assert!(
        bottom > 225.0,
        "bottom of tile is still very light, got {bottom}"
    );
    assert!(
        top - bottom > 4.0,
        "a faint gradient, got {top} vs {bottom}"
    );
}

#[test]
fn no_background_square_behind_the_logo() {
    // The source background is RGB 246, not white. Left behind, it shows as
    // a faint square on the tile. Just inside the logo crop's corner (logo
    // spans 215..809) must match the tile beside it at the same height.
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/icon-source.webp");
    let src = image::open(path).unwrap().to_rgba8();
    assert_eq!(
        key_out_background(&src).get_pixel(5, 5).0[3],
        0,
        "background keyed out"
    );
    let m = app_icon_master(&logo());
    for (inside, outside) in [((222, 222), (150, 222)), ((802, 802), (874, 802))] {
        let a = m.get_pixel(inside.0, inside.1).0;
        let b = m.get_pixel(outside.0, outside.1).0;
        for c in 0..4 {
            assert!(
                (a[c] as i32 - b[c] as i32).abs() <= 1,
                "crop corner {inside:?} {a:?} vs plain tile {outside:?} {b:?}"
            );
        }
    }
}

#[test]
fn logo_is_centred_at_about_72_percent_of_the_tile() {
    let m = app_icon_master(&logo());
    let (mut x0, mut y0, mut x1, mut y1) = (u32::MAX, u32::MAX, 0, 0);
    for y in 0..1024 {
        for x in 0..1024 {
            if is_logo_pixel(&m, x, y) {
                x0 = x0.min(x);
                y0 = y0.min(y);
                x1 = x1.max(x);
                y1 = y1.max(y);
            }
        }
    }
    let width = (x1 - x0) as f32 / 824.0;
    assert!(
        (0.66..=0.76).contains(&width),
        "logo width fraction {width}"
    );
    let cx = (x0 + x1) as f32 / 2.0;
    let cy = (y0 + y1) as f32 / 2.0;
    assert!(
        (cx - 512.0).abs() < 16.0 && (cy - 512.0).abs() < 16.0,
        "centre ({cx},{cy})"
    );
}

#[test]
fn iconset_has_every_size_from_the_master() {
    let m = app_icon_master(&logo());
    let dir = tempfile::tempdir().unwrap();
    write_iconset(&m, dir.path());
    let expected = [
        ("icon_16x16.png", 16),
        ("icon_16x16@2x.png", 32),
        ("icon_32x32.png", 32),
        ("icon_32x32@2x.png", 64),
        ("icon_128x128.png", 128),
        ("icon_128x128@2x.png", 256),
        ("icon_256x256.png", 256),
        ("icon_256x256@2x.png", 512),
        ("icon_512x512.png", 512),
        ("icon_512x512@2x.png", 1024),
    ];
    let count = std::fs::read_dir(dir.path()).unwrap().count();
    assert_eq!(count, expected.len());
    for (name, px) in expected {
        let img = image::open(dir.path().join(name)).unwrap().to_rgba8();
        assert_eq!(img.dimensions(), (px, px), "{name}");
        assert_eq!(alpha(&img, 0, 0), 0, "{name} keeps the transparent margin");
        assert_eq!(alpha(&img, px / 2, px / 2), 255, "{name} tile is opaque");
    }
}

fn assert_template(img: &RgbaImage) {
    assert_eq!(img.dimensions(), (MENUBAR_SIZE, MENUBAR_SIZE));
    for p in img.pixels() {
        if p.0[3] > 0 {
            assert_eq!(
                &p.0[..3],
                &[0, 0, 0],
                "template pixels are black plus alpha"
            );
        }
    }
}

fn coverage(img: &RgbaImage) -> f32 {
    img.pixels().map(|p| p.0[3] as f32 / 255.0).sum::<f32>() / (img.width() * img.height()) as f32
}

#[test]
fn menubar_idle_is_a_black_silhouette_with_an_aperture() {
    let t = menubar_template(&logo(), 0.0);
    assert_template(&t);
    let c = MENUBAR_SIZE / 2;
    assert_eq!(alpha(&t, c, c), 0, "the shutter's aperture stays open");
    assert_eq!(alpha(&t, 0, 0), 0);
    assert!(t.pixels().any(|p| p.0[3] == 255), "solid blades");
    let cov = coverage(&t);
    assert!((0.2..0.7).contains(&cov), "coverage {cov}");
}

#[test]
fn menubar_rotation_frames_differ_and_stay_templates() {
    let l = logo();
    let f0 = menubar_template(&l, 0.0);
    let f3 = menubar_template(&l, 90.0);
    assert_template(&f3);
    assert_ne!(f0.as_raw(), f3.as_raw());
    assert!(
        (coverage(&f0) - coverage(&f3)).abs() < 0.03,
        "rotation keeps the shape in frame"
    );
}

#[test]
fn menubar_offline_is_dimmed_with_a_full_strength_slash() {
    let idle = menubar_template(&logo(), 0.0);
    let off = menubar_offline(&idle);
    assert_template(&off);
    assert!(
        coverage(&off) < coverage(&idle) * 0.8,
        "offline reads dimmer"
    );
    // The slash runs top-left to bottom-right, at full strength.
    let s = MENUBAR_SIZE;
    assert_eq!(alpha(&off, s / 4, s / 4), 255);
    assert_eq!(alpha(&off, 3 * s / 4, 3 * s / 4), 255);
}
