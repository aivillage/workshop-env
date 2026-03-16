use thiserror::Error;

#[derive(Error, Debug)]
pub enum HubError {
    #[error("Unrecoverable error during {0}")]
    Error(String),

    #[error("Pod failed to become ready in time")]
    PodNotReady,

    #[error("Global pod limit reached")]
    PodLimitReached,

    #[error("Workshop not found")]
    WorkshopNotFound,
}
