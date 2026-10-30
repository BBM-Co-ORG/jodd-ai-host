//! jodd-ai-mcp: the MCP server half of jodd-ai-host. Skeleton only; later tasks add the protocol.

use std::process::ExitCode;

fn main() -> ExitCode {
    match std::env::args().nth(1).as_deref() {
        Some("--version") => {
            println!("jodd-ai-mcp {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!("unknown request");
            ExitCode::from(64)
        }
    }
}
