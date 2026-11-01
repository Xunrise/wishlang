mod app;
mod window;
use app::App;
use window::Mode;

fn main() {
    App::new("Demo").window().open(Mode::Maximized).run()
}
