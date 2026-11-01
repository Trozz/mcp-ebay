use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    mcp_ebay::server().main().await
}
