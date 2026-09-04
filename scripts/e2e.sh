#!/usr/bin/env bash
# End-to-end walkthrough driver: destroy/setup the local dev environment,
# send a message through messgr-ingest, and trace it through the DB.
# Each numbered step is a standalone function -- run them individually:
#   ./scripts/e2e.sh 1-destroy-env
#   ./scripts/e2e.sh 2-setup-env
#   ./scripts/e2e.sh 3-send-sms
#   ./scripts/e2e.sh 4-send-email
#   ./scripts/e2e.sh 5-trace-sms-in-db
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

WORK_DIR="$ROOT_DIR/.e2e"
CERT_DIR="$WORK_DIR/certs"
STATE_FILE="$WORK_DIR/state.env"

TENANT_SLUG="acme"
TENANT_REGION="eu"
TENANT_DB="tenant_acme"
ACTOR="operator@example.com"
PRODUCER_NAME="fraud-alerts"
PRODUCER_CN="fraud-alerts.internal"
PRODUCER_SUBJECT="CN=${PRODUCER_CN}"
INGEST_HOST="messgr-ingest.internal"
TEMPLATE_ID="balance-alert"
TEMPLATE_LOCALE="en-GB"
SMS_TEMPLATE_VERSION=1
EMAIL_TEMPLATE_VERSION=2
SMS_PROVIDER_PORT=19091
EMAIL_PROVIDER_PORT=19092

log() { echo "[e2e] $*"; }

require_state() {
    if [[ ! -f "$STATE_FILE" ]]; then
        echo "[e2e] no $STATE_FILE -- run 2-setup-env first" >&2
        exit 1
    fi
    # shellcheck disable=SC1090
    source "$STATE_FILE"
}

state_set() {
    # state_set KEY VALUE -- upsert one line in STATE_FILE
    local key="$1" value="$2"
    touch "$STATE_FILE"
    grep -v "^${key}=" "$STATE_FILE" > "$STATE_FILE.tmp" || true
    mv "$STATE_FILE.tmp" "$STATE_FILE"
    echo "${key}=${value}" >> "$STATE_FILE"
}

customer_id_for_e2e() {
    # A destination (phone/email) binds permanently to the first customer_id
    # that sends to it (DESIGN.md Sec.5 -- consent keys on customer_address.id,
    # which is append-only). This fixture always sends to the same two
    # destinations, so re-run it as the same customer instead of minting a
    # fresh one per call, which would conflict on the 2nd send onward.
    require_state
    if [[ -z "${CUSTOMER_ID:-}" ]]; then
        CUSTOMER_ID="$(uuidgen | tr '[:upper:]' '[:lower:]')"
        state_set CUSTOMER_ID "$CUSTOMER_ID"
    fi
    echo "$CUSTOMER_ID"
}

kill_pidfile() {
    local pidfile="$1"
    if [[ -f "$pidfile" ]]; then
        local pid
        pid="$(cat "$pidfile")"
        if kill -0 "$pid" 2>/dev/null; then
            log "stopping pid $pid ($(basename "$pidfile"))"
            kill "$pid" 2>/dev/null || true
            wait "$pid" 2>/dev/null || true
        fi
        rm -f "$pidfile"
    fi
}

wait_for_log() {
    # wait_for_log LOGFILE PATTERN TIMEOUT_SECONDS
    local logfile="$1" pattern="$2" timeout="$3" waited=0
    while [[ $waited -lt $timeout ]]; do
        if [[ -f "$logfile" ]] && grep -q "$pattern" "$logfile" 2>/dev/null; then
            return 0
        fi
        sleep 1
        waited=$((waited + 1))
    done
    echo "[e2e] timed out waiting for '$pattern' in $logfile" >&2
    tail -n 40 "$logfile" >&2 || true
    return 1
}

