// Build script: brand icon pipeline.
//
// Inputs:
//   assets/icon-source.webp   ... 1232x1232 six-blade shutter logo
//
// Outputs (all under OUT_DIR/icons/, picked up by include_bytes! in src/ui/tray.rs):
//   icon.ico                  ... multi-resolution (16, 24, 32, 48, 64, 128, 256). Embedded
//                                 as the Windows EXE resource via winresource so Explorer,
//                                 Apps & Features, and pinned-shortcut icons all render
//                                 the shutter logo.
//   tray_idle.rgba            ... raw 32x32 RGBA8 bitmap for the tray idle state.
//   tray_offline.rgba         ... raw 32x32 RGBA8 bitmap for the tray offline state
//                                 (desaturated blades + a corner slash badge, matching the
//                                 Slack/Teams disconnect convention).
//   tray_rot_<NN>.rgba        ... 12 raw 32x32 RGBA8 frames of the idle icon rotated
//                                 clockwise in 30-degree increments. NN = 00..11.
//                                 100ms-per-frame cycle gives one full revolution per 1.2s.
//
// macOS targets only (issue #119; the art is in build/macos_icon.rs, shared
// with tests/macos_icon.rs):
//   AppIcon.iconset/          ... the shutter on Apple's macOS icon grid (824 squircle
//                                 tile on a 1024 canvas, drop shadow, light gradient),
//                                 every size downscaled from the 1024 master.
//   menubar_idle.rgba,        ... raw 36x36 RGBA8 template images (black plus alpha) for
//   menubar_offline.rgba,         the menu bar, which macOS tints for light and dark
//   menubar_rot_<NN>.rgba         menu bars. Same 12-frame rotation as the tray.
//
// The .rgba files are header-less ... they're known to be 32x32x4 bytes (4096 bytes each),
// validated at runtime by a const assertion in tray.rs. This lets us ship pre-decoded
// pixels and skip pulling the `image` crate into the runtime binary.

use std::path::PathBuf;

use image::{GenericImageView, ImageBuffer, Rgba, RgbaImage};

#[path = "build/macos_icon.rs"]
mod macos_icon;

use macos_icon::{key_out_background, resize_high_quality, rotate_rgba};

const SOURCE_PATH: &str = "assets/icon-source.webp";

/// Sizes embedded in the .ico (each one a separate PNG-compressed entry).
const ICO_SIZES: &[u32] = &[16, 24, 32, 48, 64, 128, 256];

/// Tray icon size. Windows scales 16x16 too aggressively on high-DPI displays;
/// 32x32 looks crisp on 100% DPI and acceptable on 200% DPI without manual
/// scaling. tray-icon accepts arbitrary sizes via Icon::from_rgba.
const TRAY_SIZE: u32 = 32;

