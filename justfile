default: build

build:
    cargo build

build-prod:
    cargo build --release

test:
    cargo test

fmt:
    cargo fmt

lint:
    cargo clippy -- -D warnings

check:
    cargo check

clean:
    cargo clean

db-up:
    docker compose up -d

db-down:
    docker compose down

db-reset:
    docker compose down -v
    docker compose up -d

db-shell:
    psql postgres://messgr:messgr@localhost:5432/control

control-migrate:
    cargo run --bin messgr-control -- migrate

provision slug region db actor:
    cargo run --bin messgr-control -- provision --slug {{slug}} --region {{region}} --database-name {{db}} --actor {{actor}}

producer-register tenant_slug name cert_subject owner_team contact actor:
    cargo run --bin messgr-control -- producer register --tenant-slug {{tenant_slug}} --name {{name}} \
        --cert-subject {{cert_subject}} --owner-team {{owner_team}} --contact {{contact}} --actor {{actor}}

producer-disable tenant_slug name actor:
    cargo run --bin messgr-control -- producer disable --tenant-slug {{tenant_slug}} --name {{name}} --actor {{actor}}

producer-list tenant_slug:
    cargo run --bin messgr-control -- producer list --tenant-slug {{tenant_slug}}

dev-pki-bootstrap:
    cargo run --bin messgr-control -- dev-pki bootstrap

dev-pki-issue-cert common_name out_dir:
    cargo run --bin messgr-control -- dev-pki issue-cert --common-name {{common_name}} --out-dir {{out_dir}}

# `transit-fixture`/`transit-fixture-other` are test-only fixtures for
# tests/keystore.rs's generic KeyStore round-trip coverage — never a real
# tenant's mount. Named to NOT start with "transit/" (T-004): Vault refuses
# to mount a secrets engine nested under an already-mounted path, so a bare
# top-level "transit" fixture mount would collide with every real per-tenant
# "transit/<slug>" mount T-004's provisioning creates. Renamed from the
# original "transit"/"transit-other" (T-003) after that exact collision was
# caught live during T-004's implementation.
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

# Validate the AsciiDoc manual via snowball (broken includes/xrefs fail the check)
docs-check:
    snowball check

# Render the user manual to PDF + EPUB into dist/docs/ (never committed)
docs-build:
    snowball build -o dist/docs
