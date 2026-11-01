use crate::screen::Screen;
use crate::window::{Mode, WindowSpec};

/// A named application. Start here and describe the window you want.
pub struct App {
    name: String,
    mode: Mode,
    size: Option<(u32, u32)>,
    screen: Option<Box<dyn Screen>>,
}

impl App {
    /// Creates an application; `name` is used as the window title.
    ///
    /// # Decisions
    /// - Defaults to [`Mode::Windowed`], the platform's default size and no screen.
    pub fn new(name: impl Into<String>) -> Self {
        App { name: name.into(), mode: Mode::Windowed, size: None, screen: None }
    }

    /// Chooses the [`Mode`] the window opens in.
    pub fn mode(mut self, mode: Mode) -> Self {
        self.mode = mode;
        self
    }

    /// Sets the window's inner size in logical pixels.
    pub fn size(mut self, width: u32, height: u32) -> Self {
        self.size = Some((width, height));
        self
    }

    /// Puts `screen` inside the window, replacing any previous screen.
    pub fn contains(mut self, screen: impl Screen + 'static) -> Self {
        self.screen = Some(Box::new(screen));
        self
    }

    /// Opens the window and blocks until it is closed.
    ///
    /// # Mocked
    /// - The screen is printed to stdout instead of drawn; the window stays empty.
    pub fn run(self) {
        if let Some(screen) = &self.screen {
            println!("{}", screen.describe());
        }
        let mut spec = WindowSpec::new(self.name);
        if let Some((width, height)) = self.size {
            spec = spec.size(width, height);
        }
        spec.open(self.mode).run();
    }

    /// Begins describing the application's window.
    pub fn window(self) -> WindowSpec {
        WindowSpec::new(self.name)
    }
}
