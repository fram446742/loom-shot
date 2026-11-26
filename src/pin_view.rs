//! Layer-shell overlay used by `pin add --layer`.
//!
//! Runs as a detached child (`loom-shot pin-view PATH`) and renders the image
//! on a `zwlr_layer_shell_v1` overlay surface anchored to the top-right corner.
//! The surface has an empty input region, so clicks pass through to whatever is
//! underneath, and it stays alive until the process is killed (e.g. by
//! `pin unpin` sending SIGTERM).

use std::path::Path;

use anyhow::{Context, Result};
use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState, Region};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shell::wlr_layer::{
    Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
    LayerSurfaceConfigure,
};
use smithay_client_toolkit::shm::slot::SlotPool;
use smithay_client_toolkit::shm::{Shm, ShmHandler};
use smithay_client_toolkit::{delegate_registry, registry_handlers};
use wayland_client::globals::registry_queue_init;
use wayland_client::protocol::{wl_output, wl_shm, wl_surface};
use wayland_client::{Connection, QueueHandle};

/// Largest overlay side; bigger images are scaled down to fit.
const MAX_WIDTH: u32 = 640;
const MAX_HEIGHT: u32 = 480;
/// Gap between the overlay and the screen edges.
const MARGIN: i32 = 10;

/// Caps an image to `MAX_WIDTH`x`MAX_HEIGHT` preserving aspect ratio, never
/// returning a zero dimension.
fn fit_size(width: u32, height: u32) -> (u32, u32) {
    let width = width.max(1);
    let height = height.max(1);
    let scale = (MAX_WIDTH as f64 / width as f64)
        .min(MAX_HEIGHT as f64 / height as f64)
        .min(1.0);
    (
        ((width as f64 * scale).round() as u32).max(1),
        ((height as f64 * scale).round() as u32).max(1),
    )
}

/// Converts non-premultiplied RGBA8 to premultiplied BGRA8, the byte order of
/// the `wl_shm` `Argb8888` format on little-endian hosts.
fn premultiplied_bgra(rgba: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rgba.len());
    for pixel in rgba.as_chunks::<4>().0 {
        let (r, g, b, a) = (pixel[0], pixel[1], pixel[2], pixel[3]);
        for channel in [b, g, r] {
            out.push((u16::from(channel) * u16::from(a) / 255) as u8);
        }
        out.push(a);
    }
    out
}

struct Overlay {
    registry: RegistryState,
    output: OutputState,
    shm: Shm,
    layer: LayerSurface,
    pool: SlotPool,
    pixels: Vec<u8>,
    size: (u32, u32),
    _region: Region,
    closed: bool,
}

impl Overlay {
    fn draw(&mut self) {
        let (width, height) = self.size;
        let stride = width as i32 * 4;
        let (buffer, canvas) = match self.pool.create_buffer(
            width as i32,
            height as i32,
            stride,
            wl_shm::Format::Argb8888,
        ) {
            Ok(buffer) => buffer,
            Err(err) => {
                eprintln!("loom-shot pin-view: cannot create buffer: {err}");
                self.closed = true;
                return;
            }
        };
        let len = canvas.len().min(self.pixels.len());
        canvas[..len].copy_from_slice(&self.pixels[..len]);

        let surface = self.layer.wl_surface();
        surface.damage_buffer(0, 0, width as i32, height as i32);
        if let Err(err) = buffer.attach_to(surface) {
            eprintln!("loom-shot pin-view: cannot attach buffer: {err}");
            self.closed = true;
            return;
        }
        self.layer.commit();
    }
}

impl CompositorHandler for Overlay {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_factor: i32,
    ) {
    }

    fn transform_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_transform: wl_output::Transform,
    ) {
    }

    fn frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
    }

    fn surface_enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for Overlay {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output
    }

    fn new_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }

    fn update_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }

    fn output_destroyed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }
}

impl ShmHandler for Overlay {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl LayerShellHandler for Overlay {
    fn closed(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _layer: &LayerSurface) {
        self.closed = true;
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _layer: &LayerSurface,
        _configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        self.draw();
    }
}

impl ProvidesRegistryState for Overlay {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }

    registry_handlers![OutputState];
}

delegate_registry!(Overlay);
smithay_client_toolkit::delegate_dispatch2!(Overlay);

/// Renders `path` as a click-through layer-shell overlay until the connection
/// closes.
pub fn run(path: &Path) -> Result<()> {
    let image = image::open(path)
        .with_context(|| format!("cannot open pin image {}", path.display()))?
        .to_rgba8();
    let (source_width, source_height) = image.dimensions();
    let (width, height) = fit_size(source_width, source_height);
    let scaled = if (width, height) == (source_width, source_height) {
        image
    } else {
        image::imageops::resize(&image, width, height, image::imageops::FilterType::Triangle)
    };
    let pixels = premultiplied_bgra(scaled.as_raw());

    let conn = Connection::connect_to_env().context("cannot connect to the Wayland compositor")?;
    let (globals, mut event_queue) =
        registry_queue_init(&conn).context("cannot enumerate Wayland globals")?;
    let qh = event_queue.handle();

    let compositor =
        CompositorState::bind(&globals, &qh).context("wl_compositor is unavailable")?;
    let layer_shell = LayerShell::bind(&globals, &qh)
        .context("zwlr_layer_shell_v1 is unavailable; the compositor has no layer shell")?;
    let shm = Shm::bind(&globals, &qh).context("wl_shm is unavailable")?;

    let surface = compositor.create_surface(&qh);
    let layer = layer_shell.create_layer_surface(
        &qh,
        surface.clone(),
        Layer::Overlay,
        Some("loom-shot-pin"),
        None,
    );
    layer.set_anchor(Anchor::TOP | Anchor::RIGHT);
    layer.set_margin(MARGIN, MARGIN, MARGIN, MARGIN);
    layer.set_exclusive_zone(0);
    layer.set_keyboard_interactivity(KeyboardInteractivity::None);
    layer.set_size(width, height);

    // Empty input region: all pointer/touch input falls through the pin.
    let region = Region::new(&compositor).context("cannot create the input region")?;
    surface.set_input_region(Some(region.wl_region()));
    layer.commit();

    let pool = SlotPool::new(width as usize * height as usize * 4, &shm)
        .context("cannot create the shm pool")?;

    let mut overlay = Overlay {
        registry: RegistryState::new(&globals),
        output: OutputState::new(&globals, &qh),
        shm,
        layer,
        pool,
        pixels,
        size: (width, height),
        _region: region,
        closed: false,
    };

    while !overlay.closed {
        event_queue
            .blocking_dispatch(&mut overlay)
            .context("Wayland dispatch failed")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_images_are_kept_and_zero_is_clamped() {
        assert_eq!(fit_size(320, 200), (320, 200));
        assert_eq!(fit_size(0, 0), (1, 1));
        assert_eq!(fit_size(1, 1), (1, 1));
    }

    #[test]
    fn large_images_are_capped_preserving_aspect_ratio() {
        assert_eq!(fit_size(1280, 960), (640, 480));
        assert_eq!(fit_size(1920, 1080), (640, 360));
        assert_eq!(fit_size(100, 960), (50, 480));
    }

    #[test]
    fn rgba_is_converted_to_premultiplied_bgra() {
        let rgba = [255, 0, 0, 255, 100, 50, 25, 128, 0, 0, 0, 0];
        assert_eq!(
            premultiplied_bgra(&rgba),
            vec![0, 0, 255, 255, 12, 25, 50, 128, 0, 0, 0, 0]
        );
    }
}
