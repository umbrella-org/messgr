use std::path::PathBuf;

use chrono::{DateTime, Utc};
use clap::{Parser, Subcommand};

use messgr::config::Config;
use messgr::consent::configure::set_consent;
use messgr::customer_dek::lifecycle::pre_provision_for_tenant;
use messgr::db;
use messgr::idempotency_sweep::run_for_tenant as run_idempotency_sweep;
use messgr::ingest::model::class;
use messgr::keystore::VaultKeyStore;
use messgr::orphan_reconcile::reconcile::run_for_tenant as run_orphan_reconcile;
use messgr::partition_lifecycle::lifecycle::run_for_tenant as run_partition_lifecycle;
use messgr::producer::dev_pki;
use messgr::producer::register::{disable_producer, list_producers, register_producer};
use messgr::producer_quota::configure::{
    add_producer_quota_override, list_producer_quota, list_producer_quota_overrides,
    set_producer_quota,
};
use messgr::producer_quota::model::enforcement;
use messgr::provider_config::configure::{list_provider_config, set_provider_config};
use messgr::provider_config::model::ProviderConfigInput;
use messgr::quiet_hours::configure::{set_quiet_hours_policy, show_quiet_hours_policy};
use messgr::quiet_hours::model::QuietHoursPolicyInput;
use messgr::stats::tenant_message_stats;
use messgr::suppression::configure::{
    add_suppression, list_suppression, remove_suppression,
};
use messgr::suppression::model::reason;
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
    /// mode, and quota day boundary. A fresh tenant has no
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
    /// §4.10, §12.1, T-012). `credential_path` is read by `messgr-dispatcher`
    /// at startup via Vault KV (T-023); `rate_limit_per_sec` is stored but not
    /// yet read by anything.
    ProviderConfig {
        #[command(subcommand)]
        command: ProviderConfigCommand,
    },
    /// Add, list, or remove suppression entries (hard bounce, complaint,
    /// regulatory hold) that block a destination at dispatch (DESIGN.md §5,
    /// T-038).
    Suppression {
        #[command(subcommand)]
        command: SuppressionCommand,
    },
    /// Record a customer's opt-in/opt-out for one message class on a
    /// destination (DESIGN.md §5, T-037), enforced by `messgr-dispatcher`'s
    /// consent gate at send time for `marketing` messages.
    Consent {
        #[command(subcommand)]
        command: ConsentCommand,
    },
    /// Set, list producer send quotas, and add/list time-boxed per-day
    /// overrides (DESIGN.md §5.1, T-042), enforced by `messgr-dispatcher`'s
    /// quota gate at dispatch time.
    ProducerQuota {
        #[command(subcommand)]
        command: ProducerQuotaCommand,
    },
    /// Set or show the tenant's institution-wide quiet-hours window (DESIGN.md
    /// §5, §6.1, T-043), enforced by `messgr-dispatcher`'s quiet-hours gate at
    /// send time. Only the institution-wide default is settable -- see
    /// `05-send-timing.md`'s §6.1 correction note for why per-segment/region
    /// windows aren't exposed here.
    QuietHours {
        #[command(subcommand)]
        command: QuietHoursCommand,
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
    /// Deletes idempotency rows past their retention window (DESIGN.md §4.3:
    /// "retained 30 days, swept nightly"). Meant to run on a schedule
    /// (cron/systemd timer) — this binary does not daemonize or loop. T-029.
    IdempotencySweep {
        #[command(subcommand)]
        command: IdempotencySweepCommand,
    },
    /// Match pending orphan_event rows (delivery-receipt webhooks that
    /// arrived before, or without, a matching comms_request) against
    /// comms_event.provider_ref, promote matches into a real comms_event row
    /// encrypted under the customer's DEK, and age out rows past a
    /// fixed-attempt reconcile cap (DESIGN.md §4.4/§10, T-022, T-030). Meant
    /// to run on a schedule (cron/systemd timer) -- this binary does not
    /// daemonize or loop.
    OrphanReconcile {
        #[command(subcommand)]
        command: OrphanReconcileCommand,
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
        #[arg(long = "retention-years", value_parser = clap::value_parser!(i32).range(0..))]
        retention_years: i32,
        #[arg(long = "default-timezone")]
        default_timezone: String,
        #[arg(long = "default-locale")]
        default_locale: String,
        /// Defaults to 90 (DESIGN.md §4.10's own SQL default) when omitted.
        #[arg(long = "schedule-horizon-days", value_parser = clap::value_parser!(i32).range(0..))]
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
        #[arg(long = "kill-switch-release-rate", value_parser = clap::value_parser!(i32).range(0..))]
        kill_switch_release_rate: Option<i32>,
        /// How many reconcile passes a pending `orphan_event` row survives
        /// before it's aged out and deleted (DESIGN.md §4.4/§10). Defaults
        /// to 5 (the column's own SQL default) when omitted.
        #[arg(long = "reconcile-attempts-cap", value_parser = clap::value_parser!(i16).range(1..))]
        reconcile_attempts_cap: Option<i16>,
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
        /// `sms`, `email`, or `whatsapp` (DESIGN.md §4.4) — validated against
        /// the same closed set `template approve --channel` uses (T-025 item 4).
        #[arg(
            long,
            value_parser = clap::builder::PossibleValuesParser::new([
                channel::SMS,
                channel::EMAIL,
                channel::WHATSAPP,
            ])
        )]
        channel: String,
        /// Failover order within the channel; 1 is tried first.
        #[arg(long)]
        priority: i16,
        /// Free-text label — no real vendor is wired up yet (T-012 decision 1).
        #[arg(long)]
        provider: String,
        /// Vault KV v2 path (`<mount>/data/<path>`, e.g. `secret/data/acme/sms`);
        /// read by `messgr-dispatcher` at startup (T-023). This command never
        /// touches the secret value itself, only this pointer to it.
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
        #[arg(
            long,
            value_parser = clap::builder::PossibleValuesParser::new([
                channel::SMS,
                channel::EMAIL,
                channel::WHATSAPP,
            ])
        )]
        channel: String,
    },
}

