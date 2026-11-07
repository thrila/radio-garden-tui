use thiserror::Error;

#[derive(Error, Debug)]
pub enum RadioError {
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),

    #[error("API error: {message}")]
    Api { message: String },

    #[error("Stream not found for channel {channel_id}")]
    StreamNotFound { channel_id: String },

    #[error("Unsupported stream format: {0}")]
    UnsupportedStream(String),

    #[error("Audio device error: {0}")]
    DeviceSink(String),

    #[error("Cache error: {0}")]
    Cache(String),

    #[error("JSON parse error: {0}")]
    Json(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, RadioError>;
