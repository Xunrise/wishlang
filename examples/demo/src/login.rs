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

named_colors! {
    /// A colour for an element of the screen, chosen by name (e.g. `"Red"`).
    ///
    /// # Decisions
    /// - The variants are `Grey`, `Red`, `Green` and `Blue`.
    /// - Converting an unknown name panics with the list of known names.
    ///
    /// # Mocked
    /// - The variants are a small fixed set filled in by `named_colors!`.
    pub enum Color {}
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

    /// Colours the login button, e.g. `"Red"` or `Color::Red`.
    ///
    /// # Mocked
    /// - The colour is only shown in the text description.
    pub fn withLoginButtonOfColor(mut self, color: impl Into<Color>) -> Self {
        self.login_button_color = color.into();
        self
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

impl From<&str> for Color {
    /// Looks up a colour by its name, e.g. `"Red"`.
    fn from(name: &str) -> Self {
        Color::ALL
            .into_iter()
            .find(|color| color.name() == name)
            .unwrap_or_else(|| panic!("unknown colour {name:?}, expected one of {:?}", Color::ALL.map(Color::name)))
    }
}

impl std::fmt::Display for Color {
    /// Shows the colour's name, e.g. `Red`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// Fills an empty `enum Name {}` with the known colour variants, plus
/// `ALL` and `name()`.
macro_rules! named_colors {
    ($(#[$meta:meta])* $vis:vis enum $name:ident {}) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        $vis enum $name {
            Grey,
            Red,
            Green,
            Blue,
        }

        impl $name {
            /// Every known colour.
            pub const ALL: [$name; 4] = [$name::Grey, $name::Red, $name::Green, $name::Blue];

            /// The colour's name, e.g. `"Red"`.
            pub fn name(self) -> &'static str {
                match self {
                    $name::Grey => "Grey",
                    $name::Red => "Red",
                    $name::Green => "Green",
                    $name::Blue => "Blue",
                }
            }
        }
    };
}
use named_colors;