/// Number of rotation frames. 12 at 30 degrees per step renders smoothly
/// without being obviously stepped.
const ROT_FRAMES: u32 = 12;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=build/macos_icon.rs");
    println!("cargo:rerun-if-changed={}", SOURCE_PATH);

    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let icons_dir = out_dir.join("icons");
    std::fs::create_dir_all(&icons_dir).expect("create OUT_DIR/icons");

    // ---- Load the source webp ----
    let src = image::open(SOURCE_PATH)
        .unwrap_or_else(|e| panic!("failed to load {}: {}", SOURCE_PATH, e));
    let (sw, sh) = src.dimensions();
    if sw != sh {
        panic!(
            "icon source must be square, got {}x{} ({})",
            sw, sh, SOURCE_PATH
        );
    }

    // The source has a flat opaque background (RGB 246, not white). Key it
    // out by its measured colour so the icon composites on dark taskbars and
    // Start menus without a faint grey square (#121); the macOS art below
    // uses the same keying.
    let src = src.to_rgba8();
    let src_rgba = key_out_background(&src);

    // ---- Generate the .ico ----
    build_ico(&src_rgba, &icons_dir);

    // ---- Generate tray idle ----
    let tray_idle = resize_high_quality(&src_rgba, TRAY_SIZE);
    write_rgba(&icons_dir.join("tray_idle.rgba"), &tray_idle);

    // ---- Generate offline overlay variant ----
    // Decision: desaturate the blades AND stamp a small slash badge in the
    // bottom-right corner. The desaturation alone reads as "muted" but not
    // clearly "disconnected"; the slash badge is the load-bearing signal.
    // Matches the Slack/Teams disconnect convention noted in the issue.
    let tray_offline = make_offline(&tray_idle);
    write_rgba(&icons_dir.join("tray_offline.rgba"), &tray_offline);

    // ---- Generate rotation frames ----
    // Render each frame from the high-resolution source rotated, THEN resize
    // down. Rotating a 32x32 image and re-resizing introduces aliasing on
    // every frame; rotating the 1232x1232 source once per frame and then
    // resizing gives clean edges throughout the cycle.
    //
    // Direction: clockwise. The shutter blades curl with their tips trailing
    // a clockwise sweep ... rotating clockwise matches the visual flow.
    for i in 0..ROT_FRAMES {
        let degrees = (i as f32) * (360.0 / ROT_FRAMES as f32);
        let rotated = rotate_rgba(&src_rgba, degrees);
        let small = resize_high_quality(&rotated, TRAY_SIZE);
        let name = format!("tray_rot_{:02}.rgba", i);
        write_rgba(&icons_dir.join(name), &small);
    }

    // ---- macOS app icon and menu-bar templates ----
    // packaging/macos/make-app.sh runs `iconutil -c icns` on AppIcon.iconset
    // to produce Contents/Resources/AppIcon.icns. The menubar_*.rgba frames
    // are included by src/ui/tray.rs on macOS only.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        let logo = macos_icon::crop_logo(&src_rgba);
        let master = macos_icon::app_icon_master(&logo);
        macos_icon::write_iconset(&master, &icons_dir.join("AppIcon.iconset"));

        let idle = macos_icon::menubar_template(&logo, 0.0);
        write_rgba(&icons_dir.join("menubar_idle.rgba"), &idle);
        write_rgba(
            &icons_dir.join("menubar_offline.rgba"),
            &macos_icon::menubar_offline(&idle),
        );
        for i in 0..ROT_FRAMES {
            let degrees = (i as f32) * (360.0 / ROT_FRAMES as f32);
            let frame = macos_icon::menubar_template(&logo, degrees);
            write_rgba(
                &icons_dir.join(format!("menubar_rot_{:02}.rgba", i)),
                &frame,
            );
        }
    }

    // ---- Embed the .ico as the EXE resource icon ----
    // winresource only takes effect on Windows targets. On a Linux dev
    // sandbox doing `cargo check` it's a no-op, which is what we want.
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        let ico_path = icons_dir.join("icon.ico");
        let mut res = winresource::WindowsResource::new();
        res.set_icon(ico_path.to_str().expect("ico path utf8"));
        // Standard version block fields. The version string is supplied by
        // cargo via CARGO_PKG_VERSION.
        res.set("FileDescription", "ImmichSync");
        res.set("ProductName", "ImmichSync");
        res.set("OriginalFilename", "immichsync.exe");
        res.set("CompanyName", "Bee's Roadhouse");
        res.set(
            "LegalCopyright",
            "Copyright (c) Bee's Roadhouse. GPL-3.0-only.",
        );
        if let Err(e) = res.compile() {
            // Don't hard-fail on non-MSVC toolchains during dev; we still
            // want `cargo check` to succeed in environments without rc.exe.
            // CI runs on windows-latest where winresource finds rc.exe via
            // the MSVC build tools.
            println!("cargo:warning=winresource compile failed: {}", e);
        }
    }
}

/// Build the multi-resolution .ico file (icon.ico in OUT_DIR/icons/).
///
/// Each entry is PNG-compressed (ICO supports PNG payloads since Vista; this
/// keeps the embedded resource small while preserving alpha).
fn build_ico(src: &RgbaImage, out_dir: &std::path::Path) {
    let mut dir = ico::IconDir::new(ico::ResourceType::Icon);
    for &size in ICO_SIZES {
        let resized = resize_high_quality(src, size);
        let raw = resized.into_raw();
        let img = ico::IconImage::from_rgba_data(size, size, raw);
        let entry = ico::IconDirEntry::encode(&img).expect("encode ico entry");
        dir.add_entry(entry);
    }
    let ico_path = out_dir.join("icon.ico");
    let file = std::fs::File::create(&ico_path).expect("create icon.ico");
    dir.write(file).expect("write icon.ico");
}

