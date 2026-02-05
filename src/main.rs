use anyhow::Result;
use rmcp::{transport::stdio, ServiceExt};
use terminal_mcp::TerminalService;
use tracing_subscriber::{self, EnvFilter};

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging to stderr to avoid interfering with stdout JSON-RPC
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive(tracing::Level::INFO.into()))
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    tracing::info!("Starting terminal-mcp server with session management");
    tracing::info!("Available tools: create_session, list_sessions, destroy_session, execute_command, execute_command_async, read_streaming_output, send_control_character");

    // Create the terminal service
    let service = TerminalService::new();

    // Serve using stdio transport
    let serving = service.serve(stdio()).await.inspect_err(|e| {
        tracing::error!("Failed to start service: {:?}", e);
    })?;

    // Wait for the service to complete
    serving.waiting().await?;

    tracing::info!("Terminal MCP server shutting down");
    Ok(())
}
