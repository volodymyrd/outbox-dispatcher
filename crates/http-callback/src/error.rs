/// Error type for the `outbox-dispatcher-http-callback` crate.
#[derive(Debug, thiserror::Error)]
pub enum HttpCallbackError {
    #[error("failed to build reqwest client: {0}")]
    ClientBuild(#[from] reqwest::Error),
}
