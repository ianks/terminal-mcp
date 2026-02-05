use thiserror::Error;

#[derive(Error, Debug)]
pub enum TerminalError {
    #[error("PTY error: {0}")]
    Pty(String),

    #[error("Session not found: {0}")]
    SessionNotFound(String),

    #[error("Session already exists: {0}")]
    SessionExists(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Channel send error")]
    ChannelSend,

    #[error("Command timeout after {0}ms")]
    CommandTimeout(u64),

    #[error("Timeout: {0}")]
    Timeout(String),

    #[error("Invalid control character: {0}")]
    InvalidControlChar(char),

    #[error("Invalid command: {0}")]
    InvalidCommand(String),

    #[error("PTY not available")]
    PtyNotAvailable,

    #[error("Buffer not clean: {0}")]
    BufferNotClean(String),

    #[error("Unread output: {0}")]
    UnreadOutput(String),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

pub type Result<T> = std::result::Result<T, TerminalError>;
