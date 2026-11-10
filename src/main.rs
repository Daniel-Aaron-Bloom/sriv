// Prevent console window in addition to Slint window in Windows release builds when, e.g., starting the app via file manager. Ignored on other platforms.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::{
    cell::{Cell, RefCell},
    env::{self, args_os},
    error::Error,
    path::{Path, PathBuf},
    rc::Rc,
};

slint::include_modules!();
use i_slint_backend_winit::{EventResult as WinitWindowEventResult, WinitWindowAccessor};
use i_slint_core::items::{EventResult, PointerEventKind};
use image::{DynamicImage, GenericImageView, ImageDecoder, ImageReader};
use libheif_rs::{ColorSpace, HeifContext, LibHeif, RgbChroma};
use resvg::{tiny_skia, usvg};
use slint::{
    BackendSelector, Image, PhysicalSize, SharedPixelBuffer,
    platform::{PointerEventButton, WindowEvent::CloseRequested},
    set_xdg_app_id,
};
use winit::event::WindowEvent as WinitEvent;

/// println! in debug builds only; compiled away in release builds.
macro_rules! dlog {
    ($($arg:tt)*) => {
        #[cfg(debug_assertions)]
        println!($($arg)*)
    };
}

fn load_oriented_image(path: &Path) -> Result<DynamicImage, Box<dyn Error>> {
    let mut decoder = ImageReader::open(path)?
        .with_guessed_format()?
        .into_decoder()?;
    let orientation = decoder.orientation()?;
    let mut image = DynamicImage::from_decoder(decoder)?;
    image.apply_orientation(orientation);
    Ok(image)
}

fn load_by_extension(path: &Path) -> Result<DynamicImage, Box<dyn Error>> {
    Ok(ImageReader::open(path)?.decode()?)
}

// HEIC/HEIF (e.g. iPhone photos), which the image crate cannot decode.
// libheif applies the container's geometric transformations (rotation,
// mirroring) while decoding, so orientation is handled as with EXIF above.
fn load_heic_image(path: &Path) -> Result<DynamicImage, Box<dyn Error>> {
    let name = path.to_str().ok_or("HEIC path is not valid UTF-8")?;
    let context = HeifContext::read_from_file(name)?;
    let handle = context.primary_image_handle()?;
    let decoded = LibHeif::new().decode(&handle, ColorSpace::Rgb(RgbChroma::Rgba), None)?;
    let plane = decoded
        .planes()
        .interleaved
        .ok_or("libheif returned no interleaved RGBA plane")?;
    // Plane rows are padded to the stride; repack them without the padding.
    let row_len = plane.width as usize * 4;
    let mut pixels = Vec::with_capacity(plane.height as usize * row_len);
    for row in 0..plane.height as usize {
        pixels.extend_from_slice(&plane.data[row * plane.stride..][..row_len]);
    }
    let image = image::RgbaImage::from_raw(plane.width, plane.height, pixels)
        .ok_or("libheif plane dimensions do not match")?;
    Ok(DynamicImage::ImageRgba8(image))
}

// Rasterize SVGs with the same engine Slint's own loader uses (resvg), so they
// behave like any other image (rotation included). Renders at the SVG's
// intrinsic size; falls back to 100x100 when none is specified.
fn load_svg_image(path: &Path) -> Result<DynamicImage, Box<dyn Error>> {
    let svg_data = std::fs::read(path)?;
    let mut options = usvg::Options {
        resources_dir: path.parent().map(|p| p.to_path_buf()),
        ..Default::default()
    };
    options.fontdb_mut().load_system_fonts();
    let tree = usvg::Tree::from_data(&svg_data, &options)?;
    let size = tree.size().to_int_size();
    let mut pixmap = tiny_skia::Pixmap::new(size.width(), size.height())
        .ok_or("failed to allocate SVG raster buffer")?;
    resvg::render(&tree, tiny_skia::Transform::default(), &mut pixmap.as_mut());
    let rgba = image::RgbaImage::from_raw(size.width(), size.height(), pixmap.take_demultiplied())
        .ok_or("SVG raster buffer dimensions do not match")?;
    Ok(DynamicImage::ImageRgba8(rgba))
}

