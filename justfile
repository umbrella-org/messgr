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

vault-dev-init:
    curl -sf --header "X-Vault-Token: messgr-dev-root-token" --request POST \
        --data '{"type":"transit"}' http://localhost:8200/v1/sys/mounts/transit || true
    curl -sf --header "X-Vault-Token: messgr-dev-root-token" --request POST \
        http://localhost:8200/v1/transit/keys/messgr-dek || true
    curl -sf --header "X-Vault-Token: messgr-dev-root-token" --request POST \
        --data '{"type":"transit"}' http://localhost:8200/v1/sys/mounts/transit-other || true
    curl -sf --header "X-Vault-Token: messgr-dev-root-token" --request POST \
        http://localhost:8200/v1/transit-other/keys/messgr-dek || true
