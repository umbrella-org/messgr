use clap::{Parser, Subcommand};

use messgr::config::Config;
use messgr::db;
use messgr::tenant::provision::provision_tenant;

#[derive(Parser)]
#[command(name = "messgr-control", about = "messgr control-plane operations")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Apply pending control-database migrations. Never run automatically by
    /// any other subcommand (DESIGN.md §13: "never automatic on process
    /// start") — an operator invokes this deliberately.
    Migrate,
    /// Provision a tenant: create its database (if it doesn't already
    /// exist), run tenant migrations, and register it in the control
    /// database. Safe to re-run for the same `--slug`.
    Provision {
        #[arg(long)]
        slug: String,
        #[arg(long)]
        region: String,
        #[arg(long = "database-name")]
        database_name: String,
    },
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let config = Config::from_env();
    let cli = Cli::parse();

    let control_pool = db::connect(
        &config.control_database_url,
        config.database_max_connections,
    )
    .await
    .expect("failed to connect to control database");

    match cli.command {
        Command::Migrate => {
            sqlx::migrate!("./migrations/control")
                .run(&control_pool)
                .await
                .expect("failed to run control database migrations");
            tracing::info!("control database migrations applied");
        }
        Command::Provision {
            slug,
            region,
            database_name,
        } => {
            let tenant_id = provision_tenant(
                &control_pool,
                &config.control_database_url,
                &slug,
                &region,
                &database_name,
                config.profile,
            )
            .await
            .unwrap_or_else(|err| {
                panic!(
                    "failed to provision tenant {slug:?} (has `messgr-control migrate` been \
                     run against the control database?): {err}"
                )
            });

            println!("{tenant_id}");
        }
    }
}
