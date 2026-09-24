use clap::{Parser, Subcommand};
use opencontext::{api, db, mcp, models::Models, parsing, service::Service, worker};
use std::path::PathBuf;
use uuid::Uuid;

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[arg(long, env = "DATABASE_URL", hide_env_values = true, global = true)]
    database_url: Option<String>,
    #[arg(
        long,
        env = "OC_FILES_DIR",
        default_value = ".data/files",
        global = true
    )]
    files_dir: PathBuf,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// Configure the least-privileged runtime role using OC_RUNTIME_PASSWORD.
    RuntimeSetup,
    /// Provision a workspace and print its one-time admin API token (administrator connection).
    WorkspaceCreate {
        name: String,
    },
    /// Issue another workspace key (administrator connection).
    KeyCreate {
        workspace_id: Uuid,
        #[arg(long, default_value = "reader")]
        role: String,
    },
    /// Revoke a workspace key (administrator connection).
    KeyRevoke {
        key_id: Uuid,
    },
    Api {
        #[arg(long, env = "OC_BIND", default_value = "127.0.0.1:8080")]
        bind: String,
    },
    Worker,
    Mcp,
    #[command(hide = true)]
    ParsePdf {
        path: PathBuf,
    },
}
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "opencontext=info,sqlx=warn".into()),
        )
        .init();
    let cli = Cli::parse();
    if let Command::ParsePdf { path } = &cli.command {
        println!("{}", serde_json::to_string(&parsing::pdf_child(path)?)?);
        return Ok(());
    }
    let url = cli
        .database_url
        .ok_or_else(|| anyhow::anyhow!("DATABASE_URL is required"))?;
    let pool = db::connect(&url).await?;
    match cli.command {
        Command::RuntimeSetup => {
            db::configure_runtime(&pool, &std::env::var("OC_RUNTIME_PASSWORD")?).await?;
            println!("runtime role configured");
        }
        Command::WorkspaceCreate { name } => println!("{}", db::provision(&pool, &name).await?),
        Command::KeyCreate { workspace_id, role } => {
            println!("{}", db::issue_key(&pool, workspace_id, &role).await?)
        }
        Command::KeyRevoke { key_id } => {
            sqlx::query("UPDATE oc.api_keys SET revoked=true WHERE id=$1")
                .bind(key_id)
                .execute(&pool)
                .await?;
            println!("key revoked");
        }
        command => {
            db::check_runtime(&pool).await?;
            let service = Service::new(pool, cli.files_dir, Models::from_env()?);
            match command {
                Command::Api { bind } => api::serve(service, &bind).await?,
                Command::Worker => worker::run(service).await?,
                Command::Mcp => mcp::run(service, std::env::var("OC_API_KEY")?).await?,
                _ => unreachable!(),
            }
        }
    }
    Ok(())
}
