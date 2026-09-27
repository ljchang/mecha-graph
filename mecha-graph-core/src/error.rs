use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("embedding error: {0}")]
    Embed(String),
    #[error("parse error: {0}")]
    Parse(String),
    #[error("{0}")]
    Other(String),
    /// The model server gave no usable answer — unreachable, timed out, a 5xx
    /// (a router answers 503 while it swaps a model in), or a body that could
    /// not be read. Not an outcome of the input: a caller that marks inputs
    /// as tried (extract's poison-episode rule) must not mark on this one, or
    /// an infrastructure blip ages out every episode behind it (2026-09-27).
    #[error("{0}")]
    Transport(String),
    /// The server took the request and gave no answer within the client's
    /// timeout. Whose that is cannot be read off the error: a server working
    /// through a very long input and a link that stalled mid-request look the
    /// same from here. Callers ask the server (`ChatClient::canary`) and give
    /// the input one more try before charging it (found on review of #22).
    #[error("{0}")]
    Timeout(String),
}

pub type Result<T> = std::result::Result<T, Error>;