#[derive(Subcommand)]
enum SuppressionCommand {
    /// Add a new entry, or update an existing one's reason/review date.
    Add {
        #[arg(long = "tenant-slug")]
        tenant_slug: String,
        #[arg(long)]
        destination: String,
        #[arg(
            long,
            value_parser = clap::builder::PossibleValuesParser::new([
                reason::HARD_BOUNCE,
                reason::COMPLAINT,
                reason::REGULATORY_HOLD,
            ])
        )]
        reason: String,
        /// RFC 3339 timestamp; the entry stops blocking once this passes.
        #[arg(long = "review-at")]
        review_at: String,
        #[arg(long)]
        actor: String,
    },
    /// List every suppression entry for a tenant. destination_hmac is
    /// hex-printed -- it cannot be reversed to the original address.
    List {
        #[arg(long = "tenant-slug")]
        tenant_slug: String,
    },
    /// Retire an entry early (sets review_at to now).
    Remove {
        #[arg(long = "tenant-slug")]
        tenant_slug: String,
        #[arg(long)]
        destination: String,
        #[arg(long)]
        actor: String,
    },
}

#[derive(Subcommand)]
enum ConsentCommand {
    /// Record a customer's opt-in or opt-out for one message class on a
    /// destination that has already resolved to a real address. Rejected
    /// if no active address is on file yet -- this command never mints
    /// one (T-037 decision 1); record consent after the customer's first
    /// message, or via whatever event-feed consumer eventually lands
    /// (build-order step 10).
    Set {
        #[arg(long = "tenant-slug")]
        tenant_slug: String,
        #[arg(long)]
        destination: String,
        #[arg(
            long,
            value_parser = clap::builder::PossibleValuesParser::new([
                channel::SMS,
                channel::EMAIL,
                channel::WHATSAPP,
            ])
        )]
        channel: String,
        #[arg(
            long,
            value_parser = clap::builder::PossibleValuesParser::new([
                class::TRANSACTIONAL,
                class::MARKETING,
            ])
        )]
        class: String,
        #[arg(
            long = "opted-in",
            action = clap::ArgAction::Set,
            value_parser = clap::value_parser!(bool)
        )]
        opted_in: bool,
        /// Where this opt-in/out was captured, for evidence (e.g. web_form,
        /// ivr_call, branch_visit, sms_stop_reply) -- not the customer's own words.
        #[arg(long)]
        source: String,
        /// Operator identity recorded on the platform_audit row.
        #[arg(long)]
        actor: String,
    },
}

fn parse_local_time(value: &str) -> Result<chrono::NaiveTime, String> {
    chrono::NaiveTime::parse_from_str(value, "%H:%M")
        .map_err(|_| format!("expected HH:MM (24-hour), got {value:?}"))
}

#[derive(Subcommand)]
enum QuietHoursCommand {
    /// Create or overwrite the tenant's quiet-hours window. Safe to re-run:
    /// identical inputs are an idempotent no-op.
    Set {
        #[arg(long = "tenant-slug")]
        tenant_slug: String,
        #[arg(long = "start-local", value_parser = parse_local_time)]
        start_local: chrono::NaiveTime,
        #[arg(long = "end-local", value_parser = parse_local_time)]
        end_local: chrono::NaiveTime,
        /// Operator identity recorded on the platform_audit row.
        #[arg(long)]
        actor: String,
    },
    /// Show the tenant's quiet-hours window, or report that none is set.
    Show {
        #[arg(long = "tenant-slug")]
        tenant_slug: String,
    },
}

