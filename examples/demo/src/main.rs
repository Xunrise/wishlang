mod login;
mod screen;
use login::*;
use window::Mode::*;
mod app;
mod window;
use app::App;

fn main() {
    App::new("Demo")
        .mode(Windowed)
        .size(800, 600)
        .contains(
            SimpleLoginScreen::new()
                .withTitle("Please login")
                .withLoginButtonOfColor("Red"),
        )
        .run();
}
