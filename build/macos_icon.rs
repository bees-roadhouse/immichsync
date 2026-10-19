// Icon art shared by build.rs and tests/macos_icon.rs (issue #119).
//
// Pure pixel work on `image::RgbaImage`: no I/O except `write_iconset`.
// build.rs pulls this file in with `#[path]`, and so does the test, so the
// assertions run against exactly the art that ships.
//
// Two macOS outputs live here:
//   * the app icon: the shutter on Apple's macOS icon grid (a 1024 canvas,
//     an 824 continuous-curvature tile, a soft drop shadow in the margin,
//     a white-to-very-light gradient), downscaled to every iconset size;
//   * the menu-bar icon: a black-plus-alpha template silhouette that macOS
//     tints for light and dark menu bars.
//
// The brand logo itself is never redrawn: only framed (app icon) or reduced
// to its alpha (template).

use image::imageops::FilterType;
use image::{ImageBuffer, Rgba, RgbaImage};

/// Apple's macOS app icon grid: 1024 canvas, 824 tile, 100 px margin.
pub const APP_ICON_CANVAS: u32 = 1024;
pub const APP_ICON_TILE: u32 = 824;
/// Superellipse exponent. n = 5 is the usual stand-in for Apple's
/// continuous-curvature corner (a plain circular arc is n = 2 in the corner).
pub const SQUIRCLE_N: f32 = 5.0;
/// The logo crop (a circle around the furthest blade tip) as a fraction of
/// the tile width. The crop circle is wider than the logo's bounding box,
/// so 0.80 puts the visible logo at ~72% of the tile.
pub const LOGO_FRACTION: f32 = 0.80;
/// Drop shadow: offset down, blur half-width, peak opacity.
const SHADOW_OFFSET_Y: f32 = 12.0;
const SHADOW_SOFTNESS: f32 = 18.0;
const SHADOW_OPACITY: f32 = 0.30;
/// Tile gradient, top to bottom.
const TILE_TOP: [f32; 3] = [255.0, 255.0, 255.0];
const TILE_BOTTOM: [f32; 3] = [234.0, 236.0, 240.0];

/// Menu-bar template canvas: 18 pt at @2x. tray-icon scales the NSImage to
/// 18 pt high, so 36 px is pixel-exact on Retina.
pub const MENUBAR_SIZE: u32 = 36;
/// The silhouette fills this much of the canvas, leaving 2 px either side.
const MENUBAR_CONTENT: u32 = 32;
/// Offline template: blade opacity, and the slash's half-width and gap.
const OFFLINE_BODY_OPACITY: f32 = 0.4;
const SLASH_HALF: f32 = 1.75;
const SLASH_GAP: f32 = 1.5;

/// Convert near-white background pixels to transparent.
///
/// The source is a logo on a near-white opaque background (R,G,B ~= 240..255,
/// A = 255). Pixels within `STRICT` of white become fully transparent; pixels
/// between `STRICT` and `SOFT` of white get a linear alpha falloff for soft
/// edges.
pub fn strip_white_background(src: &RgbaImage) -> RgbaImage {
    const STRICT: i32 = 235;
    const SOFT: i32 = 250;

    let mut out = src.clone();
    for px in out.pixels_mut() {
        let r = px.0[0] as i32;
        let g = px.0[1] as i32;
        let b = px.0[2] as i32;
        let min = r.min(g).min(b);
        if min >= SOFT {
            px.0[3] = 0;
        } else if min >= STRICT {
            // Linear falloff: SOFT -> 0 alpha, STRICT -> 255 alpha.
            let t = (SOFT - min) as f32 / (SOFT - STRICT) as f32;
            px.0[3] = (t * 255.0).round().clamp(0.0, 255.0) as u8;
        }
    }
    out
}

