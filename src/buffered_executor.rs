use crate::error::{Result, TerminalError};
use crate::terminal::TerminalManager;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::time::{sleep, Duration, Instant};

/// Represents a pending command that was executed asynchronously
#[derive(Debug, Clone)]
struct PendingCommand {
    #[allow(dead_code)]
    command: String,
    #[allow(dead_code)]
    timestamp: Instant,
    #[allow(dead_code)]
    before_buffer: String,
}

/// Tracks the buffer state for intelligent output extraction
#[derive(Debug, Clone)]
struct BufferState {
    last_read: String,
    last_read_timestamp: Instant,
    unread_output: bool,
}

/// BufferedCommandExecutor provides intelligent buffering and debouncing
/// for terminal commands, allowing for more efficient output retrieval.
pub struct BufferedCommandExecutor {
    manager: Arc<TerminalManager>,
    session_name: String,
    buffer_state: Arc<Mutex<BufferState>>,
    pending_command: Arc<Mutex<Option<PendingCommand>>>,
    debounce_ms: u64,
}

impl BufferedCommandExecutor {
    /// Create a new buffered executor for a specific session
    pub fn new(manager: Arc<TerminalManager>, session_name: String) -> Self {
        Self {
            manager,
            session_name,
            buffer_state: Arc::new(Mutex::new(BufferState {
                last_read: String::new(),
                last_read_timestamp: Instant::now(),
                unread_output: false,
            })),
            pending_command: Arc::new(Mutex::new(None)),
            debounce_ms: 50,
        }
    }

    /// Executes a command with intelligent buffering and output tracking.
    /// If there's unread output from a previous command, it will return an error
    /// with the unread output unless force_execute is true.
    pub async fn execute_command(
        &self,
        command: &str,
        force_execute: bool,
    ) -> Result<ExecuteResult> {
        let mut buffer_state = self.buffer_state.lock().await;

        // Check if there's unread output from a previous command
        if buffer_state.unread_output && !force_execute {
            let current_buffer = self.retrieve_buffer().await?;
            let unread_output = self.extract_new_output(&buffer_state.last_read, &current_buffer);

            // Mark as read now
            buffer_state.last_read = current_buffer;
            buffer_state.last_read_timestamp = Instant::now();
            buffer_state.unread_output = false;

            return Err(TerminalError::UnreadOutput(format!(
                "Unread output detected from previous command. Output:\n{}",
                unread_output
            )));
        }

        // Get the buffer state before executing
        let before_buffer = self.retrieve_buffer().await?;

        // Execute the command and wait for completion
        self.manager.write_line(&self.session_name, command).await?;

        // Wait for the command to complete with buffer stabilization
        let after_buffer = self.wait_for_stable_buffer().await?;

        // Extract the command output
        let output = self.extract_new_output(&before_buffer, &after_buffer);

        // Update buffer state
        buffer_state.last_read = after_buffer.clone();
        buffer_state.last_read_timestamp = Instant::now();
        buffer_state.unread_output = false;

        Ok(ExecuteResult {
            output,
            full_buffer: Some(after_buffer),
            has_unread_output: false,
            unread_output: None,
        })
    }

    /// Executes a command without waiting for completion.
    /// Useful for long-running commands where you want to check output periodically.
    pub async fn execute_command_async(&self, command: &str) -> Result<()> {
        let mut buffer_state = self.buffer_state.lock().await;

        // Mark that we have pending unread output
        buffer_state.unread_output = true;

        // Store the current buffer state
        let before_buffer = self.retrieve_buffer().await?;
        let mut pending = self.pending_command.lock().await;
        *pending = Some(PendingCommand {
            command: command.to_string(),
            timestamp: Instant::now(),
            before_buffer,
        });

        // Execute without waiting
        self.manager.write_line(&self.session_name, command).await?;

        Ok(())
    }

    /// Reads any new output since the last read operation.
    /// This can be called multiple times to get streaming output.
    pub async fn read_new_output(&self) -> Result<ReadResult> {
        let mut buffer_state = self.buffer_state.lock().await;
        let current_buffer = self.retrieve_buffer().await?;
        let new_output = self.extract_new_output(&buffer_state.last_read, &current_buffer);

        // Update the last read state
        buffer_state.last_read = current_buffer;
        buffer_state.last_read_timestamp = Instant::now();

        // Check if the command is still running
        let is_complete = self.is_command_complete().await?;

        if is_complete {
            buffer_state.unread_output = false;
            let mut pending = self.pending_command.lock().await;
            *pending = None;
        }

        Ok(ReadResult {
            output: new_output,
            is_complete,
        })
    }

