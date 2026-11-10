// Prevent console window in addition to Slint window in Windows release builds when, e.g., starting the app via file manager. Ignored on other platforms.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::{
    cell::{Cell, RefCell},
    env::{self, args_os},
    error::Error,
    mem,
    path::{Path, PathBuf},
    rc::Rc,
};

slint::include_modules!();
use i_slint_backend_winit::{EventResult as WinitWindowEventResult, WinitWindowAccessor};
use i_slint_core::items::{EventResult, PointerEventKind};
use image::{ColorType, DynamicImage, GenericImageView, ImageDecoder, ImageReader};
use resvg::{tiny_skia, usvg};
use slint::{
    BackendSelector, Image, PhysicalSize, Rgba8Pixel, SharedPixelBuffer,
    platform::{PointerEventButton, WindowEvent::CloseRequested},
    set_xdg_app_id,
};
use winit::event::WindowEvent as WinitEvent;

fn load_oriented_image(path: &Path) -> Result<DynamicImage, Box<dyn Error>> {
    let mut decoder = ImageReader::open(path)?
        .with_guessed_format()?
        .into_decoder()?;
    let orientation = decoder.orientation()?;
    let mut image = DynamicImage::from_decoder(decoder)?;
    image.apply_orientation(orientation);
    Ok(image)
}

fn load_any_image(path: &Path) -> Result<DynamicImage, Box<dyn Error>> {
    load_oriented_image(path)
        .or_else(|_| load_by_extension(path))
        .or_else(|_| load_svg_image(path))
}

fn load_by_extension(path: &Path) -> Result<DynamicImage, Box<dyn Error>> {
    Ok(ImageReader::open(path)?.decode()?)
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

fn display_image(img: &DynamicImage) -> Image {
    let (width, height) = img.dimensions();
    let buffer = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(&img.to_rgba8(), width, height);
    Image::from_rgba8(buffer)
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
        let placeholder = DynamicImage::new(1, 1, ColorType::Rgba8);
        let img = mem::replace(&mut *self.src.borrow_mut(), placeholder);
        let img = if dir < 0 {
            img.rotate270()
        } else {
            img.rotate90()
        };
        *self.src.borrow_mut() = img;
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

    let initial = if let Some(path) = args_os().nth(1) {
        println!("{path:?}");
        let path = PathBuf::from(path);
        load_any_image(&path)?
    } else {
        DynamicImage::new(1, 1, ColorType::Rgba8)
    };

    let (width, height) = initial.dimensions();
    let viewer = Rc::new(Viewer {
        src: RefCell::new(initial),
        dim: Cell::new(PhysicalSize::new(width, height)),
        scale: Cell::new(1.0),
        min_scale: Cell::new(25f32 / f32::min(width as f32, height as f32)),
    });

    let app = AppWindow::new()?;
    viewer.push_image(&app);
    app.window().on_winit_window_event({
        let viewer = viewer.clone();
        let app_handle = app.as_weak();
        move |_win, event| {
            let app = app_handle.unwrap();
            match event {
                WinitEvent::HoveredFile(_f) => {
                    #[cfg(debug_assertions)]
                    println!("Hover {_f:?}");
                    WinitWindowEventResult::Propagate
                }
                WinitEvent::HoveredFileCancelled => {
                    #[cfg(debug_assertions)]
                    println!("Hover Cancel");
                    WinitWindowEventResult::Propagate
                }
                WinitEvent::DroppedFile(f) => {
                    #[cfg(debug_assertions)]
                    println!("Dropped {f:?}");
                    match load_any_image(f) {
                        Ok(img) => {
                            *viewer.src.borrow_mut() = img;
                            viewer.scale.set(1.0);
                            viewer.push_image(&app);
                            WinitWindowEventResult::PreventDefault
                        }
                        Err(e) => {
                            eprintln!("Failed to load dropped file: {e}");
                            WinitWindowEventResult::Propagate
                        }
                    }
                }
                WinitEvent::Focused(focused) => {
                    #[cfg(debug_assertions)]
                    println!("Entered");
                    app.set_focused(*focused);
                    WinitWindowEventResult::Propagate
                }
                WinitEvent::CursorEntered { .. } => {
                    #[cfg(debug_assertions)]
                    println!("Entered");
                    app.set_entered(true);
                    app.set_dragged(false);
                    WinitWindowEventResult::Propagate
                }
                WinitEvent::CursorLeft { .. } => {
                    #[cfg(debug_assertions)]
                    println!("Left");
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
            viewer
                .scale
                .set(f32::max(viewer.min_scale.get(), new_scale));
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
            #[cfg(debug_assertions)]
            println!("Closing");
            let app = app_handle.unwrap();
            app.window().dispatch_event(CloseRequested);
        }
    });
    app.on_pointer_event({
        let app_handle = app.as_weak();
        move |e| {
            if e.button == PointerEventButton::Left && e.kind == PointerEventKind::Down {
                #[cfg(debug_assertions)]
                println!("Dragged");
                let app = app_handle.unwrap();
                app.set_dragged(true);
                app.window().with_winit_window(|win| {
                    let _ = win.drag_window();
                });
            }
            if e.button == PointerEventButton::Right && e.kind == PointerEventKind::Down {
                #[cfg(debug_assertions)]
                println!("Closing");
                let app = app_handle.unwrap();
                app.window().dispatch_event(CloseRequested);
            }
            if e.button == PointerEventButton::Left && e.kind == PointerEventKind::Up {
                #[cfg(debug_assertions)]
                println!("Undragged");
                let app = app_handle.unwrap();
                app.set_dragged(false);
            }
        }
    });

    app.run()?;

    Ok(())
}
