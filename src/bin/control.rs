use std::path::PathBuf;

use clap::{Parser, Subcommand};

use messgr::config::Config;
use messgr::customer_dek::lifecycle::pre_provision_for_tenant;
use messgr::db;
use messgr::keystore::VaultKeyStore;
use messgr::partition_lifecycle::lifecycle::run_for_tenant as run_partition_lifecycle;
use messgr::producer::dev_pki;
use messgr::producer::register::{disable_producer, list_producers, register_producer};
use messgr::provider_config::configure::{list_provider_config, set_provider_config};
use messgr::provider_config::model::ProviderConfigInput;
use messgr::stats::tenant_message_stats;
use messgr::template::approve::{
    approve_template, list_template_versions, render_preview, show_template,
};
use messgr::template::model::channel;
use messgr::tenant::provision::provision_tenant;
use messgr::tenant_config::configure::{set_tenant_config, show_tenant_config};
use messgr::tenant_config::model::{TenantConfigInput, verification_mode};

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
    /// Set or show a tenant's typed configuration (DESIGN.md §4.10, T-007):
    /// retention, timezone/locale defaults, schedule horizon, verification
    /// mode, staleness bound, and quota day boundary. A fresh tenant has no
    /// row until `set` is run at least once.
    TenantConfig {
        #[command(subcommand)]
        command: TenantConfigCommand,
    },
    /// Per-customer DEK operations (DESIGN.md §7.6, T-008).
    CustomerDek {
        #[command(subcommand)]
        command: CustomerDekCommand,
    },
    /// Approve, show, list, or render versioned message templates (DESIGN.md
    /// §4.4, T-010). Templates are immutable once approved — a content
    /// change is always a new `--version`. The audited, role-gated admin
    /// approval workflow (§11.1, §11.3) is `T-042`'s scope; this is the bare
    /// operator-driven surface, matching `producer`/`tenant-config`.
    Template {
        #[command(subcommand)]
        command: TemplateCommand,
    },
    /// Set or list a tenant's ordered per-channel provider list (DESIGN.md
    /// §4.10, §12.1, T-012). `credential_path` and `rate_limit_per_sec` are
    /// stored but not yet read by anything (T-012 decisions 3, 4).
    ProviderConfig {
        #[command(subcommand)]
        command: ProviderConfigCommand,
    },
    /// Keep comms_request/comms_event partitions self-managing (DESIGN.md
    /// §4.1, §7.2, §7.5, T-014): create-ahead, move to slow tablespace,
    /// detach + drop at the tenant's retention boundary. Meant to run on a
    /// schedule (cron/systemd timer) — this binary does not daemonize or
    /// loop.
    PartitionLifecycle {
        #[command(subcommand)]
        command: PartitionLifecycleCommand,
    },
    /// Message-volume counts by channel and status for one tenant (DESIGN.md
    /// §11.4: counts/metadata only, never payload content). Reads
    /// `comms_request.final_status` directly. T-028.
    Stats {
        #[arg(long = "tenant-slug")]
        tenant_slug: String,
        /// Only count requests created on or after this date (UTC). Omit for
        /// all-time.
        #[arg(long)]
        since: Option<chrono::NaiveDate>,
    },
    /// Print the binary name and version, then exit.
    Version,
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
        /// Also sets `--common-name` as a DNS SAN (`issue_server_cert`
        /// instead of `issue_cert`) — required for a certificate a TLS
        /// client will hostname-check, i.e. a server identity such as
        /// `messgr-ingest`'s own cert. Producer/client certificates never
        /// need this and should omit the flag (T-011).
        #[arg(long = "server", action = clap::ArgAction::SetTrue)]
        server: bool,
    },
}

