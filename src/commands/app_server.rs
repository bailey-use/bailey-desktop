use crate::cli::AppServerArgs;
use crate::errors::ExitCode;
use crate::services::{ModelsCache, SessionStore};

pub struct AppServerCommand;

impl AppServerCommand {
    pub fn print_help() {
        println!("aivo app-server --stdio");
        println!();
        println!("Run the versioned bidirectional AgentEngine protocol over stdin/stdout.");
        println!();
        println!("Options:");
        println!("  --stdio  Use newline-delimited JSON-RPC 2.0 over stdin/stdout");
        println!("  -h, --help  Display help information");
    }

    pub async fn execute(args: AppServerArgs, store: SessionStore, cache: ModelsCache) -> ExitCode {
        if !args.stdio {
            eprintln!("Error: app-server currently requires --stdio");
            return ExitCode::UserError;
        }
        if let Err(error) = crate::app_server::ensure_default_bailey_provider(&store).await {
            eprintln!("bailey app-server: could not prepare the default model provider: {error}");
        }
        crate::app_server::run_stdio(store, cache).await
    }
}
