/// Something an [`App`](crate::app::App) can show inside its window.
pub trait Screen {
    /// A short, human-readable outline of what the screen shows.
    fn describe(&self) -> String;
}
