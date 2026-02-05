use crate::error::{Result, TerminalError};
use pty_process::{Command as PtyCommand, Pty};
use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Child;
use tokio::sync::{Mutex, RwLock};
use tokio::time::{sleep, Duration, Instant};
use tracing::{debug, error, info};

/// Terminal state information
#[derive(Debug, Clone)]
pub struct TerminalState {
    pub state: SessionState,
    pub last_prompt: Option<String>,
    pub pending_input_buffer: Vec<String>,
    pub output_rate: f64, // chars/sec
    pub time_since_last_output_ms: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SessionState {
    Busy,
    Ready,
    WaitingForInput,
}

/// A simplified terminal session using pty-process
pub struct Terminal {
    id: String,
    name: String,
    child: Arc<Mutex<Child>>,
    pty: Arc<Mutex<Pty>>,
    output_buffer: Arc<Mutex<Vec<u8>>>,
    // New fields for enhanced features
    last_output_time: Arc<Mutex<Instant>>,
    output_rate_tracker: Arc<Mutex<(usize, Instant)>>, // (bytes_count, start_time)
    detected_prompts: Arc<Mutex<Vec<String>>>,
    input_queue: Arc<Mutex<VecDeque<String>>>,
    debug_mode: Arc<Mutex<bool>>,
}

impl Terminal {
    /// Create a new terminal with name and command
    pub fn new(name: &str, command: &str) -> Result<Self> {
        // Parse command into program and arguments
        let parts: Vec<&str> = command.split_whitespace().collect();
        if parts.is_empty() {
            return Err(TerminalError::InvalidCommand("Empty command".to_string()));
        }

        let mut cmd = PtyCommand::new(parts[0]);
        for arg in &parts[1..] {
            cmd = cmd.arg(arg);
        }

        // Set environment variables for clean, predictable output
        cmd = cmd.env("TERM", "dumb"); // No ANSI escapes, no cursor moves, no color
        cmd = cmd.env("NO_COLOR", "1"); // Disable color output (de-facto standard)
        cmd = cmd.env("CLICOLOR", "0"); // Disable color for tools that don't respect NO_COLOR
        cmd = cmd.env("FORCE_COLOR", "0"); // Ensure color is disabled
        cmd = cmd.env("PAGER", "cat"); // Disable paging (no less/more)
        cmd = cmd.env("GIT_PAGER", "cat"); // Disable git paging
        cmd = cmd.env("GH_PAGER", "cat"); // Disable GitHub CLI paging
        cmd = cmd.env("LANG", "C.UTF-8"); // Consistent, predictable byte-handling
        cmd = cmd.env("LC_ALL", "C.UTF-8"); // Lock down locale quirks

        // Create PTY and spawn process
        let (pty, pts) = pty_process::open().map_err(|e| TerminalError::Pty(e.to_string()))?;

        let child = cmd
            .spawn(pts)
            .map_err(|e| TerminalError::Pty(e.to_string()))?;

        let id = uuid::Uuid::new_v4().to_string();
        let pid = child.id().map_or("unknown".to_string(), |p| p.to_string());
        info!("Created terminal '{}' (id: {}) with PID {}", name, id, pid);

        let terminal = Terminal {
            id: id.clone(),
            name: name.to_string(),
            child: Arc::new(Mutex::new(child)),
            pty: Arc::new(Mutex::new(pty)),
            output_buffer: Arc::new(Mutex::new(Vec::new())),
            last_output_time: Arc::new(Mutex::new(Instant::now())),
            output_rate_tracker: Arc::new(Mutex::new((0, Instant::now()))),
            detected_prompts: Arc::new(Mutex::new(Vec::new())),
            input_queue: Arc::new(Mutex::new(VecDeque::new())),
            debug_mode: Arc::new(Mutex::new(false)),
        };

        // Spawn a background task to continuously read from PTY
        terminal.spawn_read_task();

        Ok(terminal)
    }

