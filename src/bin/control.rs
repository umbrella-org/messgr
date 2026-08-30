use std::path::PathBuf;

use clap::{Parser, Subcommand};

use messgr::config::Config;
use messgr::db;
use messgr::keystore::VaultKeyStore;
use messgr::producer::dev_pki;
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
    /// submit messages) for a tenant (DESIGN.md §4.9). mTLS resolution
    /// (`messgr::producer::resolve`) is the first reader of what this
    /// writes; no send-path binary consumes it yet.
    Producer {
        #[command(subcommand)]
        command: ProducerCommand,
    },
    /// Dev-only internal PKI for issuing test client certificates against
    /// mTLS resolution (DESIGN.md §4.9, §11.1, T-006). Refuses to run
    /// outside `profile = dev`.
    DevPki {
        #[command(subcommand)]
        command: DevPkiCommand,
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

#[derive(Subcommand)]
enum DevPkiCommand {
    /// Idempotently ensures the dev `pki` mount, a root CA, and the
    /// permissive `producer-dev` role all exist. Safe to re-run.
    Bootstrap,
    /// Issues a leaf certificate for `--common-name` from the dev PKI,
    /// writing `cert.pem`, `key.pem`, and `ca.pem` into `--out-dir`.
    IssueCert {
        #[arg(long = "common-name")]
        common_name: String,
        #[arg(long = "out-dir")]
        out_dir: PathBuf,
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
        Command::DevPki { command } => {
            let vault_keystore = VaultKeyStore::connect(config.profile).expect(
                "failed to connect to Vault (has VAULT_ADDR/VAULT_TOKEN been set?)",
            );

            match command {
                DevPkiCommand::Bootstrap => {
                    dev_pki::bootstrap(vault_keystore.client(), config.profile)
                        .await
                        .expect("failed to bootstrap dev PKI");
                    println!("dev PKI bootstrapped: mount=pki role=producer-dev");
                }
                DevPkiCommand::IssueCert {
                    common_name,
                    out_dir,
                } => {
                    let cert = dev_pki::issue_cert(vault_keystore.client(), config.profile, &common_name)
                        .await
                        .unwrap_or_else(|err| {
                            panic!(
                                "failed to issue a dev certificate for {common_name:?} (has \
                                 `messgr-control dev-pki bootstrap` been run?): {err}"
                            )
                        });

                    std::fs::create_dir_all(&out_dir).unwrap_or_else(|err| {
                        panic!("failed to create {out_dir:?}: {err}")
                    });
                    std::fs::write(out_dir.join("cert.pem"), &cert.certificate)
                        .unwrap_or_else(|err| {
                            panic!("failed to write cert.pem: {err}")
                        });
                    std::fs::write(out_dir.join("key.pem"), &cert.private_key)
                        .unwrap_or_else(|err| panic!("failed to write key.pem: {err}"));
                    std::fs::write(out_dir.join("ca.pem"), &cert.issuing_ca)
                        .unwrap_or_else(|err| panic!("failed to write ca.pem: {err}"));

                    println!(
                        "wrote cert.pem, key.pem, ca.pem to {}",
                        out_dir.display()
                    );
                }
            }
        }
    }
}
