use clap::{Parser, Subcommand};

use messgr::config::Config;
use messgr::db;
use messgr::keystore::VaultKeyStore;
use messgr::producer::register::{disable_producer, list_producers, register_producer};
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
        /// Operator identity recorded on the platform_audit row. No auth
        /// realm exists yet for messgr-control, so this is supplied
        /// explicitly rather than inferred.
        #[arg(long)]
        actor: String,
    },
    /// Register, disable, or list producers (upstream systems allowed to
    /// submit messages) for a tenant (DESIGN.md §4.9). No mTLS resolution or
    /// send-path consumption of this identity yet (T-007).
    Producer {
        #[command(subcommand)]
        command: ProducerCommand,
    },
}

#[derive(Subcommand)]
enum ProducerCommand {
    /// Register a producer against a tenant. Writes both the tenant
    /// `producer` row and the control `producer_cert` mapping. Safe to
    /// re-run with identical inputs; rejected on conflicting ones.
    Register {
        #[arg(long = "tenant-slug")]
        tenant_slug: String,
        #[arg(long)]
        name: String,
        #[arg(long = "cert-subject")]
        cert_subject: String,
        #[arg(long = "owner-team")]
        owner_team: String,
        #[arg(long)]
        contact: String,
        /// Operator identity recorded on the platform_audit row.
        #[arg(long)]
        actor: String,
    },
    /// Disable a producer. Never deletes the control `producer_cert`
    /// mapping — re-register to reverse it.
    Disable {
        #[arg(long = "tenant-slug")]
        tenant_slug: String,
        #[arg(long)]
        name: String,
        /// Operator identity recorded on the platform_audit row.
        #[arg(long)]
        actor: String,
    },
    /// List producers registered for a tenant.
    List {
        #[arg(long = "tenant-slug")]
        tenant_slug: String,
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
            actor,
        } => {
            // Connected only here, not unconditionally in `main` — `Migrate`
            // has no Vault dependency and must not gain one (§13: never
            // couple a subcommand to a service it doesn't use).
            let vault_keystore = VaultKeyStore::connect(config.profile).expect(
                "failed to connect to Vault (has VAULT_ADDR/VAULT_TOKEN been set?)",
            );
            let outcome = provision_tenant(
                &control_pool,
                &config.control_database_url,
                &slug,
                &region,
                &database_name,
                config.profile,
                &actor,
                vault_keystore.client(),
            )
            .await
            .unwrap_or_else(|err| {
                panic!(
                    "failed to provision tenant {slug:?} (has `messgr-control migrate` been \
                     run against the control database, and is Vault reachable and unsealed?): {err}"
                )
            });

            println!("{}", outcome.tenant_id);
            println!("vault_role_id={}", outcome.vault_role_id);
            if let Some(token) = outcome.vault_wrapped_secret_id {
                println!(
                    "vault_wrapped_secret_id={token}  # single-use, 10m TTL — unwrap once at the \
                     tenant's dispatcher deployment (`vault unwrap`); do not store this line anywhere"
                );
            }
        }
        // No Vault client is connected for producer operations — they touch
        // neither Transit nor AppRole, the same reason `Migrate` does not.
        Command::Producer { command } => match command {
            ProducerCommand::Register {
                tenant_slug,
                name,
                cert_subject,
                owner_team,
                contact,
                actor,
            } => {
                let outcome = register_producer(
                    &control_pool,
                    &config.control_database_url,
                    &tenant_slug,
                    &name,
                    &cert_subject,
                    &owner_team,
                    &contact,
                    config.profile,
                    &actor,
                )
                .await
                .unwrap_or_else(|err| {
                    panic!("failed to register producer {name:?} for tenant {tenant_slug:?}: {err}")
                });

                println!("{}", outcome.producer_id);
                println!("outcome={}", outcome.outcome);
            }
            ProducerCommand::Disable {
                tenant_slug,
                name,
                actor,
            } => {
                let outcome = disable_producer(
                    &control_pool,
                    &config.control_database_url,
                    &tenant_slug,
                    &name,
                    config.profile,
                    &actor,
                )
                .await
                .unwrap_or_else(|err| {
                    panic!("failed to disable producer {name:?} for tenant {tenant_slug:?}: {err}")
                });

                println!("outcome={}", outcome.outcome);
            }
            ProducerCommand::List { tenant_slug } => {
                let producers = list_producers(
                    &control_pool,
                    &config.control_database_url,
                    &tenant_slug,
                    config.profile,
                )
                .await
                .unwrap_or_else(|err| {
                    panic!("failed to list producers for tenant {tenant_slug:?}: {err}")
                });

                for producer in producers {
                    println!(
                        "{} name={} cert_subject={} owner_team={} contact={} enabled={}",
                        producer.id,
                        producer.name,
                        producer.cert_subject,
                        producer.owner_team,
                        producer.contact,
                        producer.enabled,
                    );
                }
            }
        },
    }
}
