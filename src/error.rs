use std::fmt::{Display, Formatter};

#[derive(Debug)]
pub struct AppError {
    pub code: i32,
    pub message: String,
}

impl Display for AppError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for AppError {}

pub fn app_error(code: i32, message: impl Into<String>) -> AppError {
    AppError {
        code,
        message: message.into(),
    }
}