#[derive(Subcommand)]
enum TenantConfigCommand {
    /// Create or overwrite the tenant's single `tenant_config` row. Safe to
    /// re-run: identical inputs are an idempotent no-op, different inputs
    /// overwrite the row (there is nothing to conflict with — it is a
    /// singleton).
    Set {
        #[arg(long = "tenant-slug")]
        tenant_slug: String,
        #[arg(long = "retention-years")]
        retention_years: i32,
        #[arg(long = "default-timezone")]
        default_timezone: String,
        #[arg(long = "default-locale")]
        default_locale: String,
        /// Defaults to 90 (DESIGN.md §4.10's own SQL default) when omitted.
        #[arg(long = "schedule-horizon-days")]
        schedule_horizon_days: Option<i32>,
        #[arg(long = "quota-day-boundary-tz")]
        quota_day_boundary_tz: String,
        /// `enforce` or `observe`; defaults to `observe` (DESIGN.md §4.10's
        /// own SQL default) when omitted.
        #[arg(
            long = "verification-mode",
            value_parser = clap::builder::PossibleValuesParser::new([
                verification_mode::ENFORCE,
                verification_mode::OBSERVE,
            ])
        )]
        verification_mode: Option<String>,
        /// Rows/second the kill-switch release-drain ramp admits after a
        /// switch releases (DESIGN.md §5.2). Defaults to 500 (the column's
        /// own SQL default) when omitted.
        #[arg(long = "kill-switch-release-rate")]
        kill_switch_release_rate: Option<i32>,
        /// Operator identity recorded on the platform_audit row.
        #[arg(long)]
        actor: String,
    },
    /// Show a tenant's typed configuration, or report that none is set.
    Show {
        #[arg(long = "tenant-slug")]
        tenant_slug: String,
    },
}

#[derive(Subcommand)]
enum CustomerDekCommand {
    /// Ensures each given customer id has a customer_dek row, creating one
    /// where missing (DESIGN.md §7.6). Caller-supplied ids only — the
    /// customer projection doesn't exist yet (see T-008's Description).
    PreProvision {
        #[arg(long = "tenant-slug")]
        tenant_slug: String,
        #[arg(long = "customer-id", required = true)]
        customer_id: Vec<uuid::Uuid>,
        /// Operator identity recorded on the platform_audit row.
        #[arg(long)]
        actor: String,
    },
}

#[derive(Subcommand)]
enum TemplateCommand {
    /// Approve a new template version. Rejected if this exact
    /// `(template_id, version, locale)` has already been approved — bump
    /// `--version` instead (T-010 decision 5).
    Approve {
        #[arg(long = "tenant-slug")]
        tenant_slug: String,
        #[arg(long = "template-id")]
        template_id: String,
        #[arg(long)]
        version: i32,
        /// `sms`, `email`, or `whatsapp` (DESIGN.md §4.4).
        #[arg(
            long,
            value_parser = clap::builder::PossibleValuesParser::new([
                channel::SMS,
                channel::EMAIL,
                channel::WHATSAPP,
            ])
        )]
        channel: String,
        #[arg(long)]
        locale: String,
        /// Path to a file holding the template body — bodies can be
        /// multi-line, so this is a file rather than an inline flag.
        #[arg(long = "body-file")]
        body_file: PathBuf,
        /// Operator identity recorded on the platform_audit row and stamped
        /// as `approved_by`.
        #[arg(long)]
        actor: String,
    },
    /// Show one approved template version/locale, or report that none
    /// exists.
    Show {
        #[arg(long = "tenant-slug")]
        tenant_slug: String,
        #[arg(long = "template-id")]
        template_id: String,
        #[arg(long)]
        version: i32,
        #[arg(long)]
        locale: String,
    },
    /// List every approved version/locale for a `template_id`.
    List {
        #[arg(long = "tenant-slug")]
        tenant_slug: String,
        #[arg(long = "template-id")]
        template_id: String,
    },
    /// Render one approved template version/locale against `--var key=value`
    /// pairs, printing the rendered body. A key referenced by the body but
    /// not supplied here is a hard error (T-010 decision 3).
    Render {
        #[arg(long = "tenant-slug")]
        tenant_slug: String,
        #[arg(long = "template-id")]
        template_id: String,
        #[arg(long)]
        version: i32,
        #[arg(long)]
        locale: String,
        /// Repeatable `key=value` pair. For more than one, invoke this
        /// command directly with repeated `--var` flags — the `justfile`
        /// recipe only takes one (the same `customer-dek-pre-provision`
        /// limitation as `--customer-id`).
        #[arg(long = "var")]
        var: Vec<String>,
    },
}

