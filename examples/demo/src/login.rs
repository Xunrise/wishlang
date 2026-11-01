use crate::SimpleLoginScreen;
use crate::screen::Screen;

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
        Field { label: label.into() }
    }
}

impl PasswordField {
    /// Creates an empty password field labelled `label`.
    pub fn new(label: impl Into<String>) -> Self {
        PasswordField { label: label.into() }
    }
}

#[allow(non_snake_case)]
impl SimpleLoginScreen {
    /// Creates a login screen titled "Hello World" with a user name and a password field.
    ///
    /// # Decisions
    /// - The fields are labelled "User name" and "Password".
    pub fn new() -> Self {
        SimpleLoginScreen {
            title: "Hello World".to_string(),
            user_name_field: Field::new("User name"),
            password_field: PasswordField::new("Password"),
        }
    }

    /// Replaces the screen's title.
    pub fn withTitle(mut self, title: impl Into<String>) -> Self {
        self.title = title.into();
        self
    }

    /// Colours the login button, e.g. `"Red"`.
    ///
    /// # Mocked
    /// - `SimpleLoginScreen` has no field to keep the colour in, so it is only
    ///   printed to stdout and then forgotten.
    pub fn withLoginButtonOfColor(self, color: &str) -> Self {
        println!("login button colour: {color}");
        self
    }
}

impl Screen for SimpleLoginScreen {
    /// # Mocked
    /// - Describes the screen as text instead of drawing it.
    fn describe(&self) -> String {
        format!(
            "{}\n  [{}: ____]\n  [{}: ****]\n  [Login]",
            self.title, self.user_name_field.label, self.password_field.label
        )
    }
}