/// Key out the source's flat background for the macOS art.
///
/// `strip_white_background` (the Windows pipeline) only clears pixels at 250+
/// and the source background is RGB 246, so it leaves a 27%-alpha grey
/// square behind the logo. Here the background colour is read from the
/// top-left corner instead, and alpha ramps from 0 at `NEAR` to 255 at `FAR`
/// (largest per-channel difference from that colour), which keeps the
/// blades' soft light outlines.
pub fn key_out_background(src: &RgbaImage) -> RgbaImage {
    const NEAR: i32 = 4;
    const FAR: i32 = 20;
    let bg = src.get_pixel(0, 0).0;
    let mut out = src.clone();
    for px in out.pixels_mut() {
        let d = (0..3)
            .map(|c| (px.0[c] as i32 - bg[c] as i32).abs())
            .max()
            .unwrap_or(0);
        let t = ((d - NEAR) as f32 / (FAR - NEAR) as f32).clamp(0.0, 1.0);
        px.0[3] = (t * px.0[3] as f32).round() as u8;
    }
    out
}

/// Resize to `size`x`size` using Lanczos3 (high-quality downscale).
pub fn resize_high_quality(src: &RgbaImage, size: u32) -> RgbaImage {
    image::imageops::resize(src, size, size, FilterType::Lanczos3)
}

/// Crop the stripped logo to a square that just holds it, centred on the
/// logo's bounding box, with a side of twice the furthest opaque pixel's
/// distance from that centre. Rotating the crop therefore never clips a
/// blade tip, and scaling it sets the logo's diameter directly.
pub fn crop_logo(stripped: &RgbaImage) -> RgbaImage {
    const SOLID: u8 = 16;
    let (w, h) = stripped.dimensions();
    let (mut x0, mut y0, mut x1, mut y1) = (w, h, 0u32, 0u32);
    for (x, y, p) in stripped.enumerate_pixels() {
        if p.0[3] > SOLID {
            x0 = x0.min(x);
            y0 = y0.min(y);
            x1 = x1.max(x);
            y1 = y1.max(y);
        }
    }
    assert!(x1 >= x0 && y1 >= y0, "icon source has no opaque pixels");
    let cx = (x0 + x1) as f32 / 2.0 + 0.5;
    let cy = (y0 + y1) as f32 / 2.0 + 0.5;
    let mut r2 = 0f32;
    for (x, y, p) in stripped.enumerate_pixels() {
        if p.0[3] > SOLID {
            let dx = x as f32 + 0.5 - cx;
            let dy = y as f32 + 0.5 - cy;
            r2 = r2.max(dx * dx + dy * dy);
        }
    }
    let side = (2.0 * r2.sqrt()).ceil() as u32 + 2;
    let ox = cx - side as f32 / 2.0;
    let oy = cy - side as f32 / 2.0;
    let mut out: RgbaImage = ImageBuffer::from_pixel(side, side, Rgba([255, 255, 255, 0]));
    for y in 0..side {
        for x in 0..side {
            let sx = (ox + x as f32).round() as i64;
            let sy = (oy + y as f32).round() as i64;
            if sx >= 0 && sy >= 0 && (sx as u32) < w && (sy as u32) < h {
                out.put_pixel(x, y, *stripped.get_pixel(sx as u32, sy as u32));
            }
        }
    }
    out
}

/// Approximate signed distance in pixels from (x, y), relative to the shape's
/// centre, to a superellipse of half-width `a` and exponent `n`. Negative
/// inside. Exact on the axes, close enough near the edge for anti-aliasing.
pub fn squircle_distance(x: f32, y: f32, a: f32, n: f32) -> f32 {
    let f = (x.abs() / a).powf(n) + (y.abs() / a).powf(n);
    a * (f.powf(1.0 / n) - 1.0)
}

