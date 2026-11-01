use crate::window::WindowSpec;

/// A named application. Start here and describe the window you want.
pub struct App {
    name: String,
}

impl App {
    /// Creates an application; `name` is used as the window title.
    pub fn new(name: impl Into<String>) -> Self {
        App { name: name.into() }
    }

    /// Begins describing the application's window.
    pub fn window(self) -> WindowSpec {
        WindowSpec::new(self.name)
    }
}
