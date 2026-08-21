default: run

build:
    cargo build

build-prod:
    cargo build --release

run:
    cargo run

watch:
    cargo watch -x run

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
    # docker compose exec postgres psql -U messgr -d messgr
    psql postgres://messgr:messgr@localhost:5432/messgr

migrate:
    sqlx migrate run

migrate-revert:
    sqlx migrate revert

seed:
    curl -s -X POST http://localhost:8888/send/sms \
        -H "Content-Type: application/json" \
        -d '{"sender":"+15550001234","recipient":"+15559876543","body":"hello from justfile"}' | jq .

list:
    curl -s http://localhost:8888/send/sms | jq .

test-api:
    cd tests/api && npm test

test-perf:
    k6 run tests/performance/sms.js
