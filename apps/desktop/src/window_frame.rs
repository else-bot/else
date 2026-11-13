use std::os::unix::fs::DirBuilderExt as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use gpui_kit::*;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

const SAVE_DELAY: Duration = Duration::from_millis(500);

/// The main window's last frame on one display. macOS reports bounds
/// relative to the window's display.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
struct Frame {
    display: Uuid,
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    full_screen: bool,
}

impl Frame {
    fn capture(window: &Window, cx: &App) -> Option<Self> {
        let display = window.display(cx)?.uuid().ok()?;
        let (bounds, full_screen) = match window.window_bounds() {
            WindowBounds::Windowed(bounds) | WindowBounds::Maximized(bounds) => (bounds, false),
            WindowBounds::Fullscreen(bounds) => (bounds, true),
        };
        Some(Self {
            display,
            x: bounds.origin.x.as_f32(),
            y: bounds.origin.y.as_f32(),
            width: bounds.size.width.as_f32(),
            height: bounds.size.height.as_f32(),
            full_screen,
        })
    }

    fn place(self, min: Size<Pixels>, cx: &App) -> Option<(WindowBounds, DisplayId)> {
        let display = cx
            .displays()
            .into_iter()
            .find(|display| display.uuid().is_ok_and(|uuid| uuid == self.display))?;
        let screen = display.bounds().size;
        let size = size(
            px(self.width).clamp(min.width, screen.width.max(min.width)),
            px(self.height).clamp(min.height, screen.height.max(min.height)),
        );
        let bounds = Bounds::new(point(px(self.x), px(self.y)), size);
        if !Bounds::new(Point::default(), screen).intersects(&bounds) {
            return None;
        }
        let bounds = if self.full_screen {
            WindowBounds::Fullscreen(bounds)
        } else {
            WindowBounds::Windowed(bounds)
        };
        Some((bounds, display.id()))
    }
}

pub fn restore(path: &Path, min: Size<Pixels>, cx: &App) -> Option<(WindowBounds, DisplayId)> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice::<Frame>(&bytes).ok()?.place(min, cx)
}

pub fn remember<V: 'static>(path: PathBuf, window: &mut Window, cx: &mut Context<V>) {
    let mut pending: Option<Task<()>> = None;
    cx.observe_window_bounds(window, move |_, window, cx| {
        let path = path.clone();
        pending.replace(cx.spawn_in(window, async move |_, cx| {
            cx.background_executor().timer(SAVE_DELAY).await;
            let _ = cx.update(|window, cx| {
                if let Some(frame) = Frame::capture(window, cx) {
                    let _ = save(&path, frame);
                }
            });
        }));
    })
    .detach();
}

fn save(path: &Path, frame: Frame) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)?;
    }
    crate::config::replace_private(path, &serde_json::to_vec(&frame)?)
}
