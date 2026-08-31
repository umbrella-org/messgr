# messgr justfile

version := `git describe --tags --always --dirty 2>/dev/null || echo "dev"`
bin     := "messgr-control"

# List recipes
@_:
    just --list

# Print the current version (git describe --tags --always --dirty)
[group('meta')]
version:
    @echo {{version}}

# Alias for `build`
[group('build')]
default: build

# Debug build
[group('build')]
build:
    cargo build

# Release build
[group('build')]
build-prod:
    cargo build --release

# Run the test suite
[group('build')]
test:
    cargo test

# Format code
[group('build')]
fmt:
    cargo fmt

# Run clippy with warnings denied
[group('build')]
lint:
    cargo clippy -- -D warnings

# Type-check without building
[group('build')]
check:
    cargo check

# Remove build artifacts
[group('build')]
clean:
    cargo clean

# Start Postgres and the dev-mode Vault
[group('db')]
db-up:
    docker compose up -d

# Stop Postgres and the dev-mode Vault
[group('db')]
db-down:
    docker compose down

# Recreate Postgres and the dev-mode Vault from scratch (drops volumes)
[group('db')]
db-reset:
    docker compose down -v
    docker compose up -d

# Open a psql shell on the control database
[group('db')]
db-shell:
    psql postgres://messgr:messgr@localhost:5432/control

# `transit-fixture`/`transit-fixture-other` are test-only fixtures for
# tests/keystore.rs's generic KeyStore round-trip coverage — never a real
# tenant's mount. Named to NOT start with "transit/" (T-004): Vault refuses
# to mount a secrets engine nested under an already-mounted path, so a bare
# top-level "transit" fixture mount would collide with every real per-tenant
# "transit/<slug>" mount T-004's provisioning creates. Renamed from the
# original "transit"/"transit-other" (T-003) after that exact collision was
# caught live during T-004's implementation.
#
# Provision Vault Transit fixtures used by tests/keystore.rs
[group('db')]
vault-dev-init:
    curl -sf --header "X-Vault-Token: messgr-dev-root-token" --request POST \
        --data '{"type":"transit"}' http://localhost:8200/v1/sys/mounts/transit-fixture || true
    curl -sf --header "X-Vault-Token: messgr-dev-root-token" --request POST \
        http://localhost:8200/v1/transit-fixture/keys/messgr-dek || true
    curl -sf --header "X-Vault-Token: messgr-dev-root-token" --request POST \
        --data '{"type":"transit"}' http://localhost:8200/v1/sys/mounts/transit-fixture-other || true
    curl -sf --header "X-Vault-Token: messgr-dev-root-token" --request POST \
        http://localhost:8200/v1/transit-fixture-other/keys/messgr-dek || true
    curl -sf --header "X-Vault-Token: messgr-dev-root-token" --request POST \
        --data '{"type":"approle"}' http://localhost:8200/v1/sys/auth/approle || true

# Apply pending control-database migrations
[group('control-plane')]
control-migrate:
    cargo run --bin {{bin}} -- migrate

# Provision a tenant (database, migrations, control registration, Vault)
[group('control-plane')]
provision slug region db actor:
    cargo run --bin {{bin}} -- provision --slug {{slug}} --region {{region}} --database-name {{db}} --actor {{actor}}

# Register a producer against a tenant
[group('control-plane')]
producer-register tenant name subj team poc actor:
    cargo run --bin {{bin}} -- producer register --tenant-slug {{tenant}} --name {{name}} \
        --cert-subject {{subj}} --owner-team {{team}} --contact {{poc}} --actor {{actor}}

# Disable a producer
[group('control-plane')]
producer-disable tenant_slug name actor:
    cargo run --bin {{bin}} -- producer disable --tenant-slug {{tenant_slug}} --name {{name}} --actor {{actor}}

# List producers registered for a tenant
[group('control-plane')]
producer-list tenant_slug:
    cargo run --bin {{bin}} -- producer list --tenant-slug {{tenant_slug}}

# Set (create or overwrite) a tenant's typed configuration
[group('control-plane')]
tenant-config-set tenant retention_years default_timezone default_locale quota_day_boundary_tz staleness_max_age_seconds actor:
    cargo run --bin {{bin}} -- tenant-config set --tenant-slug {{tenant}} \
        --retention-years {{retention_years}} --default-timezone {{default_timezone}} \
        --default-locale {{default_locale}} --quota-day-boundary-tz {{quota_day_boundary_tz}} \
        --staleness-max-age-seconds {{staleness_max_age_seconds}} --actor {{actor}}

# Show a tenant's typed configuration
[group('control-plane')]
tenant-config-show tenant_slug:
    cargo run --bin {{bin}} -- tenant-config show --tenant-slug {{tenant_slug}}

# Bootstrap the dev-only internal PKI (Vault-backed)
[group('control-plane')]
dev-pki-bootstrap:
    cargo run --bin {{bin}} -- dev-pki bootstrap

# Issue a dev leaf certificate for mTLS testing
[group('control-plane')]
dev-pki-issue-cert common_name out_dir:
    cargo run --bin {{bin}} -- dev-pki issue-cert --common-name {{common_name}} --out-dir {{out_dir}}

# Validate the AsciiDoc manual via snowball (broken includes/xrefs fail the check)
[group('docs')]
docs-check:
    snowball check

# Render the user manual to PDF + EPUB into dist/docs/ (never committed)
[group('docs')]
docs-build:
    snowball build -o dist/docs