#[derive(Subcommand)]
enum ProducerQuotaCommand {
    /// Set (create or update) one (producer, channel, class) quota row.
    /// `auth` is never a legal `--class` value (AGENTS.md invariant 5: quota
    /// must never block auth traffic); `--enforcement hard` is rejected for
    /// `transactional` for the same reason.
    Set {
        #[arg(long = "tenant-slug")]
        tenant_slug: String,
        #[arg(long = "producer-name")]
        producer_name: String,
        #[arg(
            long,
            value_parser = clap::builder::PossibleValuesParser::new([
                channel::SMS,
                channel::EMAIL,
                channel::WHATSAPP,
            ])
        )]
        channel: String,
        #[arg(
            long,
            value_parser = clap::builder::PossibleValuesParser::new([
                class::MARKETING,
                class::TRANSACTIONAL,
            ])
        )]
        class: String,
        /// Burst ceiling; omit for unlimited.
        #[arg(long = "per-minute")]
        per_minute: Option<i32>,
        /// Daily total; omit for unlimited.
        #[arg(long = "per-day")]
        per_day: Option<i32>,
        #[arg(
            long,
            value_parser = clap::builder::PossibleValuesParser::new([
                enforcement::HARD,
                enforcement::SOFT,
            ])
        )]
        enforcement: String,
        #[arg(long)]
        actor: String,
    },
    /// List every producer_quota row for a tenant.
    List {
        #[arg(long = "tenant-slug")]
        tenant_slug: String,
    },
    /// Add a time-boxed per_day uplift (e.g. a campaign day). Raises
    /// `per_day` only -- `enforcement` is always inherited from the base
    /// `producer_quota` row.
    OverrideAdd {
        #[arg(long = "tenant-slug")]
        tenant_slug: String,
        #[arg(long = "producer-name")]
        producer_name: String,
        #[arg(
            long,
            value_parser = clap::builder::PossibleValuesParser::new([
                channel::SMS,
                channel::EMAIL,
                channel::WHATSAPP,
            ])
        )]
        channel: String,
        #[arg(
            long,
            value_parser = clap::builder::PossibleValuesParser::new([
                class::MARKETING,
                class::TRANSACTIONAL,
            ])
        )]
        class: String,
        #[arg(long = "per-day")]
        per_day: i32,
        /// RFC 3339 timestamp; the override starts applying at this instant.
        #[arg(long = "valid-from")]
        valid_from: String,
        /// RFC 3339 timestamp; the override stops applying at this instant.
        #[arg(long = "valid-to")]
        valid_to: String,
        #[arg(long = "approved-by")]
        approved_by: String,
        #[arg(long)]
        reason: String,
        #[arg(long)]
        actor: String,
    },
    /// List every producer_quota_override row for a tenant.
    OverrideList {
        #[arg(long = "tenant-slug")]
        tenant_slug: String,
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

#[derive(Subcommand)]
enum IdempotencySweepCommand {
    /// Delete every idempotency row whose expires_at is at or before now.
    Run {
        #[arg(long = "tenant-slug")]
        tenant_slug: String,
    },
}

#[derive(Subcommand)]
enum OrphanReconcileCommand {
    /// Match pending orphan_event rows against comms_event.provider_ref,
    /// promote matches into comms_event (encrypted under the customer's
    /// DEK), age out rows past the reconcile_attempts cap.
    Run {
        #[arg(long = "tenant-slug")]
        tenant_slug: String,
    },
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let cli = Cli::parse();

    if matches!(cli.command, Command::Version) {
        println!("messgr-control {}", env!("CARGO_PKG_VERSION"));
        return std::process::ExitCode::SUCCESS;
    }

    tracing_subscriber::fmt::init();

    let config = Config::from_env();

    // Connecting to the control database is process startup, not a
    // subcommand's own operation (T-025 decision 2) — a process that can't
    // reach its own required infrastructure has nothing useful to do, so
    // panicking loudly here matches `Config::from_env`'s own precedent.
    let control_pool = db::connect(
        &config.control_database_url,
        config.database_max_connections,
    )
    .await
    .expect("failed to connect to control database");

    match run(cli, config, control_pool).await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err}");
            std::process::ExitCode::FAILURE
        }
    }
}

