use thiserror::Error;

pub type Result<T> = std::result::Result<T, CloudError>;

/// What went wrong talking to a storage account, in words a user (or agent) can act on.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CloudError {
    #[error("not found on the storage account")]
    NotFound,
    #[error("the storage account is full")]
    QuotaExceeded,
    #[error("{size} bytes is more than the {max} bytes allowed")]
    TooLarge { size: u64, max: u64 },
    #[error("invalid storage name: {0}")]
    InvalidName(String),
    #[error("damaged data: {0}")]
    Corrupt(String),
    #[error("not supported: {0}")]
    Unsupported(String),
    #[error("storage error: {0}")]
    Io(String),
}

impl From<std::io::Error> for CloudError {
    fn from(e: std::io::Error) -> Self {
        match e.kind() {
            std::io::ErrorKind::NotFound => CloudError::NotFound,
            std::io::ErrorKind::StorageFull => CloudError::QuotaExceeded,
            _ => CloudError::Io(e.to_string()),
        }
    }
}