/// Build the offline overlay variant.
///
/// Steps:
///   1. Desaturate the blades (read as muted/inactive).
///   2. Stamp a small red-ish slash badge in the bottom-right corner.
///
/// The slash is what carries the actual "offline" signal; desaturation alone
/// is too easy to miss against a busy taskbar.
fn make_offline(src: &RgbaImage) -> RgbaImage {
    let (w, h) = src.dimensions();
    let mut out: RgbaImage = ImageBuffer::new(w, h);

    // Pass 1: desaturate the source.
    for (x, y, px) in src.enumerate_pixels() {
        let r = px.0[0] as f32;
        let g = px.0[1] as f32;
        let b = px.0[2] as f32;
        let a = px.0[3];
        // Rec.601 luma + slight bias toward darker (so the muted version
        // contrasts with the bright idle version on hover/inspection).
        let y_lum = (0.299 * r + 0.587 * g + 0.114 * b) * 0.75;
        let v = y_lum.round().clamp(0.0, 255.0) as u8;
        out.put_pixel(x, y, Rgba([v, v, v, a]));
    }

    // Pass 2: badge. Bottom-right corner. Filled circle, then a diagonal slash
    // through it. Sized to ~40% of the icon's edge so it reads at 32x32.
    let badge_radius = (w as f32 * 0.22).round() as i32;
    let badge_cx = w as i32 - badge_radius - 1;
    let badge_cy = h as i32 - badge_radius - 1;
    let badge_color = Rgba([0xC0u8, 0x39, 0x2Bu8, 0xFFu8]); // muted red
    let slash_color = Rgba([0xFFu8, 0xFFu8, 0xFFu8, 0xFFu8]); // white slash for contrast

    // Filled circle.
    for y in 0..h as i32 {
        for x in 0..w as i32 {
            let dx = x - badge_cx;
            let dy = y - badge_cy;
            let d2 = dx * dx + dy * dy;
            if d2 <= badge_radius * badge_radius {
                out.put_pixel(x as u32, y as u32, badge_color);
            }
        }
    }

    // Diagonal slash. Line from top-left to bottom-right of the badge,
    // 2px thick at 32x32 (scaled with icon size).
    let slash_thickness = ((w as f32) / 16.0).max(1.0);
    let p0 = (badge_cx - badge_radius + 2, badge_cy - badge_radius + 2);
    let p1 = (badge_cx + badge_radius - 2, badge_cy + badge_radius - 2);
    draw_thick_line(&mut out, p0, p1, slash_thickness, slash_color);

    out
}

/// Draw a thick line via per-pixel distance check (simple but pixel-accurate
/// at icon sizes where Bresenham looks chunky).
fn draw_thick_line(
    img: &mut RgbaImage,
    p0: (i32, i32),
    p1: (i32, i32),
    thickness: f32,
    color: Rgba<u8>,
) {
    let (w, h) = img.dimensions();
    let (x0, y0) = (p0.0 as f32, p0.1 as f32);
    let (x1, y1) = (p1.0 as f32, p1.1 as f32);
    let dx = x1 - x0;
    let dy = y1 - y0;
    let length = (dx * dx + dy * dy).sqrt().max(1.0);
    let nx = -dy / length;
    let ny = dx / length;
    let half = thickness / 2.0;

    let min_x = (x0.min(x1) - half).floor().max(0.0) as u32;
    let max_x = (x0.max(x1) + half).ceil().min(w as f32 - 1.0) as u32;
    let min_y = (y0.min(y1) - half).floor().max(0.0) as u32;
    let max_y = (y0.max(y1) + half).ceil().min(h as f32 - 1.0) as u32;

    for y in min_y..=max_y {
        for x in min_x..=max_x {
            let px = x as f32 - x0;
            let py = y as f32 - y0;
            // Project onto line direction; reject if outside segment.
            let t = (px * dx + py * dy) / (length * length);
            if !(0.0..=1.0).contains(&t) {
                continue;
            }
            // Perpendicular distance.
            let pd = (px * nx + py * ny).abs();
            if pd <= half {
                img.put_pixel(x, y, color);
            }
        }
    }
}

fn write_rgba(path: &std::path::Path, img: &RgbaImage) {
    let raw = img.as_raw();
    std::fs::write(path, raw).expect("write rgba");
}
