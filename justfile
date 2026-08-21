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