start_mock_provider() {
    # start_mock_provider PORT LOGFILE PIDFILE -- tiny HTTP stub matching
    # HttpSender's contract: POST /messages -> 200 {message_id, status}
    local port="$1" logfile="$2" pidfile="$3"
    python3 -c '
import json, sys
from http.server import BaseHTTPRequestHandler, HTTPServer

port = int(sys.argv[1])

class Handler(BaseHTTPRequestHandler):
    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        self.rfile.read(length)
        body = json.dumps({"message_id": "mock-" + self.path.strip("/"), "status": "queued"}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, fmt, *args):
        sys.stderr.write("%s - %s\n" % (self.address_string(), fmt % args))

HTTPServer(("127.0.0.1", port), Handler).serve_forever()
' "$port" > "$logfile" 2>&1 &
    echo $! > "$pidfile"
}

step_1_destroy_env() {
    log "stopping background processes"
    kill_pidfile "$WORK_DIR/ingest.pid"
    kill_pidfile "$WORK_DIR/dispatcher.pid"
    kill_pidfile "$WORK_DIR/mock-sms.pid"
    kill_pidfile "$WORK_DIR/mock-email.pid"

    log "tearing down docker compose (postgres + vault), dropping volumes"
    docker compose down -v

    log "removing $WORK_DIR (certs, pids, logs, state)"
    rm -rf "$WORK_DIR"
    log "destroy-env done"
}