// Try each loader in turn. If everything fails, report the error from the
// first attempt: later fallbacks produce misleading messages (e.g. an SVG
// parse error for a corrupt PNG). HEIF containers are the exception — their
// most specific error comes from the libheif attempt.
fn load_any_image(path: &Path) -> Result<DynamicImage, Box<dyn Error>> {
    let first_err = match load_oriented_image(path) {
        Ok(img) => return Ok(img),
        Err(e) => e,
    };
    if let Ok(img) = load_by_extension(path) {
        return Ok(img);
    }
    let heic_err = match load_heic_image(path) {
        Ok(img) => return Ok(img),
        Err(e) => e,
    };
    if matches!(
        path.extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.to_ascii_lowercase())
            .as_deref(),
        Some("heic" | "heif" | "hif" | "avif")
    ) {
        return Err(heic_err);
    }
    load_svg_image(path).map_err(|_| first_err)
}

// For already-Rgba8 sources, pass the buffer straight to Slint instead of
// cloning through to_rgba8() — one full-image copy less per push.
fn display_image(img: &DynamicImage) -> Image {
    let (width, height) = img.dimensions();
    match img {
        DynamicImage::ImageRgba8(buffer) => {
            Image::from_rgba8(SharedPixelBuffer::clone_from_slice(buffer, width, height))
        }
        other => Image::from_rgba8(SharedPixelBuffer::clone_from_slice(
            &other.to_rgba8(),
            width,
            height,
        )),
    }
}

// Neutral dark placeholder shown when launched without a file (or when the
// initial file failed to load), sized so the window is usable instead of
// 1x1 pixels.
fn placeholder_image() -> DynamicImage {
    DynamicImage::ImageRgba8(image::ImageBuffer::from_pixel(
        512,
        384,
        image::Rgba([0x20, 0x20, 0x24, 0xff]),
    ))
}

struct Viewer {
    src: RefCell<DynamicImage>,
    dim: Cell<PhysicalSize>,
    scale: Cell<f32>,
    min_scale: Cell<f32>,
}

impl Viewer {
    // Push the current source image to the UI and resize the window to match.
    fn push_image(&self, app: &AppWindow) {
        let img = self.src.borrow();
        let (width, height) = img.dimensions();
        self.dim.set(PhysicalSize::new(width, height));
        self.min_scale
            .set(25f32 / f32::min(width as f32, height as f32));
        let picture = display_image(&img);
        drop(img);
        app.set_picture(picture);
        self.apply_size(app);
    }

    fn apply_size(&self, app: &AppWindow) {
        let mut size = self.dim.get().to_logical(app.window().scale_factor());
        size.width *= self.scale.get();
        size.height *= self.scale.get();
        app.window().set_size(size);
    }

