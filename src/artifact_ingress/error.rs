use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactIngressError {
    code: &'static str,
    message: String,
}

impl ArtifactIngressError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub fn code(&self) -> &'static str {
        self.code
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for ArtifactIngressError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for ArtifactIngressError {}

pub type ArtifactIngressResult<T> = Result<T, ArtifactIngressError>;