#[derive(Subcommand)]
enum ProviderConfigCommand {
    /// Create or overwrite one `(channel, priority)` row in a tenant's
    /// provider list. Safe to re-run: identical inputs are an idempotent
    /// no-op, different inputs overwrite that row.
    Set {
        #[arg(long = "tenant-slug")]
        tenant_slug: String,
        /// `sms`, `email`, or `whatsapp` (DESIGN.md §4.4) — free text, not
        /// validated against `template::model::channel` (no shared
        /// enforcement exists between the two tables yet).
        #[arg(long)]
        channel: String,
        /// Failover order within the channel; 1 is tried first.
        #[arg(long)]
        priority: i16,
        /// Free-text label — no real vendor is wired up yet (T-012 decision 1).
        #[arg(long)]
        provider: String,
        /// Vault path; stored but not yet read (T-012 decision 3).
        #[arg(long = "credential-path")]
        credential_path: String,
        #[arg(long = "rate-limit-per-sec")]
        rate_limit_per_sec: i32,
        /// Operator identity recorded on the platform_audit row.
        #[arg(long)]
        actor: String,
    },
    /// List a tenant's provider list for one channel, in failover order.
    List {
        #[arg(long = "tenant-slug")]
        tenant_slug: String,
        #[arg(long)]
        channel: String,
    },
}

#[derive(Subcommand)]
enum PartitionLifecycleCommand {
    /// Ensure current+next month partitions exist, move partitions past 18
    /// months to the cold tablespace, and detach + drop partitions past the
    /// tenant's `tenant_config.retention_years` boundary. Skips the drop
    /// step entirely (never assumes a default) when the tenant has no
    /// `tenant_config` row.
    Run {
        #[arg(long = "tenant-slug")]
        tenant_slug: String,
    },
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    if matches!(cli.command, Command::Version) {
        println!("messgr-control {}", env!("CARGO_PKG_VERSION"));
        return;
    }

    tracing_subscriber::fmt::init();

