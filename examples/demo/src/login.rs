use crate::screen::Screen;

/// A login screen with a title, a user name field, a password field and a
/// login button.
///
/// # Mocked
/// - Nothing is drawn; the screen only describes itself as text.
pub struct SimpleLoginScreen {
    title: String,
    user_name_field: Field,
    password_field: PasswordField,
    login_button_color: Color,
}

/// The colour of a button.
///
/// # Decisions
/// - Only a few named colours for now; `"Red"`-style names are parsed with [`Color::from_name`].
pub enum Color {
    Grey,
    Red,
    Green,
    Blue,
}

/// A single-line text input with a label.
///
/// # Mocked
/// - Holds only its label; there is no editable text or rendering yet.
pub struct Field {
    label: String,
}

/// A text input whose content is hidden while typing.
///
/// # Mocked
/// - Holds only its label; there is no editable text, masking or rendering yet.
pub struct PasswordField {
    label: String,
}

impl Field {
    /// Creates an empty field labelled `label`.
    pub fn new(label: impl Into<String>) -> Self {
        Field {
            label: label.into(),
        }
    }
}

impl PasswordField {
    /// Creates an empty password field labelled `label`.
    pub fn new(label: impl Into<String>) -> Self {
        PasswordField {
            label: label.into(),
        }
    }
}

#[allow(non_snake_case)]
impl SimpleLoginScreen {
    /// Creates a login screen titled "Hello World" with a user name and a password field.
    ///
    /// # Decisions
    /// - The fields are labelled "User name" and "Password".
    /// - The login button starts out "Grey".
    pub fn new() -> Self {
        SimpleLoginScreen {
            title: "Hello World".to_string(),
            user_name_field: Field::new("User name"),
            password_field: PasswordField::new("Password"),
            login_button_color: Color::Grey,
        }
    }

    /// Replaces the screen's title.
    pub fn withTitle(mut self, title: impl Into<String>) -> Self {
        self.title = title.into();
        self
    }

    /// Colours the login button by name, e.g. `"Red"`.
    ///
    /// # Decisions
    /// - Panics on an unknown colour name, so typos show up immediately.
    ///
    /// # Mocked
    /// - The colour is only shown in the text description.
    pub fn withLoginButtonOfColor(mut self, color: &str) -> Self {
        self.login_button_color = Color::from_name(color);
        self
    }
}

impl Color {
    /// Looks up a colour by its name, ignoring case (`"Red"`, `"red"`).
    ///
    /// Panics if the name is not one of the known colours.
    pub fn from_name(name: &str) -> Self {
        match name.to_lowercase().as_str() {
            "grey" | "gray" => Color::Grey,
            "red" => Color::Red,
            "green" => Color::Green,
            "blue" => Color::Blue,
            _ => panic!("unknown colour {name:?}; expected Grey, Red, Green or Blue"),
        }
    }
}

/// Shows the colour's name, e.g. `Red`.
impl std::fmt::Display for Color {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            Color::Grey => "Grey",
            Color::Red => "Red",
            Color::Green => "Green",
            Color::Blue => "Blue",
        };
        f.write_str(name)
    }
}

impl Screen for SimpleLoginScreen {
    /// # Mocked
    /// - Describes the screen as text instead of drawing it.
    fn describe(&self) -> String {
        format!(
            "{}\n  [{}: ____]\n  [{}: ****]\n  [Login ({})]",
            self.title,
            self.user_name_field.label,
            self.password_field.label,
            self.login_button_color
        )
    }
}