    /// Get the terminal ID
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Get the terminal name
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Spawn a background task to continuously read from PTY
    fn spawn_read_task(&self) {
        let pty = self.pty.clone();
        let output_buffer = self.output_buffer.clone();
        let last_output_time = self.last_output_time.clone();
        let id = self.id.clone();

        tokio::spawn(async move {
            let mut buf = vec![0; 4096];
            loop {
                let mut pty_guard = pty.lock().await;
                match tokio::time::timeout(Duration::from_millis(100), pty_guard.read(&mut buf))
                    .await
                {
                    Ok(Ok(n)) if n > 0 => {
                        let mut buffer = output_buffer.lock().await;
                        buffer.extend_from_slice(&buf[..n]);
                        *last_output_time.lock().await = Instant::now();
                        debug!("Read {} bytes from PTY for terminal {}", n, id);
                    }
                    Ok(Ok(0)) => {
                        debug!("PTY closed for terminal {}", id);
                        break;
                    }
                    Ok(Ok(_)) => {
                        // Should not happen, but handle it
                        continue;
                    }
                    Ok(Err(e)) => {
                        debug!("PTY read error for terminal {}: {}", id, e);
                        break;
                    }
                    Err(_) => {
                        // Timeout - no data available, continue
                    }
                }
                drop(pty_guard);
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        });
    }

    /// Check if the process is still alive
    pub async fn is_alive(&self) -> bool {
        let mut child = self.child.lock().await;
        child.try_wait().unwrap_or(None).is_none()
    }

    /// Check if the process is still alive
    async fn ensure_alive(&self) -> Result<()> {
        if !self.is_alive().await {
            return Err(TerminalError::PtyNotAvailable);
        }
        Ok(())
    }

    /// Write a command to the terminal
    pub async fn write_command(&self, command: &str) -> Result<()> {
        self.ensure_alive().await?;

        let mut data = command.as_bytes().to_vec();
        data.push(b'\n');

        let mut pty = self.pty.lock().await;
        let (_, mut writer) = pty.split();
        writer.write_all(&data).await?;
        writer.flush().await?;

        debug!("Sent command to terminal {}: {}", self.id, command);
        Ok(())
    }

    /// Send raw bytes to the terminal
    pub async fn write_raw(&self, data: &[u8]) -> Result<()> {
        debug!(
            "write_raw: Writing {} bytes to terminal {}",
            data.len(),
            self.id
        );

        let mut pty = self.pty.lock().await;
        let (_, mut writer) = pty.split();
        writer.write_all(data).await?;
        writer.flush().await?;

        debug!("write_raw: Data written to terminal {}", self.id);
        Ok(())
    }

    /// Write text with automatic newline appended
    pub async fn write_line(&self, text: &str) -> Result<()> {
        debug!(
            "write_line: Writing line '{}' to terminal {}",
            text, self.id
        );
        let mut data = text.as_bytes().to_vec();
        data.push(b'\n');
        self.write_raw(&data).await
    }

    /// Send a control character
    pub async fn send_control(&self, c: char) -> Result<()> {
        let lower = c.to_ascii_lowercase();
        let byte = match lower {
            'a'..='z' => lower as u8 - b'a' + 1,
            '[' => 0x1b, // ESC
            '\\' => 0x1c,
            ']' => 0x1d,
            '^' => 0x1e,
            '_' => 0x1f,
            '?' => 0x7f, // DEL
            _ => return Err(TerminalError::InvalidControlChar(c)),
        };

        self.write_raw(&[byte]).await?;
        debug!(
            "Sent control character Ctrl-{} to terminal {}",
            c.to_uppercase(),
            self.id
        );
        Ok(())
    }

    /// Read available output without blocking
    async fn read_available(&self) -> Result<Vec<u8>> {
        let mut buffer = vec![0u8; 4096];
        let mut output = Vec::new();

        debug!(
            "read_available: Starting read operation for terminal {}",
            self.id
        );

        let mut pty = self.pty.lock().await;
        let (mut reader, _) = pty.split();

        // Try to read data without blocking
        loop {
            match tokio::time::timeout(Duration::from_millis(10), reader.read(&mut buffer)).await {
                Ok(Ok(0)) => {
                    debug!("read_available: EOF on PTY read for terminal {}", self.id);
                    break;
                }
                Ok(Ok(n)) => {
                    output.extend_from_slice(&buffer[..n]);
                    debug!(
                        "read_available: Read {} bytes from PTY for terminal {}",
                        n, self.id
                    );
                    // Continue reading if we got a full buffer
                    if n == buffer.len() {
                        continue;
                    }
                    break;
                }
                Ok(Err(e)) => {
                    debug!(
                        "read_available: PTY read error for terminal {}: {}",
                        self.id, e
                    );
                    return Err(e.into());
                }
                Err(_) => {
                    // Timeout - no more data available
                    debug!(
                        "read_available: No more data available for terminal {}",
                        self.id
                    );
                    break;
                }
            }
        }

        // Append to buffer and update tracking
        if !output.is_empty() {
            let mut buf_guard = self.output_buffer.lock().await;
            buf_guard.extend(&output);
            debug!("read_available: Appended {} bytes to buffer, total buffer size: {} for terminal {}", 
                   output.len(), buf_guard.len(), self.id);

            // Update last output time
            *self.last_output_time.lock().await = Instant::now();

            // Update output rate tracker
            let mut rate_tracker = self.output_rate_tracker.lock().await;
            rate_tracker.0 += output.len();
        }

        Ok(output)
    }

    /// Execute a command and wait for output
    pub async fn execute_command(&self, command: &str, timeout_ms: u64) -> Result<String> {
        debug!(
            "Execute command '{}' with timeout {}ms",
            command, timeout_ms
        );

        // Clear buffer before command
        self.output_buffer.lock().await.clear();

        // Send command
        if let Err(e) = self.write_command(command).await {
            error!("Failed to write command '{}': {}", command, e);
            return Err(e);
        }

        // Collect output with proper timeout
        let start = std::time::Instant::now();
        let mut all_output = Vec::new();
        let mut last_data_time = std::time::Instant::now();

        // Wait briefly for command to start processing
        sleep(Duration::from_millis(50)).await;

        while start.elapsed() < Duration::from_millis(timeout_ms) {
            match self.read_available().await {
                Ok(output) => {
                    if !output.is_empty() {
                        all_output.extend(&output);
                        last_data_time = std::time::Instant::now();
                        debug!("Got {} bytes, total: {}", output.len(), all_output.len());
                    } else if last_data_time.elapsed() > Duration::from_millis(500)
                        && !all_output.is_empty()
                    {
                        // No new data for 500ms and we have some output - command likely finished
                        debug!("Command appears complete (500ms idle)");
                        break;
                    }
                }
                Err(e) => {
                    error!("Error reading from PTY during execute_command: {}", e);
                    return Err(e);
                }
            }

            // Avoid busy-waiting
            sleep(Duration::from_millis(10)).await;
        }

        if all_output.is_empty() && start.elapsed() >= Duration::from_millis(timeout_ms) {
            return Err(TerminalError::CommandTimeout(timeout_ms));
        }

        Ok(String::from_utf8_lossy(&all_output).to_string())
    }

    /// Read all buffered output
    pub async fn read_buffer(&self) -> Vec<u8> {
        let buffer = self.output_buffer.lock().await;
        buffer.clone()
    }

    /// Clear the output buffer
    pub async fn clear_buffer(&self) {
        self.output_buffer.lock().await.clear();
    }

    /// Read streaming output (non-blocking)
    pub async fn read_streaming(&self) -> Result<String> {
        debug!("read_streaming: Starting for terminal {}", self.id);

        // Atomic read and clear of buffer (background task fills it)
        let buffered_data = {
            let mut buffer = self.output_buffer.lock().await;
            let data = buffer.clone();
            buffer.clear();
            data
        };

        let result = String::from_utf8_lossy(&buffered_data).to_string();
        debug!(
            "read_streaming: Returning {} chars for terminal {}",
            result.len(),
            self.id
        );

        Ok(result)
    }

    /// Wait for a specific prompt to appear in the output
    pub async fn wait_for_prompt(
        &self,
        prompts: &[&str],
        timeout_ms: u64,
        include_output: bool,
    ) -> Result<(Option<String>, String)> {
        debug!(
            "wait_for_prompt: Waiting for prompts {:?} with timeout {}ms",
            prompts, timeout_ms
        );

        let start = Instant::now();
        let mut collected_output = Vec::new();
        #[allow(unused_assignments)]
        let mut found_prompt = None;

        // Clear buffer before waiting
        self.clear_buffer().await;

        while start.elapsed() < Duration::from_millis(timeout_ms) {
            // Read any available output
            let output = self.read_available().await?;

            if !output.is_empty() {
                collected_output.extend(&output);
                let output_str = String::from_utf8_lossy(&collected_output);

                // Check for prompts
                for prompt in prompts {
                    if output_str.contains(prompt) {
                        found_prompt = Some(prompt.to_string());

                        // Update detected prompts
                        let mut detected = self.detected_prompts.lock().await;
                        if !detected.contains(&prompt.to_string()) {
                            detected.push(prompt.to_string());
                        }

                        debug!(
                            "wait_for_prompt: Found prompt '{}' after {}ms",
                            prompt,
                            start.elapsed().as_millis()
                        );

                        // Give it a moment to ensure no more output is coming
                        sleep(Duration::from_millis(50)).await;
                        let final_output = self.read_available().await?;
                        collected_output.extend(&final_output);

                        let full_output = if include_output {
                            String::from_utf8_lossy(&collected_output).to_string()
                        } else {
                            String::new()
                        };

                        return Ok((found_prompt, full_output));
                    }
                }
            }

            sleep(Duration::from_millis(50)).await;
        }

        // Timeout reached
        let full_output = if include_output {
            String::from_utf8_lossy(&collected_output).to_string()
        } else {
            String::new()
        };

        Ok((None, full_output))
    }

    /// Get the current state of the terminal
    pub async fn get_terminal_state(&self) -> Result<TerminalState> {
        let last_output_time = *self.last_output_time.lock().await;
        let time_since_last_output_ms = last_output_time.elapsed().as_millis() as u64;

        // Calculate output rate
        let (bytes_count, start_time) = *self.output_rate_tracker.lock().await;
        let elapsed_secs = start_time.elapsed().as_secs_f64();
        let output_rate = if elapsed_secs > 0.0 {
            bytes_count as f64 / elapsed_secs
        } else {
            0.0
        };

        // Determine state based on output activity
        let state = if time_since_last_output_ms < 100 {
            SessionState::Busy
        } else if time_since_last_output_ms > 500 {
            SessionState::Ready
        } else {
            SessionState::WaitingForInput
        };

        // Get last detected prompt
        let detected_prompts = self.detected_prompts.lock().await;
        let last_prompt = detected_prompts.last().cloned();

        // Get pending input
        let input_queue = self.input_queue.lock().await;
        let pending_input_buffer: Vec<String> = input_queue.iter().cloned().collect();

        Ok(TerminalState {
            state,
            last_prompt,
            pending_input_buffer,
            output_rate,
            time_since_last_output_ms,
        })
    }

    /// Read output until it stabilizes (no new output for stability_ms)
    pub async fn read_until_stable(
        &self,
        stability_ms: u64,
        max_timeout_ms: u64,
        return_partial_on_timeout: bool,
    ) -> Result<String> {
        debug!(
            "read_until_stable: stability_ms={}, max_timeout_ms={}",
            stability_ms, max_timeout_ms
        );

        let start = Instant::now();
        let mut collected_output = Vec::new();
        let mut last_data_time = Instant::now();

        while start.elapsed() < Duration::from_millis(max_timeout_ms) {
            let output = self.read_available().await?;

            if !output.is_empty() {
                collected_output.extend(&output);
                last_data_time = Instant::now();
                debug!(
                    "read_until_stable: Got {} bytes, total: {}",
                    output.len(),
                    collected_output.len()
                );
            } else if last_data_time.elapsed() >= Duration::from_millis(stability_ms) {
                // Output has been stable for the required duration
                debug!(
                    "read_until_stable: Output stable for {}ms, returning",
                    stability_ms
                );
                return Ok(String::from_utf8_lossy(&collected_output).to_string());
            }

            sleep(Duration::from_millis(10)).await;
        }

        // Timeout reached
        if return_partial_on_timeout || !collected_output.is_empty() {
            Ok(String::from_utf8_lossy(&collected_output).to_string())
        } else {
            Err(TerminalError::Timeout(format!(
                "read_until_stable timed out after {}ms",
                max_timeout_ms
            )))
        }
    }

    /// Clear the input queue
    pub async fn clear_input_queue(&self) -> Result<()> {
        let mut queue = self.input_queue.lock().await;
        queue.clear();
        debug!("Cleared input queue for terminal {}", self.id);
        Ok(())
    }

    /// Peek at the input queue without removing items
    pub async fn peek_input_queue(&self) -> Result<Vec<String>> {
        let queue = self.input_queue.lock().await;
        Ok(queue.iter().cloned().collect())
    }

    /// Queue a command to be sent (for input queue management)
    pub async fn queue_input(&self, input: &str) -> Result<()> {
        let mut queue = self.input_queue.lock().await;
        queue.push_back(input.to_string());
        debug!("Queued input '{}' for terminal {}", input, self.id);
        Ok(())
    }

    /// Process queued inputs
    pub async fn process_input_queue(&self) -> Result<usize> {
        let mut queue = self.input_queue.lock().await;
        let mut processed = 0;

        while let Some(input) = queue.pop_front() {
            drop(queue); // Release lock before writing
            self.write_line(&input).await?;
            processed += 1;
            queue = self.input_queue.lock().await;
        }

        Ok(processed)
    }

    /// Enable or disable debug mode
    pub async fn set_debug_mode(&self, enabled: bool) -> Result<()> {
        *self.debug_mode.lock().await = enabled;
        debug!(
            "Debug mode {} for terminal {}",
            if enabled { "enabled" } else { "disabled" },
            self.id
        );
        Ok(())
    }

    /// Read output with debug annotations if debug mode is enabled
    pub async fn read_with_debug(&self) -> Result<String> {
        let output = self.read_streaming().await?;
        let debug_enabled = *self.debug_mode.lock().await;

        if debug_enabled && !output.is_empty() {
            let mut annotated = String::new();
            let lines: Vec<&str> = output.lines().collect();

            for (i, line) in lines.iter().enumerate() {
                // Annotate control characters
                let annotated_line = line
                    .replace('\n', "[\\n]")
                    .replace('\r', "[\\r]")
                    .replace('\t', "[\\t]");

                // Check if this line contains a known prompt
                let detected_prompts = self.detected_prompts.lock().await;
                let mut _is_prompt = false;
                for prompt in detected_prompts.iter() {
                    if line.contains(prompt) {
                        annotated.push_str(&format!("[PROMPT DETECTED: {}] ", prompt));
                        _is_prompt = true;
                        break;
                    }
                }

                // Add line number and content
                annotated.push_str(&format!("[L{}] {}", i + 1, annotated_line));

                if i < lines.len() - 1 {
                    annotated.push('\n');
                }
            }

            // Add state info at the end if not empty
            if !annotated.is_empty() {
                let state = self.get_terminal_state().await?;
                annotated.push_str(&format!(
                    "\n[STATE: {:?}, OUTPUT_RATE: {:.1} chars/sec, IDLE: {}ms]",
                    state.state, state.output_rate, state.time_since_last_output_ms
                ));
            }

            Ok(annotated)
        } else {
            Ok(output)
        }
    }

    /// Wait for session to be ready with debouncing
    /// This ensures the shell has finished initialization and is ready for commands
    pub async fn wait_for_ready(&self, timeout_ms: u64) -> Result<()> {
        debug!(
            "Waiting for session to be ready with timeout {}ms",
            timeout_ms
        );

        match tokio::time::timeout(Duration::from_millis(timeout_ms), async {
            let mut last_output_time = std::time::Instant::now();
            let mut total_output = 0usize;
            let mut stable_count = 0;

            // Debounce period: if no new output for this duration, consider ready
            const DEBOUNCE_MS: u64 = 200;
            const STABLE_THRESHOLD: u32 = 3;

            loop {
                // Try to read any output
                let output = self.read_available().await?;

                if !output.is_empty() {
                    total_output += output.len();
                    last_output_time = std::time::Instant::now();
                    stable_count = 0;
                    debug!(
                        "Got {} bytes during wait_for_ready, total: {}",
                        output.len(),
                        total_output
                    );

                    // Check if we got a shell prompt (common patterns)
                    let output_str = String::from_utf8_lossy(&output);
                    if output_str.contains("$")
                        || output_str.contains("#")
                        || output_str.contains(">")
                    {
                        debug!("Detected shell prompt in output: '{}'", output_str.trim());
                        // Give it a moment to ensure no more output is coming
                        sleep(Duration::from_millis(50)).await;
                        let _ = self.read_available().await;
                        return Ok(());
                    }
                } else {
                    // Check if we've been stable (no output) for long enough
                    let elapsed = last_output_time.elapsed();
                    if elapsed >= Duration::from_millis(DEBOUNCE_MS) {
                        stable_count += 1;

                        // If we've been stable for multiple checks, assume ready
                        if stable_count >= STABLE_THRESHOLD {
                            debug!(
                                "Session ready after {} stable checks ({}ms debounce)",
                                stable_count, DEBOUNCE_MS
                            );

                            // Do one final read to clear any remaining data
                            let _ = self.read_available().await;
                            return Ok(());
                        }
                    }
                }

                sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        {
            Ok(result) => result,
            Err(_) => {
                debug!("Session ready check timed out, doing final buffer clear");
                // Clear any pending data before proceeding
                let _ = self.read_available().await;
                Ok(())
            }
        }
    }
}

/// Terminal manager handles multiple terminal sessions
pub struct TerminalManager {
    terminals: Arc<RwLock<HashMap<String, Arc<Terminal>>>>,
}

impl Default for TerminalManager {
    fn default() -> Self {
        Self::new()
    }
}

impl TerminalManager {
    pub fn new() -> Self {
        TerminalManager {
            terminals: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Create a new terminal session
    pub async fn create_session(&self, name: &str, command: &str) -> Result<String> {
        let terminal = Arc::new(Terminal::new(name, command)?);
        let id = terminal.id().to_string();

        // Wait for session to be ready (especially important for SSH)
        terminal.wait_for_ready(2000).await?;

        let mut terminals = self.terminals.write().await;
        terminals.insert(name.to_string(), terminal);

        Ok(id)
    }

    /// Get a terminal by name
    pub async fn get_terminal(&self, name: &str) -> Option<Arc<Terminal>> {
        let terminals = self.terminals.read().await;
        terminals.get(name).cloned()
    }

    /// List all terminal sessions
    pub async fn list_sessions(&self) -> Vec<(String, String, bool)> {
        let terminals = self.terminals.read().await;
        let mut sessions = Vec::new();

        for (name, terminal) in terminals.iter() {
            let is_alive = terminal.is_alive().await;
            sessions.push((name.clone(), terminal.id().to_string(), is_alive));
        }

        sessions
    }

    /// Destroy a terminal session
    pub async fn destroy_session(&self, name: &str) -> Result<()> {
        let mut terminals = self.terminals.write().await;

        if let Some(_terminal) = terminals.remove(name) {
            // The terminal will be cleaned up when dropped
            info!("Destroyed terminal session '{}'", name);
            Ok(())
        } else {
            Err(TerminalError::SessionNotFound(name.to_string()))
        }
    }

    // Convenience methods for service layer compatibility

    /// Create alias for service layer
    pub async fn create(&self, name: &str, command: &str) -> Result<String> {
        self.create_session(name, command).await
    }

    /// Send control character to a terminal
    pub async fn send_control(&self, name: &str, c: char) -> Result<()> {
        if let Some(terminal) = self.get_terminal(name).await {
            terminal.send_control(c).await
        } else {
            Err(TerminalError::SessionNotFound(name.to_string()))
        }
    }

    /// List alias for service layer
    pub async fn list(&self) -> Vec<(String, String, bool)> {
        self.list_sessions().await
    }

    /// Destroy alias for service layer
    pub async fn destroy(&self, name: &str) -> Result<()> {
        self.destroy_session(name).await
    }

    /// Read streaming output from a terminal
    pub async fn read_streaming(&self, name: &str) -> Result<String> {
        if let Some(terminal) = self.get_terminal(name).await {
            terminal.read_streaming().await
        } else {
            Err(TerminalError::SessionNotFound(name.to_string()))
        }
    }

    /// Write raw text to a terminal
    pub async fn write(&self, name: &str, text: &str) -> Result<()> {
        if let Some(terminal) = self.get_terminal(name).await {
            terminal.write_raw(text.as_bytes()).await
        } else {
            Err(TerminalError::SessionNotFound(name.to_string()))
        }
    }

    /// Write text with automatic newline to a terminal
    pub async fn write_line(&self, name: &str, text: &str) -> Result<()> {
        if let Some(terminal) = self.get_terminal(name).await {
            terminal.write_line(text).await
        } else {
            Err(TerminalError::SessionNotFound(name.to_string()))
        }
    }

    /// Read streaming output with timeout
    pub async fn read_streaming_with_timeout(&self, name: &str, timeout_ms: u64) -> Result<String> {
        debug!(
            "read_streaming_with_timeout: Called for session '{}' with timeout {}ms",
            name, timeout_ms
        );

        if let Some(terminal) = self.get_terminal(name).await {
            // Add a small initial delay to ensure any pending writes have completed
            sleep(Duration::from_millis(50)).await;

            // Try to read with timeout
            match tokio::time::timeout(Duration::from_millis(timeout_ms), terminal.read_streaming())
                .await
            {
                Ok(Ok(output)) => {
                    debug!(
                        "read_streaming_with_timeout: Got {} chars from session '{}'",
                        output.len(),
                        name
                    );
                    Ok(output)
                }
                Ok(Err(e)) => {
                    debug!(
                        "read_streaming_with_timeout: Error reading from session '{}': {}",
                        name, e
                    );
                    Err(e)
                }
                Err(_) => {
                    debug!(
                        "read_streaming_with_timeout: Timeout after {}ms for session '{}'",
                        timeout_ms, name
                    );
                    Ok(String::new()) // Timeout just returns empty string
                }
            }
        } else {
            debug!("read_streaming_with_timeout: Session '{}' not found", name);
            Err(TerminalError::SessionNotFound(name.to_string()))
        }
    }

    /// Read all output from a terminal (entire buffer)
    pub async fn read_all_output(&self, name: &str) -> Result<String> {
        if let Some(terminal) = self.get_terminal(name).await {
            let buffer = terminal.read_buffer().await;
            Ok(String::from_utf8_lossy(&buffer).to_string())
        } else {
            Err(TerminalError::SessionNotFound(name.to_string()))
        }
    }
}