    let config = Config::from_env();

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
                    server,
                } => {
                    let cert = if server {
                        dev_pki::issue_server_cert(
                            vault_keystore.client(),
                            config.profile,
                            &common_name,
                        )
                        .await
                    } else {
                        dev_pki::issue_cert(vault_keystore.client(), config.profile, &common_name)
                            .await
                    }
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
        // No Vault client is connected here either — neither arm touches
        // Transit or AppRole.
        Command::TenantConfig { command } => {
            match command {
                TenantConfigCommand::Set {
                    tenant_slug,
                    retention_years,
                    default_timezone,
                    default_locale,
                    schedule_horizon_days,
                    quota_day_boundary_tz,
                    verification_mode: verification_mode_arg,
                    kill_switch_release_rate,
                    actor,
                } => {
                    let input = TenantConfigInput {
                        retention_years,
                        default_timezone,
                        default_locale,
                        schedule_horizon_days: schedule_horizon_days.unwrap_or(90),
                        quota_day_boundary_tz,
                        verification_mode: verification_mode_arg
                            .unwrap_or_else(|| verification_mode::OBSERVE.to_string()),
                        kill_switch_release_rate: kill_switch_release_rate
                            .unwrap_or(500),
                    };

                    let outcome = set_tenant_config(
                    &control_pool,
                    &config.control_database_url,
                    &tenant_slug,
                    input,
                    &actor,
                )
                .await
                .unwrap_or_else(|err| {
                    panic!("failed to set tenant_config for tenant {tenant_slug:?}: {err}")
                });

                    println!("outcome={}", outcome.outcome);
                }
                TenantConfigCommand::Show { tenant_slug } => {
                    let config_row = show_tenant_config(
                    &control_pool,
                    &config.control_database_url,
                    &tenant_slug,
                )
                .await
                .unwrap_or_else(|err| {
                    panic!("failed to show tenant_config for tenant {tenant_slug:?}: {err}")
                });

                    match config_row {
                        Some(config_row) => println!(
                            "retention_years={} default_timezone={} default_locale={} \
                         schedule_horizon_days={} quota_day_boundary_tz={} verification_mode={} \
                         kill_switch_release_rate={}",
                            config_row.retention_years,
                            config_row.default_timezone,
                            config_row.default_locale,
                            config_row.schedule_horizon_days,
                            config_row.quota_day_boundary_tz,
                            config_row.verification_mode,
                            config_row.kill_switch_release_rate,
                        ),
                        None => println!("not configured"),
                    }
                }
            }
        }
        Command::CustomerDek { command } => {
            let vault_keystore = VaultKeyStore::connect(config.profile).expect(
                "failed to connect to Vault (has VAULT_ADDR/VAULT_TOKEN been set?)",
            );
            match command {
                CustomerDekCommand::PreProvision {
                    tenant_slug,
                    customer_id,
                    actor,
                } => {
                    let outcome = pre_provision_for_tenant(
                        &control_pool,
                        &config.control_database_url,
                        &tenant_slug,
                        &vault_keystore,
                        &customer_id,
                        &actor,
                    )
                    .await
                    .unwrap_or_else(|err| {
                        panic!("failed to pre-provision DEKs for tenant {tenant_slug:?}: {err}")
                    });

                    println!(
                        "created={} already_existed={}",
                        outcome.created, outcome.already_existed
                    );
                }
            }
        }
        // No Vault client is connected here either — none of the four arms
        // touches Transit or AppRole.
        Command::Template { command } => match command {
            TemplateCommand::Approve {
                tenant_slug,
                template_id,
                version,
                channel,
                locale,
                body_file,
                actor,
            } => {
                let body = std::fs::read_to_string(&body_file).unwrap_or_else(|err| {
                    panic!("failed to read {body_file:?}: {err}")
                });

                let outcome = approve_template(
                    &control_pool,
                    &config.control_database_url,
                    &tenant_slug,
                    &template_id,
                    version,
                    &channel,
                    &locale,
                    &body,
                    &actor,
                )
                .await
                .unwrap_or_else(|err| {
                    panic!(
                        "failed to approve template {template_id:?} version {version} \
                         locale {locale:?} for tenant {tenant_slug:?}: {err}"
                    )
                });

                println!("outcome={}", outcome.outcome);
            }
            TemplateCommand::Show {
                tenant_slug,
                template_id,
                version,
                locale,
            } => {
                let template = show_template(
                    &control_pool,
                    &config.control_database_url,
                    &tenant_slug,
                    &template_id,
                    version,
                    &locale,
                )
                .await
                .unwrap_or_else(|err| {
                    panic!(
                        "failed to show template {template_id:?} version {version} \
                         locale {locale:?} for tenant {tenant_slug:?}: {err}"
                    )
                });

                match template {
                    Some(template) => println!(
                        "template_id={} version={} channel={} locale={} approved_by={} \
                         approved_at={} body={:?}",
                        template.template_id,
                        template.version,
                        template.channel,
                        template.locale,
                        template.approved_by,
                        template.approved_at,
                        template.body,
                    ),
                    None => println!("not found"),
                }
            }
            TemplateCommand::List {
                tenant_slug,
                template_id,
            } => {
                let templates = list_template_versions(
                    &control_pool,
                    &config.control_database_url,
                    &tenant_slug,
                    &template_id,
                )
                .await
                .unwrap_or_else(|err| {
                    panic!(
                        "failed to list template versions for {template_id:?} tenant \
                         {tenant_slug:?}: {err}"
                    )
                });

                for template in templates {
                    println!(
                        "version={} locale={} channel={} approved_by={} approved_at={}",
                        template.version,
                        template.locale,
                        template.channel,
                        template.approved_by,
                        template.approved_at,
                    );
                }
            }
            TemplateCommand::Render {
                tenant_slug,
                template_id,
                version,
                locale,
                var,
            } => {
                let variables: std::collections::HashMap<String, String> = var
                    .iter()
                    .map(|pair| {
                        pair.split_once('=').unwrap_or_else(|| {
                            panic!("--var {pair:?} must be in key=value form")
                        })
                    })
                    .map(|(key, value)| (key.to_string(), value.to_string()))
                    .collect();

                let rendered = render_preview(
                    &control_pool,
                    &config.control_database_url,
                    &tenant_slug,
                    &template_id,
                    version,
                    &locale,
                    &variables,
                )
                .await
                .unwrap_or_else(|err| {
                    panic!(
                        "failed to render template {template_id:?} version {version} \
                         locale {locale:?} for tenant {tenant_slug:?}: {err}"
                    )
                });

                println!("{rendered}");
            }
        },
        // No Vault client is connected here either.
        Command::ProviderConfig { command } => match command {
            ProviderConfigCommand::Set {
                tenant_slug,
                channel,
                priority,
                provider,
                credential_path,
                rate_limit_per_sec,
                actor,
            } => {
                let input = ProviderConfigInput {
                    channel,
                    priority,
                    provider,
                    credential_path,
                    rate_limit_per_sec,
                };

                let outcome = set_provider_config(
                    &control_pool,
                    &config.control_database_url,
                    &tenant_slug,
                    input,
                    &actor,
                )
                .await
                .unwrap_or_else(|err| {
                    panic!("failed to set provider_config for tenant {tenant_slug:?}: {err}")
                });

                println!("outcome={}", outcome.outcome);
            }
            ProviderConfigCommand::List {
                tenant_slug,
                channel,
            } => {
                let rows = list_provider_config(
                    &control_pool,
                    &config.control_database_url,
                    &tenant_slug,
                    &channel,
                )
                .await
                .unwrap_or_else(|err| {
                    panic!(
                        "failed to list provider_config for tenant {tenant_slug:?} \
                         channel {channel:?}: {err}"
                    )
                });

                if rows.is_empty() {
                    println!("no provider configured for channel {channel}");
                } else {
                    for row in rows {
                        println!(
                            "priority={} provider={} credential_path={} rate_limit_per_sec={}",
                            row.priority,
                            row.provider,
                            row.credential_path,
                            row.rate_limit_per_sec,
                        );
                    }
                }
            }
        },
        // No Vault client is connected here either — partition-lifecycle
        // touches neither Transit nor AppRole.
        Command::PartitionLifecycle { command } => match command {
            PartitionLifecycleCommand::Run { tenant_slug } => {
                let report = run_partition_lifecycle(
                    &control_pool,
                    &config.control_database_url,
                    &tenant_slug,
                    chrono::Utc::now(),
                )
                .await
                .unwrap_or_else(|err| {
                    panic!("partition lifecycle run failed for tenant {tenant_slug:?}: {err}")
                });

                println!(
                    "created={} moved={} dropped={}",
                    report.created.len(),
                    report.moved.len(),
                    report.dropped.len()
                );
                if report.retention_skipped {
                    println!("retention: skipped (tenant_config not set)");
                }
            }
        },
        Command::Stats { tenant_slug, since } => {
            let rows = tenant_message_stats(
                &control_pool,
                &config.control_database_url,
                &tenant_slug,
                since,
            )
            .await
            .unwrap_or_else(|err| {
                panic!("failed to compute stats for tenant {tenant_slug:?}: {err}")
            });

            let mut channels: Vec<&str> =
                rows.iter().map(|r| r.channel.as_str()).collect();
            channels.sort_unstable();
            channels.dedup();

            for channel in channels {
                let total: i64 = rows
                    .iter()
                    .filter(|r| r.channel == channel)
                    .map(|r| r.count)
                    .sum();
                println!("{channel} total={total}");
                for row in rows.iter().filter(|r| r.channel == channel) {
                    println!("{channel} status={} count={}", row.status, row.count);
                }
            }
        }
        Command::Version => unreachable!("handled before Config::from_env() above"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_parses_as_its_own_subcommand() {
        let cli = Cli::try_parse_from(["messgr-control", "version"])
            .expect("parsing the version subcommand must succeed");

        assert!(matches!(cli.command, Command::Version));
    }

    #[test]
    fn tenant_config_set_defaults_schedule_horizon_and_verification_mode_when_omitted()
    {
        let cli = Cli::try_parse_from([
            "messgr-control",
            "tenant-config",
            "set",
            "--tenant-slug",
            "acme",
            "--retention-years",
            "7",
            "--default-timezone",
            "Europe/London",
            "--default-locale",
            "en-GB",
            "--quota-day-boundary-tz",
            "Europe/London",
            "--actor",
            "operator@example.com",
        ])
        .expect("parsing tenant-config set without the optional flags must succeed");

        let Command::TenantConfig {
            command:
                TenantConfigCommand::Set {
                    schedule_horizon_days,
                    verification_mode,
                    ..
                },
        } = cli.command
        else {
            panic!("expected TenantConfig::Set");
        };

        assert_eq!(schedule_horizon_days, None);
        assert_eq!(verification_mode, None);
    }

    #[test]
    fn tenant_config_set_rejects_an_invalid_verification_mode() {
        let result = Cli::try_parse_from([
            "messgr-control",
            "tenant-config",
            "set",
            "--tenant-slug",
            "acme",
            "--retention-years",
            "7",
            "--default-timezone",
            "Europe/London",
            "--default-locale",
            "en-GB",
            "--quota-day-boundary-tz",
            "Europe/London",
            "--verification-mode",
            "sometimes",
            "--actor",
            "operator@example.com",
        ]);

        assert!(
            result.is_err(),
            "an invalid --verification-mode value must fail to parse"
        );
    }

    #[test]
    fn template_approve_rejects_an_invalid_channel() {
        let result = Cli::try_parse_from([
            "messgr-control",
            "template",
            "approve",
            "--tenant-slug",
            "acme",
            "--template-id",
            "balance-alert",
            "--version",
            "1",
            "--channel",
            "carrier-pigeon",
            "--locale",
            "en-GB",
            "--body-file",
            "/tmp/does-not-need-to-exist.txt",
            "--actor",
            "operator@example.com",
        ]);

        assert!(
            result.is_err(),
            "an invalid --channel value must fail to parse"
        );
    }

    #[test]
    fn provider_config_set_parses_required_flags() {
        let cli = Cli::try_parse_from([
            "messgr-control",
            "provider-config",
            "set",
            "--tenant-slug",
            "acme",
            "--channel",
            "sms",
            "--priority",
            "1",
            "--provider",
            "generic-http",
            "--credential-path",
            "secret/data/acme/sms",
            "--rate-limit-per-sec",
            "10",
            "--actor",
            "operator@example.com",
        ])
        .expect("parsing provider-config set must succeed");

        let Command::ProviderConfig {
            command:
                ProviderConfigCommand::Set {
                    tenant_slug,
                    channel,
                    priority,
                    provider,
                    credential_path,
                    rate_limit_per_sec,
                    actor,
                },
        } = cli.command
        else {
            panic!("expected ProviderConfig::Set");
        };

        assert_eq!(tenant_slug, "acme");
        assert_eq!(channel, "sms");
        assert_eq!(priority, 1);
        assert_eq!(provider, "generic-http");
        assert_eq!(credential_path, "secret/data/acme/sms");
        assert_eq!(rate_limit_per_sec, 10);
        assert_eq!(actor, "operator@example.com");
    }
}
