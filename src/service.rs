use std::{collections::HashMap, sync::Arc};
use tokio::sync::Mutex;

use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::*,
    schemars,
    service::RequestContext,
    tool, tool_handler, tool_router, ErrorData as McpError, RoleServer, ServerHandler,
};
use serde_json::json;
use tracing::debug;

use crate::{
    buffered_executor::BufferedCommandExecutor, error::TerminalError, terminal::TerminalManager,
};

/// Global state for managing sessions and their buffered executors
struct SessionManager {
    terminal_manager: Arc<TerminalManager>,
    executors: Arc<Mutex<HashMap<String, Arc<BufferedCommandExecutor>>>>,
}

impl SessionManager {
    fn new(terminal_manager: Arc<TerminalManager>) -> Self {
        Self {
            terminal_manager,
            executors: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    async fn get_executor(
        &self,
        session_name: &str,
    ) -> Result<Arc<BufferedCommandExecutor>, TerminalError> {
        let executors = self.executors.lock().await;

        if let Some(executor) = executors.get(session_name) {
            Ok(executor.clone())
        } else {
            Err(TerminalError::SessionNotFound(session_name.to_string()))
        }
    }

    async fn create_session(
        &self,
        session_name: &str,
        command: &str,
    ) -> Result<Arc<BufferedCommandExecutor>, TerminalError> {
        // Check if session already exists
        {
            let executors = self.executors.lock().await;
            if executors.contains_key(session_name) {
                return Err(TerminalError::SessionExists(session_name.to_string()));
            }
        }

        // Create the terminal session
        self.terminal_manager
            .create_session(session_name, command)
            .await?;

        // Create buffered executor for this session
        let executor = Arc::new(BufferedCommandExecutor::new(
            self.terminal_manager.clone(),
            session_name.to_string(),
        ));

        // Store the executor
        let mut executors = self.executors.lock().await;
        executors.insert(session_name.to_string(), executor.clone());

        Ok(executor)
    }

    async fn destroy_session(&self, session_name: &str) -> Result<(), TerminalError> {
        // Remove executor
        let mut executors = self.executors.lock().await;
        executors.remove(session_name);

        // Destroy terminal session
        self.terminal_manager.destroy_session(session_name).await
    }

    async fn list_sessions(&self) -> Vec<(String, String, bool)> {
        self.terminal_manager.list_sessions().await
    }
}

/// Request types

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct CreateSessionRequest {
    #[schemars(description = "Name for the session (e.g., 'ssh-prod', 'database', 'test-server')")]
    pub session_name: String,
    #[schemars(
        description = "Command to run in the session (e.g., 'bash', 'python', 'ssh user@host')"
    )]
    pub command: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ListSessionsRequest {}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct DestroySessionRequest {
    #[schemars(description = "Name of the session to destroy")]
    pub session_name: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ExecuteCommandRequest {
    #[schemars(description = "The command to execute in the terminal")]
    pub command: String,
    #[schemars(description = "Name of the session to execute the command in")]
    pub session_name: String,
    #[schemars(
        description = "If true, returns the full terminal buffer. If false (default), returns only the command output."
    )]
    #[serde(default)]
    pub return_full_output: bool,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct SendControlCharacterRequest {
    #[schemars(
        description = "The letter corresponding to the control character (e.g., 'C' for Control-C, ']' for telnet escape)"
    )]
    pub letter: String,
    #[schemars(description = "Name of the session to send the control character to")]
    pub session_name: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ExecuteCommandAsyncRequest {
    #[schemars(description = "The command to execute asynchronously")]
    pub command: String,
    #[schemars(description = "Name of the session to execute the command in")]
    pub session_name: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ReadStreamingOutputRequest {
    #[schemars(description = "Name of the session to read output from")]
    pub session_name: String,
}

/// MCP Terminal Service with session management
#[derive(Clone)]
pub struct TerminalService {
    session_manager: Arc<SessionManager>,
    tool_router: ToolRouter<Self>,
}

impl Default for TerminalService {
    fn default() -> Self {
        Self::new()
    }
}

impl TerminalService {
    pub fn new() -> Self {
        let terminal_manager = Arc::new(TerminalManager::new());
        let session_manager = Arc::new(SessionManager::new(terminal_manager));

        let service = Self {
            session_manager,
            tool_router: Self::tool_router(),
        };

        tracing::info!("TerminalService initialized with 7 tools");

        service
    }
}

#[tool_router]
impl TerminalService {
    #[tool(
        description = "Creates a new terminal session with a specific command (e.g., 'bash', 'ssh user@host', 'python')"
    )]
    async fn create_session(
        &self,
        Parameters(CreateSessionRequest {
            session_name,
            command,
        }): Parameters<CreateSessionRequest>,
    ) -> Result<CallToolResult, McpError> {
        tracing::info!(
            "Creating session '{}' with command: {}",
            session_name,
            command
        );

        match self
            .session_manager
            .create_session(&session_name, &command)
            .await
        {
            Ok(_) => Ok(CallToolResult::success(vec![Content::text(format!(
                "Created session '{}' with command: {}",
                session_name, command
            ))])),
            Err(e) => Err(McpError::internal_error(
                "Failed to create session",
                Some(json!({
                    "session_name": session_name,
                    "command": command,
                    "error": e.to_string()
                })),
            )),
        }
    }

    #[tool(description = "Lists all active terminal sessions")]
    async fn list_sessions(
        &self,
        Parameters(ListSessionsRequest {}): Parameters<ListSessionsRequest>,
    ) -> Result<CallToolResult, McpError> {
        let sessions = self.session_manager.list_sessions().await;

        let session_info: Vec<serde_json::Value> = sessions
            .into_iter()
            .map(|(name, id, is_alive)| {
                json!({
                    "name": name,
                    "id": id,
                    "is_alive": is_alive
                })
            })
            .collect();

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&session_info).unwrap_or_else(|_| "[]".to_string()),
        )]))
    }

    #[tool(description = "Destroys a terminal session, terminating its process")]
    async fn destroy_session(
        &self,
        Parameters(DestroySessionRequest { session_name }): Parameters<DestroySessionRequest>,
    ) -> Result<CallToolResult, McpError> {
        match self.session_manager.destroy_session(&session_name).await {
            Ok(_) => Ok(CallToolResult::success(vec![Content::text(format!(
                "Destroyed session '{}'",
                session_name
            ))])),
            Err(e) => Err(McpError::internal_error(
                "Failed to destroy session",
                Some(json!({
                    "session_name": session_name,
                    "error": e.to_string()
                })),
            )),
        }
    }

    #[tool(
        description = "Executes a command in the terminal and returns the output immediately. This combines write and read operations for better performance."
    )]
    async fn execute_command(
        &self,
        Parameters(ExecuteCommandRequest {
            command,
            session_name,
            return_full_output,
        }): Parameters<ExecuteCommandRequest>,
    ) -> Result<CallToolResult, McpError> {
        tracing::info!(
            "execute_command called with: {} in session: {}",
            command,
            session_name
        );

        let executor = self
            .session_manager
            .get_executor(&session_name)
            .await
            .map_err(|e| {
                McpError::internal_error(
                    "Failed to get session",
                    Some(json!({"error": e.to_string()})),
                )
            })?;

        debug!(
            "Executing command '{}' in session '{}'",
            command, session_name
        );

        match executor.execute_command(&command, false).await {
            Ok(result) => {
                let output = if return_full_output {
                    result.full_buffer.unwrap_or(result.output)
                } else {
                    result.output
                };

                Ok(CallToolResult::success(vec![Content::text(output)]))
            }
            Err(TerminalError::UnreadOutput(msg)) => {
                // Return unread output with the error message
                Ok(CallToolResult::success(vec![Content::text(msg)]))
            }
            Err(e) => Err(McpError::internal_error(
                "Failed to execute command",
                Some(json!({
                    "command": command,
                    "error": e.to_string()
                })),
            )),
        }
    }

    #[tool(
        description = "Sends a control character to the active terminal (e.g., Control-C, or special sequences like ']' for telnet escape)"
    )]
    async fn send_control_character(
        &self,
        Parameters(SendControlCharacterRequest {
            letter,
            session_name,
        }): Parameters<SendControlCharacterRequest>,
    ) -> Result<CallToolResult, McpError> {
        // Verify session exists
        self.session_manager
            .get_executor(&session_name)
            .await
            .map_err(|e| {
                McpError::internal_error(
                    "Failed to get session",
                    Some(json!({"error": e.to_string()})),
                )
            })?;

        // Validate letter and pass it to send_control (which handles conversion)
        let control_char = if letter.to_uppercase() == "]" {
            ']'
        } else if letter.to_uppercase() == "ESCAPE" || letter.to_uppercase() == "ESC" {
            '\x1b'
        } else {
            let letter_lower = letter.to_lowercase();
            if letter_lower.len() != 1 {
                return Err(McpError::invalid_params(
                    "Invalid control character letter",
                    Some(json!({"letter": letter})),
                ));
            }

            let c = letter_lower.chars().next().unwrap();
            if !c.is_ascii_lowercase() {
                return Err(McpError::invalid_params(
                    "Invalid control character letter",
                    Some(json!({"letter": letter})),
                ));
            }

            c
        };

        match self
            .session_manager
            .terminal_manager
            .send_control(&session_name, control_char)
            .await
        {
            Ok(_) => Ok(CallToolResult::success(vec![Content::text(format!(
                "Sent control character: Control-{}",
                letter.to_uppercase()
            ))])),
            Err(e) => Err(McpError::internal_error(
                "Failed to send control character",
                Some(json!({
                    "letter": letter,
                    "error": e.to_string()
                })),
            )),
        }
    }

    #[tool(
        description = "Starts executing a command without waiting for completion. Use read_streaming_output to get the output."
    )]
    async fn execute_command_async(
        &self,
        Parameters(ExecuteCommandAsyncRequest {
            command,
            session_name,
        }): Parameters<ExecuteCommandAsyncRequest>,
    ) -> Result<CallToolResult, McpError> {
        let executor = self
            .session_manager
            .get_executor(&session_name)
            .await
            .map_err(|e| {
                McpError::internal_error(
                    "Failed to get session",
                    Some(json!({"error": e.to_string()})),
                )
            })?;

        debug!(
            "Executing async command '{}' in session '{}'",
            command, session_name
        );

        match executor.execute_command_async(&command).await {
            Ok(_) => Ok(CallToolResult::success(vec![Content::text(
                "Command execution started. Use read_streaming_output to get the output.",
            )])),
            Err(e) => Err(McpError::internal_error(
                "Failed to start async command",
                Some(json!({
                    "command": command,
                    "error": e.to_string()
                })),
            )),
        }
    }

    #[tool(
        description = "Reads any new output from an async command execution. Returns the new output and whether the command is complete."
    )]
    async fn read_streaming_output(
        &self,
        Parameters(ReadStreamingOutputRequest { session_name }): Parameters<
            ReadStreamingOutputRequest,
        >,
    ) -> Result<CallToolResult, McpError> {
        let executor = self
            .session_manager
            .get_executor(&session_name)
            .await
            .map_err(|e| {
                McpError::internal_error(
                    "Failed to get session",
                    Some(json!({"error": e.to_string()})),
                )
            })?;

        debug!("Reading streaming output from session '{}'", session_name);

        match executor.read_new_output().await {
            Ok(result) => {
                let response = json!({
                    "output": result.output,
                    "isComplete": result.is_complete
                });

                Ok(CallToolResult::success(vec![Content::text(
                    serde_json::to_string_pretty(&response)
                        .unwrap_or_else(|_| "Error formatting response".to_string()),
                )]))
            }
            Err(e) => Err(McpError::internal_error(
                "Failed to read streaming output",
                Some(json!({
                    "error": e.to_string()
                })),
            )),
        }
    }
}

#[tool_handler]
impl ServerHandler for TerminalService {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            protocol_version: ProtocolVersion::V_2024_11_05,
            capabilities: ServerCapabilities::builder()
                .enable_tools()
                .build(),
            server_info: Implementation {
                name: "terminal-mcp".to_string(),
                version: "0.1.0".to_string(),
                title: None,
                icons: None,
                website_url: None,
            },
            instructions: Some("This server provides terminal session management. Create sessions with specific commands (bash, ssh, python, etc.), execute commands, read output, and send control characters. All operations require a session_name parameter.".to_string()),
        }
    }

    async fn initialize(
        &self,
        _request: InitializeRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<InitializeResult, McpError> {
        tracing::info!("Terminal MCP server initialized with 7 tools");
        Ok(self.get_info())
    }
}