step_2_setup_env() {
    mkdir -p "$WORK_DIR" "$CERT_DIR"

    if [[ ! -f .env ]]; then
        log "no .env -- copying from .env.example"
        cp .env.example .env
    fi

    log "building binaries once (debug)"
    cargo build --bin messgr-control --bin messgr-ingest --bin messgr-dispatcher

    log "starting postgres + dev-mode vault"
    docker compose up -d

    log "waiting for vault"
    local waited=0
    until curl -sf http://localhost:8200/v1/sys/health >/dev/null 2>&1; do
        waited=$((waited + 1))
        if [[ $waited -ge 60 ]]; then
            echo "[e2e] vault never became healthy" >&2
            exit 1
        fi
        sleep 1
    done

    log "waiting for postgres"
    waited=0
    until docker compose exec -T postgres pg_isready -U messgr >/dev/null 2>&1; do
        waited=$((waited + 1))
        if [[ $waited -ge 60 ]]; then
            echo "[e2e] postgres never became ready" >&2
            exit 1
        fi
        sleep 1
    done

    log "vault-dev-init"
    just vault-dev-init

    log "control-migrate"
    just control-migrate

    log "provisioning tenant '$TENANT_SLUG'"
    local provision_out
    provision_out="$(just provision "$TENANT_SLUG" "$TENANT_REGION" "$TENANT_DB" "$ACTOR")"
    echo "$provision_out"
    local tenant_id vault_role_id vault_wrapped_secret_id
    tenant_id="$(echo "$provision_out" | head -n1)"
    vault_role_id="$(echo "$provision_out" | grep -o 'vault_role_id=[^ ]*' | cut -d= -f2)"
    vault_wrapped_secret_id="$(echo "$provision_out" | grep -o 'vault_wrapped_secret_id=[^ ]*' | cut -d= -f2)"
    if [[ -z "$vault_wrapped_secret_id" ]]; then
        echo "[e2e] no vault_wrapped_secret_id in provision output -- tenant already existed (run 1-destroy-env first)" >&2
        exit 1
    fi
    state_set TENANT_ID "$tenant_id"
    state_set VAULT_ROLE_ID "$vault_role_id"
    state_set VAULT_WRAPPED_SECRET_ID "$vault_wrapped_secret_id"

    log "tenant-config set"
    just tenant-config-set "$TENANT_SLUG" 7 Europe/London "$TEMPLATE_LOCALE" Europe/London "$ACTOR"

    log "registering producer '$PRODUCER_NAME'"
    just producer-register "$TENANT_SLUG" "$PRODUCER_NAME" "$PRODUCER_SUBJECT" fraud fraud-oncall@example.com "$ACTOR"

    log "writing provider credentials to Vault KV (secret/data/$TENANT_SLUG/{sms,email})"
    curl -sf --header "X-Vault-Token: messgr-dev-root-token" --request POST \
        --data '{"data":{"api_key":"dev-key"}}' \
        "http://localhost:8200/v1/secret/data/$TENANT_SLUG/sms"
    curl -sf --header "X-Vault-Token: messgr-dev-root-token" --request POST \
        --data '{"data":{"api_key":"dev-key"}}' \
        "http://localhost:8200/v1/secret/data/$TENANT_SLUG/email"

    log "provider-config: sms -> localhost:$SMS_PROVIDER_PORT, email -> localhost:$EMAIL_PROVIDER_PORT"
    just provider-config-set "$TENANT_SLUG" sms 1 generic-http "secret/data/$TENANT_SLUG/sms" 10 "$ACTOR"
    just provider-config-set "$TENANT_SLUG" email 1 generic-http "secret/data/$TENANT_SLUG/email" 10 "$ACTOR"

    log "approving templates"
    printf 'Hi {{name}}, your balance is {{balance}}.' > "$WORK_DIR/template-body.txt"
    just template-approve "$TENANT_SLUG" "$TEMPLATE_ID" "$SMS_TEMPLATE_VERSION" sms "$TEMPLATE_LOCALE" "$WORK_DIR/template-body.txt" "$ACTOR"
    just template-approve "$TENANT_SLUG" "$TEMPLATE_ID" "$EMAIL_TEMPLATE_VERSION" email "$TEMPLATE_LOCALE" "$WORK_DIR/template-body.txt" "$ACTOR"

    log "dev PKI: bootstrap + issue producer client cert + ingest server cert"
    just dev-pki-bootstrap
    just dev-pki-issue-cert "$PRODUCER_CN" "$CERT_DIR/producer"
    just dev-pki-issue-server-cert "$INGEST_HOST" "$CERT_DIR/ingest"

    log "starting mock providers on :$SMS_PROVIDER_PORT (sms) and :$EMAIL_PROVIDER_PORT (email)"
    start_mock_provider "$SMS_PROVIDER_PORT" "$WORK_DIR/mock-sms.log" "$WORK_DIR/mock-sms.pid"
    start_mock_provider "$EMAIL_PROVIDER_PORT" "$WORK_DIR/mock-email.log" "$WORK_DIR/mock-email.pid"

    log "starting messgr-ingest"
    INGEST_TLS_CERT_FILE="$CERT_DIR/ingest/cert.pem" \
    INGEST_TLS_KEY_FILE="$CERT_DIR/ingest/key.pem" \
    INGEST_TLS_CLIENT_CA_FILE="$CERT_DIR/ingest/ca.pem" \
        ./target/debug/messgr-ingest > "$WORK_DIR/ingest.log" 2>&1 &
    echo $! > "$WORK_DIR/ingest.pid"
    wait_for_log "$WORK_DIR/ingest.log" "messgr-ingest listening" 30

    log "starting messgr-dispatcher (channels: sms,email)"
    DISPATCHER_TENANT_SLUG="$TENANT_SLUG" \
    DISPATCHER_CHANNELS="sms,email" \
    DISPATCHER_SMS_BASE_URL="http://localhost:$SMS_PROVIDER_PORT" \
    DISPATCHER_EMAIL_BASE_URL="http://localhost:$EMAIL_PROVIDER_PORT" \
    VAULT_ROLE_ID="$vault_role_id" \
    VAULT_WRAPPED_SECRET_ID="$vault_wrapped_secret_id" \
        ./target/debug/messgr-dispatcher > "$WORK_DIR/dispatcher.log" 2>&1 &
    echo $! > "$WORK_DIR/dispatcher.pid"
    wait_for_log "$WORK_DIR/dispatcher.log" "starting claim loop" 30

    log "setup-env done -- tenant_id=$tenant_id"
}

MAX_CONCURRENT_SENDS=50