    fn rotate(&self, app: &AppWindow, dir: i32) {
        // rotate90/rotate270 take &self and return a new image, so there is
        // nothing to swap in place.
        let rotated = if dir < 0 {
            self.src.borrow().rotate270()
        } else {
            self.src.borrow().rotate90()
        };
        *self.src.borrow_mut() = rotated;
        self.push_image(app);
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let mut selector = BackendSelector::new().backend_name("winit".into());
    // winit 0.30 implements file drag-and-drop only on X11 (plus macOS and
    // Windows); its Wayland backend has no DnD, so HoveredFile/DroppedFile
    // never fire there. When running under a Wayland session, force the X11
    // backend so drops go through XWayland.
    #[cfg(target_os = "linux")]
    if env::var_os("WAYLAND_DISPLAY").is_some() || env::var_os("WAYLAND_SOCKET").is_some() {
        use winit::platform::x11::EventLoopBuilderExtX11;
        let mut event_loop_builder = winit::event_loop::EventLoop::with_user_event();
        event_loop_builder.with_x11();
        selector = selector.with_winit_event_loop_builder(event_loop_builder);
    }
    selector.select()?;
    set_xdg_app_id("sriv")?;

    // Load the file passed on the command line, if any. On failure, still
    // open a window and report the problem in the UI instead of dying without
    // a trace — the desktop entry launches us without a terminal.
    let mut startup_error = None;
    let initial = if let Some(arg) = args_os().nth(1) {
        let path = PathBuf::from(arg);
        dlog!("Loading {path:?}");
        match load_any_image(&path) {
            Ok(img) => img,
            Err(e) => {
                startup_error = Some(format!("Couldn't load {}: {e}", path.display()));
                placeholder_image()
            }
        }
    } else {
        placeholder_image()
    };

    let (width, height) = initial.dimensions();
    let viewer = Rc::new(Viewer {
        src: RefCell::new(initial),
        dim: Cell::new(PhysicalSize::new(width, height)),
        scale: Cell::new(1.0),
        min_scale: Cell::new(25f32 / f32::min(width as f32, height as f32)),
    });

    let app = AppWindow::new()?;
    if let Some(error) = startup_error {
        app.set_error_text(error.into());
    }
    viewer.push_image(&app);
    app.window().on_winit_window_event({
        let viewer = viewer.clone();
        let app_handle = app.as_weak();
        move |_win, event| {
            let app = app_handle.unwrap();
            match event {
                WinitEvent::HoveredFile(_f) => {
                    dlog!("Hover {_f:?}");
                    WinitWindowEventResult::Propagate
                }
                WinitEvent::HoveredFileCancelled => {
                    dlog!("Hover Cancel");
                    WinitWindowEventResult::Propagate
                }
                WinitEvent::DroppedFile(f) => {
                    dlog!("Dropped {f:?}");
                    match load_any_image(f) {
                        Ok(img) => {
                            *viewer.src.borrow_mut() = img;
                            viewer.scale.set(1.0);
                            viewer.push_image(&app);
                            app.set_error_text("".into());
                            WinitWindowEventResult::PreventDefault
                        }
                        Err(e) => {
                            dlog!("Failed to load dropped file: {e}");
                            app.set_error_text(
                                format!("Couldn't load {}: {e}", f.display()).into(),
                            );
                            WinitWindowEventResult::Propagate
                        }
                    }
                }
                WinitEvent::Focused(focused) => {
                    dlog!("Focused");
                    app.set_focused(*focused);
                    WinitWindowEventResult::Propagate
                }
                WinitEvent::CursorEntered { .. } => {
                    dlog!("Entered");
                    app.set_entered(true);
                    app.set_dragged(false);
                    WinitWindowEventResult::Propagate
                }
                WinitEvent::CursorLeft { .. } => {
                    dlog!("Left");
                    app.set_entered(false);
                    WinitWindowEventResult::Propagate
                }
                _ => WinitWindowEventResult::Propagate,
            }
        }
    });
    app.on_get_width({
        let viewer = viewer.clone();
        let app_handle = app.as_weak();
        move || {
            let app = app_handle.unwrap();
            viewer
                .dim
                .get()
                .to_logical(app.window().scale_factor())
                .width
        }
    });
    app.on_get_height({
        let viewer = viewer.clone();
        let app_handle = app.as_weak();
        move || {
            let app = app_handle.unwrap();
            viewer
                .dim
                .get()
                .to_logical(app.window().scale_factor())
                .height
        }
    });
    app.on_scroll_event({
        let viewer = viewer.clone();
        let app_handle = app.as_weak();
        move |e| {
            let scale_factor = if e.modifiers.control {
                1000f32
            } else if e.modifiers.shift {
                200f32
            } else {
                800f32
            };
            let app = app_handle.unwrap();
            let new_scale = viewer.scale.get() + e.delta_y / scale_factor;
            // Cap the longest displayed side at 8192 logical pixels so a few
            // wheel ticks can't blow the window far past the screen.
            let size = viewer.dim.get();
            let max_scale = 8192f32 / f32::max(size.width as f32, size.height as f32);
            viewer
                .scale
                .set(new_scale.min(max_scale).max(viewer.min_scale.get()));
            viewer.apply_size(&app);
            EventResult::Accept
        }
    });
    app.on_rotate({
        let viewer = viewer.clone();
        let app_handle = app.as_weak();
        move |dir| {
            let app = app_handle.unwrap();
            viewer.rotate(&app, dir);
        }
    });
    app.on_close_requested({
        let app_handle = app.as_weak();
        move || {
            dlog!("Closing");
            let app = app_handle.unwrap();
            app.window().dispatch_event(CloseRequested);
        }
    });
    app.on_pointer_event({
        let app_handle = app.as_weak();
        move |e| {
            if e.button == PointerEventButton::Left && e.kind == PointerEventKind::Down {
                dlog!("Dragged");
                let app = app_handle.unwrap();
                app.set_dragged(true);
                app.window().with_winit_window(|win| {
                    let _ = win.drag_window();
                });
            }
            if e.button == PointerEventButton::Right && e.kind == PointerEventKind::Down {
                dlog!("Closing");
                let app = app_handle.unwrap();
                app.window().dispatch_event(CloseRequested);
            }
            if e.button == PointerEventButton::Left && e.kind == PointerEventKind::Up {
                dlog!("Undragged");
                let app = app_handle.unwrap();
                app.set_dragged(false);
            }
        }
    });

    app.run()?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use libheif_rs::{Channel, CompressionFormat};

    // Encode a solid-color image into a real HEIC via libheif's own encoder,
    // then decode it through load_heic_image. Skips on machines whose libheif
    // has no HEVC encoder (x265) plugin.
    #[test]
    fn heic_roundtrip() -> Result<(), Box<dyn Error>> {
        const W: u32 = 64;
        const H: u32 = 48;
        const COLOR: [u8; 4] = [200, 30, 140, 255];

        let lh = LibHeif::new();
        let mut encoder = match lh.encoder_for_format(CompressionFormat::Hevc) {
            Ok(encoder) => encoder,
            Err(_) => return Ok(()),
        };

        let mut encoded_image = libheif_rs::Image::new(W, H, ColorSpace::Rgb(RgbChroma::Rgba))?;
        encoded_image.create_plane(Channel::Interleaved, W, H, 8)?;
        {
            let plane = encoded_image
                .planes_mut()
                .interleaved
                .expect("no interleaved plane");
            for row in 0..H as usize {
                for px in plane.data[row * plane.stride..][..W as usize * 4]
                    .as_chunks_mut::<4>()
                    .0
                {
                    px.copy_from_slice(&COLOR);
                }
            }
        }

        let mut context = HeifContext::new()?;
        context.encode_image(&encoded_image, &mut encoder, None)?;
        let bytes = context.write_to_bytes()?;
        assert!(
            bytes.len() > 8,
            "suspiciously tiny HEIC: {} bytes",
            bytes.len()
        );

        let path = std::env::temp_dir().join("sriv-heic-roundtrip.heic");
        std::fs::write(&path, &bytes)?;
        let decoded = load_heic_image(&path);
        let _ = std::fs::remove_file(&path);
        let decoded = decoded?;

        assert_eq!(decoded.dimensions(), (W, H));
        // x265 is lossy, but a solid color should survive roughly intact.
        let center = decoded.get_pixel(W / 2, H / 2);
        for (got, want) in center.0.iter().zip(COLOR.iter()) {
            assert!(
                i32::from(*got).abs_diff(i32::from(*want)) < 40,
                "drifted: {center:?}"
            );
        }
        Ok(())
    }
}
