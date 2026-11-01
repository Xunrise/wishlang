mod login;
mod screen;
use login::*;
use window::Mode::*;
mod app;
mod window;
use app::App;
use window::Mode;

fn main() {
    let app = App::new("Demo")
        .mode(Windowed)
        .size(800, 600)
        .contains(
            SimpleLoginScreen::new()
                .withTitle("Please login")
                .withLoginButtonOfColor("Red"),
        )
        .run();
}

struct SimpleLoginScreen{
    title: String = "Hello World",
    user_name_field: Field,
    password_field: PasswordField
}