send_comms() {
    # send_comms CHANNEL DESTINATION TEMPLATE_VERSION [CUSTOMER_ID] -> prints comms_request_id
    # CUSTOMER_ID defaults to a fresh uuid; pass the same one across calls
    # sharing a DESTINATION, since a destination binds to one customer_id
    # (DESIGN.md Sec.5 -- consent keys on customer_address.id).
    local channel="$1" destination="$2" template_version="$3"
    local customer_id="${4:-}"
    if [[ -z "$customer_id" ]]; then
        customer_id="$(uuidgen | tr '[:upper:]' '[:lower:]')"
    fi
    # uuidgen (not $$/date) keeps this unique across concurrent subshells,
    # which all share the parent's $$ under bash.
    local idempotency_key="e2e-${channel}-$(uuidgen | tr '[:upper:]' '[:lower:]')"

    local response
    response="$(curl -sk --fail-with-body \
        --cert "$CERT_DIR/producer/cert.pem" --key "$CERT_DIR/producer/key.pem" \
        --cacert "$CERT_DIR/ingest/ca.pem" \
        --resolve "${INGEST_HOST}:8443:127.0.0.1" \
        -H "Idempotency-Key: $idempotency_key" -H "Content-Type: application/json" \
        -d "{\"customer_id\":\"$customer_id\",\"destination\":\"$destination\",\"channel\":\"$channel\",\"class\":\"transactional\",\"template_id\":\"$TEMPLATE_ID\",\"template_version\":$template_version,\"variables\":{\"name\":\"Jordan\",\"balance\":\"100.00\"}}" \
        "https://${INGEST_HOST}:8443/comms")"

    echo "$response" >&2
    echo "$response" | grep -o '"comms_request_id":"[^"]*"' | cut -d'"' -f4
}

require_count() {
    # require_count VALUE -> positive integer, defaulting empty to 1
    local value="${1:-1}"
    if ! [[ "$value" =~ ^[1-9][0-9]*$ ]]; then
        echo "[e2e] count must be a positive integer, got '$value'" >&2
        exit 1
    fi
    echo "$value"
}

bulk_send() {
    # bulk_send CHANNEL DESTINATION TEMPLATE_VERSION COUNT STATE_KEY -- fires
    # COUNT sends concurrently (batched at MAX_CONCURRENT_SENDS) and stores
    # the last successful comms_request_id under STATE_KEY.
    local channel="$1" destination="$2" template_version="$3" count="$4" state_key="$5"
    log "sending $count $channel message(s) via messgr-ingest asynchronously (up to $MAX_CONCURRENT_SENDS at a time)"

    local customer_id
    customer_id="$(customer_id_for_e2e)"

    local result_dir
    result_dir="$(mktemp -d "$WORK_DIR/bulk-${channel}.XXXXXX")"

    local i=1 batch_end j
    while [[ $i -le $count ]]; do
        batch_end=$((i + MAX_CONCURRENT_SENDS - 1))
        [[ $batch_end -gt $count ]] && batch_end=$count

        for ((j = i; j <= batch_end; j++)); do
            (
                local id
                if id="$(send_comms "$channel" "$destination" "$template_version" "$customer_id" 2>"$result_dir/$j.err")"; then
                    echo "$id" > "$result_dir/$j.ok"
                fi
            ) &
        done
        wait

        log "  ...$batch_end/$count sent"
        i=$((batch_end + 1))
    done

    local ok_count last_id
    ok_count="$(find "$result_dir" -name '*.ok' | wc -l | tr -d ' ')"
    last_id="$(find "$result_dir" -name '*.ok' -exec cat {} \; | tail -n1)"
    if [[ -n "$last_id" ]]; then
        state_set "$state_key" "$last_id"
    fi
    log "$channel bulk send done: $ok_count/$count succeeded"
    if [[ "$ok_count" != "$count" ]]; then
        log "failures logged under $result_dir (*.err) -- not cleaning up"
    else
        rm -rf "$result_dir"
    fi
}

