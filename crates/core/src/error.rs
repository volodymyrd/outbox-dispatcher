use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    /// A specified callback name was not found in the event's callback array.
    #[error("Callback target missing: {0}")]
    CallbackTargetMissing(String),

    /// Encountered structurally invalid data in a row.
    #[error("Invalid data: {0}")]
    InvalidData(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Aggregated validation errors returned by [`AppConfig::validate`] and [`KeyRing::load`].
///
/// Collects all problems instead of stopping at the first, so operators see every issue
/// in a single startup failure.
#[derive(Debug, Error)]
#[error("{} validation error(s): {}", self.0.len(), self.0.join("; "))]
pub struct ValidationErrors(pub Vec<String>);
