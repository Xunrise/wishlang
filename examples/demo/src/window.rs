use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::window::{Window, WindowId};

/// How a window is shown when it opens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// A regular, decorated window at the platform's default size; not maximized.
    Windowed,
    /// Fills the screen's work area but keeps decorations; not exclusive fullscreen.
    Maximized,
}

/// A window that has been described but not yet shown.
pub struct WindowSpec {
    title: String,
    size: Option<(u32, u32)>,
}

/// A window that is configured to open and ready to [`run`](OpenWindow::run).
pub struct OpenWindow {
    title: String,
    mode: Mode,
    size: Option<(u32, u32)>,
}

impl WindowSpec {
    pub(crate) fn new(title: String) -> Self {
        WindowSpec { title, size: None }
    }

    /// Sets the window's inner size in logical pixels.
    pub fn size(mut self, width: u32, height: u32) -> Self {
        self.size = Some((width, height));
        self
    }

    /// Chooses the [`Mode`] the window opens in. Nothing is shown until `run`.
    pub fn open(self, mode: Mode) -> OpenWindow {
        OpenWindow { title: self.title, mode, size: self.size }
    }
}

impl OpenWindow {
    /// Opens the window and blocks until it is closed.
    ///
    /// # Decisions
    /// - Failures to create the event loop or run it panic with a message, so
    ///   that `run()` returns `()` as the call site expects.
    pub fn run(self) {
        let event_loop = EventLoop::new().expect("could not create the event loop");
        let mut handler = Handler { spec: self, window: None };
        event_loop.run_app(&mut handler).expect("event loop failed");
    }
}

struct Handler {
    spec: OpenWindow,
    window: Option<Window>,
}

impl ApplicationHandler for Handler {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let mut attrs = Window::default_attributes()
            .with_title(self.spec.title.clone())
            .with_maximized(self.spec.mode == Mode::Maximized);
        if let Some((width, height)) = self.spec.size {
            attrs = attrs.with_inner_size(LogicalSize::new(width, height));
        }
        self.window = Some(event_loop.create_window(attrs).expect("could not create the window"));
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        if let WindowEvent::CloseRequested = event {
            event_loop.exit();
        }
    }
}