step_3_send_sms() {
    require_state
    local count
    count="$(require_count "${1:-1}")"
    if [[ "$count" -eq 1 ]]; then
        log "sending sms via messgr-ingest"
        local comms_request_id
        comms_request_id="$(send_comms sms "+15550100" "$SMS_TEMPLATE_VERSION" "$(customer_id_for_e2e)")"
        state_set LAST_SMS_REQUEST_ID "$comms_request_id"
        log "comms_request_id=$comms_request_id"
    else
        bulk_send sms "+15550100" "$SMS_TEMPLATE_VERSION" "$count" LAST_SMS_REQUEST_ID
    fi
}

step_4_send_email() {
    require_state
    local count
    count="$(require_count "${1:-1}")"
    if [[ "$count" -eq 1 ]]; then
        log "sending email via messgr-ingest"
        local comms_request_id
        comms_request_id="$(send_comms email "jordan@example.com" "$EMAIL_TEMPLATE_VERSION" "$(customer_id_for_e2e)")"
        state_set LAST_EMAIL_REQUEST_ID "$comms_request_id"
        log "comms_request_id=$comms_request_id"
    else
        bulk_send email "jordan@example.com" "$EMAIL_TEMPLATE_VERSION" "$count" LAST_EMAIL_REQUEST_ID
    fi
}

tenant_psql() {
    # tenant_psql SQL -- via `docker compose exec`, not host->container
    # port-forwarding, which has proven flaky in some Docker Desktop setups
    docker compose exec -T postgres psql -U messgr -d "$TENANT_DB" -c "$1"
}

step_5_trace_sms_in_db() {
    require_state
    local id="${LAST_SMS_REQUEST_ID:-}"

    if [[ -z "$id" ]]; then
        log "no LAST_SMS_REQUEST_ID in state -- tracing the most recent sms comms_request instead"
        id="$(docker compose exec -T postgres psql -U messgr -d "$TENANT_DB" -Atc \
            "SELECT id FROM comms_request WHERE channel = 'sms' ORDER BY created_at DESC LIMIT 1")"
        id="$(echo "$id" | tr -d '\r')"
        if [[ -z "$id" ]]; then
            echo "[e2e] no sms comms_request rows found -- run 3-send-sms first" >&2
            exit 1
        fi
    fi

    log "comms_request_id=$id"
    echo "--- comms_request ---"
    tenant_psql "SELECT id, created_at, channel, class, template_id, template_version, final_status, finalized_at
                 FROM comms_request WHERE id = '$id'"

    echo "--- outbox (present while still in flight) ---"
    tenant_psql "SELECT comms_request_id, channel, attempts, next_attempt_at, leased_until
                 FROM outbox WHERE comms_request_id = '$id'"

    echo "--- comms_event ---"
    tenant_psql "SELECT event_type, occurred_at, provider_ref, provider_status
                 FROM comms_event WHERE comms_request_id = '$id' ORDER BY occurred_at"
}

usage() {
    cat >&2 <<EOF
Usage: $0 <step> [count]

Steps:
  1-destroy-env      stop background processes, docker compose down -v, wipe .e2e/
  2-setup-env        full local env: db+vault, provision tenant, certs, start ingest+dispatcher+mock providers
  3-send-sms [n]     POST /comms for channel=sms. n>1 fires n sends asynchronously (default 1)
  4-send-email [n]   POST /comms for channel=email. n>1 fires n sends asynchronously (default 1)
  5-trace-sms-in-db  print the ledger/outbox/event rows for the last sms send
EOF
    exit 1
}

case "${1:-}" in
    1-destroy-env)     step_1_destroy_env ;;
    2-setup-env)       step_2_setup_env ;;
    3-send-sms)        step_3_send_sms "${2:-1}" ;;
    4-send-email)      step_4_send_email "${2:-1}" ;;
    5-trace-sms-in-db) step_5_trace_sms_in_db ;;
    *)                 usage ;;
esac
