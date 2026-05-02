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