fn smoothstep(e0: f32, e1: f32, x: f32) -> f32 {
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Straight-alpha "source over destination", channels in 0..=255 / 0..=1.
fn over(dst: [f32; 4], src: [f32; 4]) -> [f32; 4] {
    let a = src[3] + dst[3] * (1.0 - src[3]);
    if a <= 0.0 {
        return [0.0; 4];
    }
    let mut out = [0f32; 4];
    for c in 0..3 {
        out[c] = (src[c] * src[3] + dst[c] * dst[3] * (1.0 - src[3])) / a;
    }
    out[3] = a;
    out
}

/// The 1024x1024 master app icon: drop shadow, squircle tile with a faint
/// gradient, and the cropped logo centred at `LOGO_FRACTION` of the tile.
pub fn app_icon_master(logo: &RgbaImage) -> RgbaImage {
    let canvas = APP_ICON_CANVAS;
    let centre = canvas as f32 / 2.0;
    let half = APP_ICON_TILE as f32 / 2.0;
    let tile_top = centre - half;

    let logo_px = (APP_ICON_TILE as f32 * LOGO_FRACTION).round() as u32;
    let logo_small = resize_high_quality(logo, logo_px);
    let logo_origin = (canvas - logo_px) / 2;

    let mut out: RgbaImage = ImageBuffer::new(canvas, canvas);
    for (x, y, px) in out.enumerate_pixels_mut() {
        let rx = x as f32 + 0.5 - centre;
        let ry = y as f32 + 0.5 - centre;

        let shadow_d = squircle_distance(rx, ry - SHADOW_OFFSET_Y, half, SQUIRCLE_N);
        let shadow_a =
            SHADOW_OPACITY * (1.0 - smoothstep(-SHADOW_SOFTNESS, SHADOW_SOFTNESS, shadow_d));
        let mut acc = [0.0, 0.0, 0.0, shadow_a];

        let tile_d = squircle_distance(rx, ry, half, SQUIRCLE_N);
        let tile_a = (0.5 - tile_d).clamp(0.0, 1.0);
        if tile_a > 0.0 {
            let t = ((y as f32 + 0.5 - tile_top) / APP_ICON_TILE as f32).clamp(0.0, 1.0);
            let mut tile = [0f32; 4];
            for c in 0..3 {
                tile[c] = TILE_TOP[c] + (TILE_BOTTOM[c] - TILE_TOP[c]) * t;
            }
            tile[3] = tile_a;
            acc = over(acc, tile);

            let (lx, ly) = (x.wrapping_sub(logo_origin), y.wrapping_sub(logo_origin));
            if lx < logo_px && ly < logo_px {
                let p = logo_small.get_pixel(lx, ly).0;
                let src = [
                    p[0] as f32,
                    p[1] as f32,
                    p[2] as f32,
                    p[3] as f32 / 255.0 * tile_a,
                ];
                acc = over(acc, src);
            }
        }

        *px = Rgba([
            acc[0].round().clamp(0.0, 255.0) as u8,
            acc[1].round().clamp(0.0, 255.0) as u8,
            acc[2].round().clamp(0.0, 255.0) as u8,
            (acc[3] * 255.0).round().clamp(0.0, 255.0) as u8,
        ]);
    }
    out
}

/// Write the PNG set `iconutil` expects: 16..512 pt at 1x and 2x, each one
/// downscaled straight from the 1024 master (never from a smaller size).
pub fn write_iconset(master: &RgbaImage, dir: &std::path::Path) {
    std::fs::create_dir_all(dir).expect("create AppIcon.iconset");
    for pt in [16u32, 32, 128, 256, 512] {
        for scale in [1u32, 2] {
            let px = pt * scale;
            let name = if scale == 1 {
                format!("icon_{pt}x{pt}.png")
            } else {
                format!("icon_{pt}x{pt}@2x.png")
            };
            let img = if px == master.width() {
                master.clone()
            } else {
                resize_high_quality(master, px)
            };
            img.save(dir.join(name)).expect("write iconset png");
        }
    }
}

/// Rotate an RGBA image clockwise by `degrees` around the center, keeping
/// the output dimensions the same as the input. Pixels outside the original
/// canvas after rotation become transparent.
pub fn rotate_rgba(src: &RgbaImage, degrees: f32) -> RgbaImage {
    let (w, h) = src.dimensions();
    let cx = (w as f32 - 1.0) / 2.0;
    let cy = (h as f32 - 1.0) / 2.0;
    // Positive degrees here means clockwise in screen coordinates (y down).
    let rad = degrees.to_radians();
    let cos = rad.cos();
    let sin = rad.sin();

    let mut out: RgbaImage = ImageBuffer::from_pixel(w, h, Rgba([0, 0, 0, 0]));
    for y in 0..h {
        for x in 0..w {
            // Inverse map: for each output pixel, find the source pixel.
            // dst = R(theta) * src  (clockwise, y-down)
            //   dx = cos*sx + sin*sy
            //   dy = -sin*sx + cos*sy
            // Invert: src = R(-theta) * dst
            //   sx =  cos*dx - sin*dy
            //   sy =  sin*dx + cos*dy
            let dx = x as f32 - cx;
            let dy = y as f32 - cy;
            let sx = cos * dx - sin * dy + cx;
            let sy = sin * dx + cos * dy + cy;
            if sx < 0.0 || sy < 0.0 || sx > (w as f32 - 1.0) || sy > (h as f32 - 1.0) {
                continue;
            }
            // Bilinear sample.
            let x0 = sx.floor() as u32;
            let y0 = sy.floor() as u32;
            let x1 = (x0 + 1).min(w - 1);
            let y1 = (y0 + 1).min(h - 1);
            let fx = sx - x0 as f32;
            let fy = sy - y0 as f32;
            let p00 = src.get_pixel(x0, y0).0;
            let p10 = src.get_pixel(x1, y0).0;
            let p01 = src.get_pixel(x0, y1).0;
            let p11 = src.get_pixel(x1, y1).0;
            let mut chan = [0f32; 4];
            for c in 0..4 {
                let top = p00[c] as f32 * (1.0 - fx) + p10[c] as f32 * fx;
                let bot = p01[c] as f32 * (1.0 - fx) + p11[c] as f32 * fx;
                chan[c] = top * (1.0 - fy) + bot * fy;
            }
            out.put_pixel(
                x,
                y,
                Rgba([
                    chan[0].round() as u8,
                    chan[1].round() as u8,
                    chan[2].round() as u8,
                    chan[3].round() as u8,
                ]),
            );
        }
    }
    out
}

/// A menu-bar template frame: the logo's alpha as a black silhouette,
/// rotated clockwise by `degrees`, on a `MENUBAR_SIZE` canvas.
///
/// Rotation happens at 4x the content size (cheap, and clean once
/// downscaled), not on the full-resolution crop.
pub fn menubar_template(logo: &RgbaImage, degrees: f32) -> RgbaImage {
    let mut work = resize_high_quality(logo, MENUBAR_CONTENT * 4);
    for p in work.pixels_mut() {
        p.0[0] = 0;
        p.0[1] = 0;
        p.0[2] = 0;
    }
    if degrees != 0.0 {
        work = rotate_rgba(&work, degrees);
    }
    let small = resize_high_quality(&work, MENUBAR_CONTENT);
    let pad = (MENUBAR_SIZE - MENUBAR_CONTENT) / 2;
    let mut out: RgbaImage = ImageBuffer::new(MENUBAR_SIZE, MENUBAR_SIZE);
    for (x, y, p) in small.enumerate_pixels() {
        // Resampling can leave stray non-zero RGB on near-transparent
        // pixels; a template is black plus alpha, so force it.
        out.put_pixel(x + pad, y + pad, Rgba([0, 0, 0, p.0[3]]));
    }
    out
}

/// The offline/paused/error template: blades dimmed to
/// `OFFLINE_BODY_OPACITY`, with a full-strength slash from top-left to
/// bottom-right and a clear gap either side of it so it reads at 18 pt.
pub fn menubar_offline(template: &RgbaImage) -> RgbaImage {
    let (w, h) = template.dimensions();
    let s = w as f32;
    let (x0, y0) = (s * 0.12, s * 0.12);
    let (x1, y1) = (s * 0.88, s * 0.88);
    let (dx, dy) = (x1 - x0, y1 - y0);
    let len2 = dx * dx + dy * dy;

    let mut out: RgbaImage = ImageBuffer::new(w, h);
    for y in 0..h {
        for x in 0..w {
            let px = x as f32 + 0.5;
            let py = y as f32 + 0.5;
            let t = (((px - x0) * dx + (py - y0) * dy) / len2).clamp(0.0, 1.0);
            let (qx, qy) = (x0 + t * dx, y0 + t * dy);
            let d = ((px - qx).powi(2) + (py - qy).powi(2)).sqrt();

            let slash = (SLASH_HALF + 0.5 - d).clamp(0.0, 1.0);
            let keep = (d - SLASH_HALF - SLASH_GAP + 0.5).clamp(0.0, 1.0);
            let body = template.get_pixel(x, y).0[3] as f32 / 255.0 * OFFLINE_BODY_OPACITY * keep;
            let a = slash.max(body);
            out.put_pixel(x, y, Rgba([0, 0, 0, (a * 255.0).round() as u8]));
        }
    }
    out
}