/// Everything a subcommand actually does, once past process startup (T-025
/// decision 2) — every failure here is `?`-propagated instead of panicking,
/// so a bad tenant slug or a transient DB/Vault failure is a reported error
/// with a non-zero exit, not a crash.
async fn run(
    cli: Cli,
    config: Config,
    control_pool: sqlx::PgPool,
) -> Result<(), String> {
    match cli.command {
        Command::Migrate => {
            sqlx::migrate!("./migrations/control")
                .run(&control_pool)
                .await
                .map_err(|err| {
                    format!("failed to run control database migrations: {err}")
                })?;
            println!("control database migrations applied");
        }
        Command::Provision {
            slug,
            region,
            database_name,
            actor,
        } => {
            // Connected only here, not unconditionally in `main` — `Migrate`
            // has no Vault dependency and must not gain one (§13: never
            // couple a subcommand to a service it doesn't use). Connecting
            // to Vault is startup-class regardless of call site (decision
            // 2) — left as `.expect()`.
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
            .map_err(|err| {
                format!(
                    "failed to provision tenant {slug:?} (has `messgr-control migrate` been \
                     run against the control database, and is Vault reachable and unsealed?): {err}"
                )
            })?;

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
                .map_err(|err| {
                    format!("failed to register producer {name:?} for tenant {tenant_slug:?}: {err}")
                })?;

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
                .map_err(|err| {
                    format!("failed to disable producer {name:?} for tenant {tenant_slug:?}: {err}")
                })?;

                println!("outcome={}", outcome.outcome);
            }
            ProducerCommand::List { tenant_slug } => {
                let producers = list_producers(
                    &control_pool,
                    &config.control_database_url,
                    &tenant_slug,
                )
                .await
                .map_err(|err| {
                    format!(
                        "failed to list producers for tenant {tenant_slug:?}: {err}"
                    )
                })?;

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
            // Connecting to Vault is startup-class regardless of call site
            // (decision 2) — left as `.expect()`.
            let vault_keystore = VaultKeyStore::connect(config.profile).expect(
                "failed to connect to Vault (has VAULT_ADDR/VAULT_TOKEN been set?)",
            );

            match command {
                DevPkiCommand::Bootstrap => {
                    dev_pki::bootstrap(vault_keystore.client(), config.profile)
                        .await
                        .map_err(|err| format!("failed to bootstrap dev PKI: {err}"))?;
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
                    .map_err(|err| {
                        format!(
                            "failed to issue a dev certificate for {common_name:?} (has \
                             `messgr-control dev-pki bootstrap` been run?): {err}"
                        )
                    })?;

                    std::fs::create_dir_all(&out_dir).map_err(|err| {
                        format!("failed to create {out_dir:?}: {err}")
                    })?;
                    std::fs::write(out_dir.join("cert.pem"), &cert.certificate)
                        .map_err(|err| format!("failed to write cert.pem: {err}"))?;
                    std::fs::write(out_dir.join("key.pem"), &cert.private_key)
                        .map_err(|err| format!("failed to write key.pem: {err}"))?;
                    std::fs::write(out_dir.join("ca.pem"), &cert.issuing_ca)
                        .map_err(|err| format!("failed to write ca.pem: {err}"))?;

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
                    reconcile_attempts_cap,
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
                        reconcile_attempts_cap: reconcile_attempts_cap.unwrap_or(5),
                    };

                    let outcome = set_tenant_config(
                    &control_pool,
                    &config.control_database_url,
                    &tenant_slug,
                    input,
                    &actor,
                )
                .await
                .map_err(|err| {
                    format!("failed to set tenant_config for tenant {tenant_slug:?}: {err}")
                })?;

                    println!("outcome={}", outcome.outcome);
                }
                TenantConfigCommand::Show { tenant_slug } => {
                    let config_row = show_tenant_config(
                    &control_pool,
                    &config.control_database_url,
                    &tenant_slug,
                )
                .await
                .map_err(|err| {
                    format!("failed to show tenant_config for tenant {tenant_slug:?}: {err}")
                })?;

                    match config_row {
                        Some(config_row) => println!(
                            "retention_years={} default_timezone={} default_locale={} \
                         schedule_horizon_days={} quota_day_boundary_tz={} verification_mode={} \
                         kill_switch_release_rate={} reconcile_attempts_cap={}",
                            config_row.retention_years,
                            config_row.default_timezone,
                            config_row.default_locale,
                            config_row.schedule_horizon_days,
                            config_row.quota_day_boundary_tz,
                            config_row.verification_mode,
                            config_row.kill_switch_release_rate,
                            config_row.reconcile_attempts_cap,
                        ),
                        None => println!("not configured"),
                    }
                }
            }
        }
        Command::CustomerDek { command } => {
            // Connecting to Vault is startup-class regardless of call site
            // (decision 2) — left as `.expect()`.
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
                    .map_err(|err| {
                        format!("failed to pre-provision DEKs for tenant {tenant_slug:?}: {err}")
                    })?;

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
                let body = std::fs::read_to_string(&body_file)
                    .map_err(|err| format!("failed to read {body_file:?}: {err}"))?;

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
                .map_err(|err| {
                    format!(
                        "failed to approve template {template_id:?} version {version} \
                         locale {locale:?} for tenant {tenant_slug:?}: {err}"
                    )
                })?;

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
                .map_err(|err| {
                    format!(
                        "failed to show template {template_id:?} version {version} \
                         locale {locale:?} for tenant {tenant_slug:?}: {err}"
                    )
                })?;

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
                .map_err(|err| {
                    format!(
                        "failed to list template versions for {template_id:?} tenant \
                         {tenant_slug:?}: {err}"
                    )
                })?;

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
                let mut variables: std::collections::HashMap<String, String> =
                    std::collections::HashMap::new();
                for pair in &var {
                    let (key, value) = pair.split_once('=').ok_or_else(|| {
                        format!("--var {pair:?} must be in key=value form")
                    })?;
                    variables.insert(key.to_string(), value.to_string());
                }

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
                .map_err(|err| {
                    format!(
                        "failed to render template {template_id:?} version {version} \
                         locale {locale:?} for tenant {tenant_slug:?}: {err}"
                    )
                })?;

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
                .map_err(|err| {
                    format!("failed to set provider_config for tenant {tenant_slug:?}: {err}")
                })?;

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
                .map_err(|err| {
                    format!(
                        "failed to list provider_config for tenant {tenant_slug:?} \
                         channel {channel:?}: {err}"
                    )
                })?;

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
        Command::Suppression { command } => {
            let vault_keystore = VaultKeyStore::connect(config.profile).expect(
                "failed to connect to Vault (has VAULT_ADDR/VAULT_TOKEN been set?)",
            );

            match command {
                SuppressionCommand::Add {
                    tenant_slug,
                    destination,
                    reason,
                    review_at,
                    actor,
                } => {
                    let review_at = DateTime::parse_from_rfc3339(&review_at)
                        .map(|dt| dt.with_timezone(&Utc))
                        .map_err(|err| format!(
                            "invalid --review-at {review_at:?} (expected RFC 3339): {err}"
                        ))?;
                    let outcome = add_suppression(
                        &control_pool,
                        &config.control_database_url,
                        &tenant_slug,
                        &vault_keystore,
                        &destination,
                        &reason,
                        review_at,
                        &actor,
                    )
                    .await
                    .map_err(|err| format!(
                        "failed to add suppression entry for tenant {tenant_slug:?}: {err}"
                    ))?;
                    println!("outcome={}", outcome.outcome);
                }
                SuppressionCommand::List { tenant_slug } => {
                    let rows = list_suppression(&control_pool, &config.control_database_url, &tenant_slug)
                        .await
                        .map_err(|err| format!(
                            "failed to list suppression entries for tenant {tenant_slug:?}: {err}"
                        ))?;
                    if rows.is_empty() {
                        println!("no suppression entries for tenant {tenant_slug}");
                    } else {
                        let now = Utc::now();
                        for row in rows {
                            let status = if row.review_at > now {
                                "active"
                            } else {
                                "expired"
                            };
                            let hmac_hex: String = row
                                .destination_hmac
                                .iter()
                                .map(|b| format!("{b:02x}"))
                                .collect();
                            println!(
                                "destination_hmac={hmac_hex} reason={} added_at={} review_at={} status={status}",
                                row.reason, row.added_at, row.review_at,
                            );
                        }
                    }
                }
                SuppressionCommand::Remove {
                    tenant_slug,
                    destination,
                    actor,
                } => {
                    remove_suppression(
                        &control_pool,
                        &config.control_database_url,
                        &tenant_slug,
                        &vault_keystore,
                        &destination,
                        &actor,
                    )
                    .await
                    .map_err(|err| format!(
                        "failed to remove suppression entry for tenant {tenant_slug:?}: {err}"
                    ))?;
                    println!("outcome=retired");
                }
            }
        }
        Command::Consent { command } => {
            let vault_keystore = VaultKeyStore::connect(config.profile).expect(
                "failed to connect to Vault (has VAULT_ADDR/VAULT_TOKEN been set?)",
            );
            match command {
                ConsentCommand::Set {
                    tenant_slug,
                    destination,
                    channel,
                    class,
                    opted_in,
                    source,
                    actor,
                } => {
                    let outcome = set_consent(
                        &control_pool,
                        &config.control_database_url,
                        &tenant_slug,
                        &vault_keystore,
                        &destination,
                        &channel,
                        &class,
                        opted_in,
                        &source,
                        &actor,
                    )
                    .await
                    .map_err(|err| {
                        format!(
                            "failed to set consent for tenant {tenant_slug:?}: {err}"
                        )
                    })?;
                    println!("outcome={}", outcome.outcome);
                }
            }
        }
        // No Vault client here either — nothing in producer_quota computes
        // an HMAC or reads a KV secret.
        Command::ProducerQuota { command } => {
            match command {
                ProducerQuotaCommand::Set {
                    tenant_slug,
                    producer_name,
                    channel,
                    class,
                    per_minute,
                    per_day,
                    enforcement,
                    actor,
                } => {
                    let outcome = set_producer_quota(
                    &control_pool,
                    &config.control_database_url,
                    &tenant_slug,
                    &producer_name,
                    &channel,
                    &class,
                    per_minute,
                    per_day,
                    &enforcement,
                    &actor,
                )
                .await
                .map_err(|err| {
                    format!(
                        "failed to set producer quota for tenant {tenant_slug:?}: {err}"
                    )
                })?;
                    println!("outcome={}", outcome.outcome);
                }
                ProducerQuotaCommand::List { tenant_slug } => {
                    let rows = list_producer_quota(
                    &control_pool,
                    &config.control_database_url,
                    &tenant_slug,
                )
                .await
                .map_err(|err| {
                    format!("failed to list producer quotas for tenant {tenant_slug:?}: {err}")
                })?;
                    if rows.is_empty() {
                        println!("no producer_quota rows for tenant {tenant_slug}");
                    } else {
                        for row in rows {
                            println!(
                                "producer_id={} channel={} class={} per_minute={} per_day={} enforcement={}",
                                row.producer_id,
                                row.channel,
                                row.class,
                                row.per_minute
                                    .map_or("unlimited".to_string(), |v| v.to_string()),
                                row.per_day
                                    .map_or("unlimited".to_string(), |v| v.to_string()),
                                row.enforcement,
                            );
                        }
                    }
                }
                ProducerQuotaCommand::OverrideAdd {
                    tenant_slug,
                    producer_name,
                    channel,
                    class,
                    per_day,
                    valid_from,
                    valid_to,
                    approved_by,
                    reason,
                    actor,
                } => {
                    let valid_from = DateTime::parse_from_rfc3339(&valid_from)
                    .map(|dt| dt.with_timezone(&Utc))
                    .map_err(|err| {
                        format!("invalid --valid-from {valid_from:?} (expected RFC 3339): {err}")
                    })?;
                    let valid_to = DateTime::parse_from_rfc3339(&valid_to)
                    .map(|dt| dt.with_timezone(&Utc))
                    .map_err(|err| {
                        format!("invalid --valid-to {valid_to:?} (expected RFC 3339): {err}")
                    })?;
                    let id = add_producer_quota_override(
                    &control_pool,
                    &config.control_database_url,
                    &tenant_slug,
                    &producer_name,
                    &channel,
                    &class,
                    per_day,
                    valid_from,
                    valid_to,
                    &approved_by,
                    &reason,
                    &actor,
                )
                .await
                .map_err(|err| {
                    format!(
                        "failed to add producer quota override for tenant {tenant_slug:?}: {err}"
                    )
                })?;
                    println!("id={id}");
                }
                ProducerQuotaCommand::OverrideList { tenant_slug } => {
                    let rows = list_producer_quota_overrides(
                    &control_pool,
                    &config.control_database_url,
                    &tenant_slug,
                )
                .await
                .map_err(|err| {
                    format!(
                        "failed to list producer quota overrides for tenant {tenant_slug:?}: {err}"
                    )
                })?;
                    if rows.is_empty() {
                        println!(
                            "no producer_quota_override rows for tenant {tenant_slug}"
                        );
                    } else {
                        for row in rows {
                            println!(
                                "id={} producer_id={} channel={} class={} per_day={} valid_from={} valid_to={} approved_by={} reason={}",
                                row.id,
                                row.producer_id,
                                row.channel,
                                row.class,
                                row.per_day,
                                row.valid_from,
                                row.valid_to,
                                row.approved_by,
                                row.reason,
                            );
                        }
                    }
                }
            }
        }
        // No Vault client is connected here either -- quiet-hours touches
        // neither Transit nor AppRole, matching tenant-config.
        Command::QuietHours { command } => match command {
            QuietHoursCommand::Set {
                tenant_slug,
                start_local,
                end_local,
                actor,
            } => {
                let outcome = set_quiet_hours_policy(
                    &control_pool,
                    &config.control_database_url,
                    &tenant_slug,
                    QuietHoursPolicyInput {
                        start_local,
                        end_local,
                    },
                    &actor,
                )
                .await
                .map_err(|err| {
                    format!("failed to set quiet_hours_policy for tenant {tenant_slug:?}: {err}")
                })?;
                println!("outcome={}", outcome.outcome);
            }
            QuietHoursCommand::Show { tenant_slug } => {
                let policy = show_quiet_hours_policy(
                    &control_pool,
                    &config.control_database_url,
                    &tenant_slug,
                )
                .await
                .map_err(|err| {
                    format!("failed to show quiet_hours_policy for tenant {tenant_slug:?}: {err}")
                })?;

                match policy {
                    Some(policy) => println!(
                        "start_local={} end_local={}",
                        policy.start_local, policy.end_local
                    ),
                    None => println!(
                        "no quiet-hours policy configured for tenant {tenant_slug}"
                    ),
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
                .map_err(|err| {
                    format!("partition lifecycle run failed for tenant {tenant_slug:?}: {err}")
                })?;

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
        // No Vault client is connected here either — idempotency holds no
        // PII (the key and comms_request_id are opaque).
        Command::IdempotencySweep { command } => match command {
            IdempotencySweepCommand::Run { tenant_slug } => {
                let deleted = run_idempotency_sweep(
                    &control_pool,
                    &config.control_database_url,
                    &tenant_slug,
                    chrono::Utc::now(),
                    config.database_max_connections,
                )
                .await
                .map_err(|err| {
                    format!(
                        "idempotency sweep failed for tenant {tenant_slug:?}: {err}"
                    )
                })?;

                println!("deleted={deleted}");
            }
        },
        Command::OrphanReconcile { command } => {
            // Unlike idempotency-sweep and partition-lifecycle, this command
            // does connect to Vault (admin-token client): a promoted
            // orphan_event's raw payload must be encrypted under the
            // matched customer's DEK before it becomes a comms_event row
            // (AGENTS.md hard invariant 7) -- matching messgr-ingest's own
            // T-011 decision 5 rationale, this is a per-invocation,
            // multi-tenant-over-time CLI, not the single-tenant AppRole case
            // `connect_as_tenant` fits. Connecting to Vault is startup-class
            // regardless of call site (decision 2) -- left as `.expect()`.
            let vault_keystore = VaultKeyStore::connect(config.profile).expect(
                "failed to connect to Vault (has VAULT_ADDR/VAULT_TOKEN been set?)",
            );
            match command {
                OrphanReconcileCommand::Run { tenant_slug } => {
                    let report = run_orphan_reconcile(
                        &control_pool,
                        &config.control_database_url,
                        &tenant_slug,
                        &vault_keystore,
                        config.database_max_connections,
                    )
                    .await
                    .map_err(|err| {
                        format!(
                            "orphan reconcile run failed for tenant {tenant_slug:?}: {err}"
                        )
                    })?;

                    println!(
                        "reconciled={} aged_out={} still_pending={}",
                        report.reconciled, report.aged_out, report.still_pending
                    );
                }
            }
        }
        Command::Stats { tenant_slug, since } => {
            let rows = tenant_message_stats(
                &control_pool,
                &config.control_database_url,
                &tenant_slug,
                since,
            )
            .await
            .map_err(|err| {
                format!("failed to compute stats for tenant {tenant_slug:?}: {err}")
            })?;

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

    Ok(())
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

    #[test]
    fn provider_config_set_rejects_an_invalid_channel() {
        let result = Cli::try_parse_from([
            "messgr-control",
            "provider-config",
            "set",
            "--tenant-slug",
            "acme",
            "--channel",
            "carrier-pigeon",
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
        ]);

        assert!(
            result.is_err(),
            "an invalid --channel value must fail to parse"
        );
    }

    #[test]
    fn provider_config_list_rejects_an_invalid_channel() {
        let result = Cli::try_parse_from([
            "messgr-control",
            "provider-config",
            "list",
            "--tenant-slug",
            "acme",
            "--channel",
            "carrier-pigeon",
        ]);

        assert!(
            result.is_err(),
            "an invalid --channel value must fail to parse"
        );
    }

    #[test]
    fn suppression_add_parses_every_flag() {
        let cli = Cli::try_parse_from([
            "messgr-control",
            "suppression",
            "add",
            "--tenant-slug",
            "acme",
            "--destination",
            "+15550100",
            "--reason",
            "hard_bounce",
            "--review-at",
            "2027-01-01T00:00:00Z",
            "--actor",
            "operator@example.com",
        ])
        .expect("parsing suppression add must succeed");

        let Command::Suppression {
            command:
                SuppressionCommand::Add {
                    tenant_slug,
                    destination,
                    reason,
                    review_at,
                    actor,
                },
        } = cli.command
        else {
            panic!("expected Suppression::Add");
        };

        assert_eq!(tenant_slug, "acme");
        assert_eq!(destination, "+15550100");
        assert_eq!(reason, "hard_bounce");
        assert_eq!(review_at, "2027-01-01T00:00:00Z");
        assert_eq!(actor, "operator@example.com");
    }

    #[test]
    fn suppression_add_rejects_an_invalid_reason() {
        let result = Cli::try_parse_from([
            "messgr-control",
            "suppression",
            "add",
            "--tenant-slug",
            "acme",
            "--destination",
            "+15550100",
            "--reason",
            "annoyed",
            "--review-at",
            "2027-01-01T00:00:00Z",
            "--actor",
            "operator@example.com",
        ]);

        assert!(
            result.is_err(),
            "an invalid --reason value must fail to parse"
        );
    }

    #[test]
    fn suppression_list_parses() {
        let cli = Cli::try_parse_from([
            "messgr-control",
            "suppression",
            "list",
            "--tenant-slug",
            "acme",
        ])
        .expect("parsing suppression list must succeed");

        let Command::Suppression {
            command: SuppressionCommand::List { tenant_slug },
        } = cli.command
        else {
            panic!("expected Suppression::List");
        };

        assert_eq!(tenant_slug, "acme");
    }

    #[test]
    fn suppression_remove_parses() {
        let cli = Cli::try_parse_from([
            "messgr-control",
            "suppression",
            "remove",
            "--tenant-slug",
            "acme",
            "--destination",
            "+15550100",
            "--actor",
            "operator@example.com",
        ])
        .expect("parsing suppression remove must succeed");

        let Command::Suppression {
            command:
                SuppressionCommand::Remove {
                    tenant_slug,
                    destination,
                    actor,
                },
        } = cli.command
        else {
            panic!("expected Suppression::Remove");
        };

        assert_eq!(tenant_slug, "acme");
        assert_eq!(destination, "+15550100");
        assert_eq!(actor, "operator@example.com");
    }

    #[test]
    fn tenant_config_set_rejects_a_negative_retention_years() {
        let result = Cli::try_parse_from([
            "messgr-control",
            "tenant-config",
            "set",
            "--tenant-slug",
            "acme",
            "--retention-years",
            "-1",
            "--default-timezone",
            "Europe/London",
            "--default-locale",
            "en-GB",
            "--quota-day-boundary-tz",
            "Europe/London",
            "--actor",
            "operator@example.com",
        ]);

        assert!(
            result.is_err(),
            "a negative --retention-years must fail to parse"
        );
    }

    #[test]
    fn tenant_config_set_rejects_a_negative_schedule_horizon_days() {
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
            "--schedule-horizon-days",
            "-1",
            "--quota-day-boundary-tz",
            "Europe/London",
            "--actor",
            "operator@example.com",
        ]);

        assert!(
            result.is_err(),
            "a negative --schedule-horizon-days must fail to parse"
        );
    }

    #[test]
    fn tenant_config_set_rejects_a_negative_kill_switch_release_rate() {
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
            "--kill-switch-release-rate",
            "-1",
            "--actor",
            "operator@example.com",
        ]);

        assert!(
            result.is_err(),
            "a negative --kill-switch-release-rate must fail to parse"
        );
    }

    #[test]
    fn tenant_config_set_rejects_a_zero_reconcile_attempts_cap() {
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
            "--reconcile-attempts-cap",
            "0",
            "--actor",
            "operator@example.com",
        ]);

        assert!(
            result.is_err(),
            "a zero --reconcile-attempts-cap must fail to parse"
        );
    }

    #[test]
    fn consent_set_parses_every_flag() {
        let cli = Cli::try_parse_from([
            "messgr-control",
            "consent",
            "set",
            "--tenant-slug",
            "acme",
            "--destination",
            "+15550100",
            "--channel",
            "sms",
            "--class",
            "marketing",
            "--opted-in",
            "true",
            "--source",
            "web_form",
            "--actor",
            "operator@example.com",
        ])
        .expect("parsing consent set must succeed");

        let Command::Consent {
            command:
                ConsentCommand::Set {
                    tenant_slug,
                    destination,
                    channel,
                    class,
                    opted_in,
                    source,
                    actor,
                },
        } = cli.command
        else {
            panic!("expected Consent::Set");
        };

        assert_eq!(tenant_slug, "acme");
        assert_eq!(destination, "+15550100");
        assert_eq!(channel, "sms");
        assert_eq!(class, "marketing");
        assert!(opted_in);
        assert_eq!(source, "web_form");
        assert_eq!(actor, "operator@example.com");
    }

    #[test]
    fn consent_set_rejects_an_invalid_class() {
        let result = Cli::try_parse_from([
            "messgr-control",
            "consent",
            "set",
            "--tenant-slug",
            "acme",
            "--destination",
            "+15550100",
            "--channel",
            "sms",
            "--class",
            "auth",
            "--opted-in",
            "true",
            "--source",
            "web_form",
            "--actor",
            "operator@example.com",
        ]);

        assert!(
            result.is_err(),
            "an invalid --class value must fail to parse"
        );
    }

    #[test]
    fn consent_set_rejects_an_invalid_channel() {
        let result = Cli::try_parse_from([
            "messgr-control",
            "consent",
            "set",
            "--tenant-slug",
            "acme",
            "--destination",
            "+15550100",
            "--channel",
            "carrier-pigeon",
            "--class",
            "marketing",
            "--opted-in",
            "true",
            "--source",
            "web_form",
            "--actor",
            "operator@example.com",
        ]);

        assert!(
            result.is_err(),
            "an invalid --channel value must fail to parse"
        );
    }

    #[test]
    fn consent_set_rejects_a_non_boolean_opted_in() {
        let result = Cli::try_parse_from([
            "messgr-control",
            "consent",
            "set",
            "--tenant-slug",
            "acme",
            "--destination",
            "+15550100",
            "--channel",
            "sms",
            "--class",
            "marketing",
            "--opted-in",
            "sometimes",
            "--source",
            "web_form",
            "--actor",
            "operator@example.com",
        ]);

        assert!(
            result.is_err(),
            "a non-boolean --opted-in value must fail to parse"
        );
    }

    #[test]
    fn quiet_hours_set_parses_every_flag() {
        let cli = Cli::try_parse_from([
            "messgr-control",
            "quiet-hours",
            "set",
            "--tenant-slug",
            "acme",
            "--start-local",
            "22:00",
            "--end-local",
            "07:00",
            "--actor",
            "operator@example.com",
        ])
        .expect("parsing quiet-hours set must succeed");

        let Command::QuietHours {
            command:
                QuietHoursCommand::Set {
                    tenant_slug,
                    start_local,
                    end_local,
                    actor,
                },
        } = cli.command
        else {
            panic!("expected QuietHours::Set");
        };

        assert_eq!(tenant_slug, "acme");
        assert_eq!(
            start_local,
            chrono::NaiveTime::from_hms_opt(22, 0, 0).unwrap()
        );
        assert_eq!(end_local, chrono::NaiveTime::from_hms_opt(7, 0, 0).unwrap());
        assert_eq!(actor, "operator@example.com");
    }

    #[test]
    fn quiet_hours_set_rejects_an_invalid_time() {
        let result = Cli::try_parse_from([
            "messgr-control",
            "quiet-hours",
            "set",
            "--tenant-slug",
            "acme",
            "--start-local",
            "25:99",
            "--end-local",
            "07:00",
            "--actor",
            "operator@example.com",
        ]);

        assert!(
            result.is_err(),
            "an invalid --start-local value must fail to parse"
        );
    }

    #[test]
    fn quiet_hours_show_parses() {
        let cli = Cli::try_parse_from([
            "messgr-control",
            "quiet-hours",
            "show",
            "--tenant-slug",
            "acme",
        ])
        .expect("parsing quiet-hours show must succeed");

        let Command::QuietHours {
            command: QuietHoursCommand::Show { tenant_slug },
        } = cli.command
        else {
            panic!("expected QuietHours::Show");
        };

        assert_eq!(tenant_slug, "acme");
    }
}