    /// Checks if there's any unread output available
    pub async fn has_unread_output(&self) -> Result<bool> {
        let buffer_state = self.buffer_state.lock().await;
        if !buffer_state.unread_output {
            return Ok(false);
        }

        let current_buffer = self.retrieve_buffer().await?;
        Ok(current_buffer != buffer_state.last_read)
    }

    /// Gets all unread output without marking it as read
    pub async fn peek_unread_output(&self) -> Result<Option<String>> {
        let buffer_state = self.buffer_state.lock().await;
        if !buffer_state.unread_output {
            return Ok(None);
        }

        let current_buffer = self.retrieve_buffer().await?;
        let unread = self.extract_new_output(&buffer_state.last_read, &current_buffer);
        Ok(if unread.is_empty() {
            None
        } else {
            Some(unread)
        })
    }

    /// Retrieve the current terminal buffer
    async fn retrieve_buffer(&self) -> Result<String> {
        // Get all output from the terminal
        let output = self.manager.read_all_output(&self.session_name).await?;
        Ok(output)
    }

    /// Wait for the buffer to stabilize (no changes for a period of time)
    async fn wait_for_stable_buffer(&self) -> Result<String> {
        let mut previous_buffer = self.retrieve_buffer().await?;
        let mut stable_count = 0;
        let required_stable_checks = 3;
        let check_interval = Duration::from_millis(100);

        while stable_count < required_stable_checks {
            sleep(check_interval).await;
            let current_buffer = self.retrieve_buffer().await?;

            if current_buffer == previous_buffer {
                stable_count += 1;
            } else {
                stable_count = 0;
                previous_buffer = current_buffer;
            }
        }

        Ok(previous_buffer)
    }

    /// Checks if a command has completed execution
    async fn is_command_complete(&self) -> Result<bool> {
        // Wait a bit to ensure the buffer has settled
        sleep(Duration::from_millis(self.debounce_ms)).await;

        let buffer1 = self.retrieve_buffer().await?;
        sleep(Duration::from_millis(self.debounce_ms)).await;
        let buffer2 = self.retrieve_buffer().await?;

        // If buffer hasn't changed, command is likely complete
        Ok(buffer1 == buffer2)
    }

    /// Extracts new output by comparing before and after buffers
    fn extract_new_output(&self, before_buffer: &str, after_buffer: &str) -> String {
        // Handle empty before buffer
        if before_buffer.is_empty() {
            return after_buffer.to_string();
        }

        // If buffers are identical, no new output
        if before_buffer == after_buffer {
            return String::new();
        }

        // If after buffer is shorter, something went wrong
        if after_buffer.len() < before_buffer.len() {
            return String::new();
        }

        // Simple case: after buffer has content appended to before buffer
        if let Some(new_output) = after_buffer.strip_prefix(before_buffer) {
            return new_output.to_string();
        }

        // More complex case: need to find where they diverge
        let before_lines: Vec<&str> = before_buffer.lines().collect();
        let after_lines: Vec<&str> = after_buffer.lines().collect();

        // Find the first line that differs
        let mut first_different_line = None;
        for i in 0..before_lines.len().min(after_lines.len()) {
            if before_lines[i] != after_lines[i] {
                first_different_line = Some(i);
                break;
            }
        }

        // If no differences found in common lines, new content is after the before buffer
        if first_different_line.is_none() {
            if after_lines.len() > before_lines.len() {
                return after_lines[before_lines.len()..].join("\n");
            }
            return String::new();
        }

        let first_different = first_different_line.unwrap();

        // Check if the different line was partially appended to
        if first_different < before_lines.len() {
            let before_line = before_lines[first_different];
            let after_line = after_lines[first_different];

            if let Some(appended_content) = after_line.strip_prefix(before_line) {
                // Line was appended to
                let remaining_lines = &after_lines[first_different + 1..];

                if !appended_content.is_empty() || !remaining_lines.is_empty() {
                    let mut result = appended_content.to_string();
                    if !remaining_lines.is_empty() {
                        if !result.is_empty() {
                            result.push('\n');
                        }
                        result.push_str(&remaining_lines.join("\n"));
                    }
                    return result;
                }
            }
        }

        // Return everything from the first different line onward
        after_lines[first_different..].join("\n")
    }
}

/// Result of executing a command
#[derive(Debug)]
pub struct ExecuteResult {
    pub output: String,
    pub full_buffer: Option<String>,
    pub has_unread_output: bool,
    pub unread_output: Option<String>,
}

/// Result of reading new output
#[derive(Debug)]
pub struct ReadResult {
    pub output: String,
    pub is_complete: bool,
}
