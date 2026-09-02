---
id: T-023
title: Read provider credentials from Vault instead of environment variables
project: messgr
depends-on: []
spawned-by: []
impact: high
complexity: medium
cost: M
---

# T-023 — Read provider credentials from Vault instead of environment variables

## Outcome

`messgr-dispatcher` resolves each channel's provider credential from `provider_config.credential_path` against Vault, matching §13's "all secrets come from Vault — none in config files, none in environment variables". `DISPATCHER_<CH>_API_KEY` environment variables are no longer read anywhere, and `provider_config` (shipped in T-012, unread since) has a first real reader.

## Description

`src/bin/dispatcher.rs` currently builds each channel's `HttpSender` from `env_var("DISPATCHER_{CH}_API_KEY")`. This was an explicit, acknowledged deferral at the time — `src/sender/http.rs::HttpSender::new`'s own doc comment says so, citing "T-012 decision 3" — not a silent regression, but it is still the one place the shipped system violates §13's secrets rule, and `provider_config` (channel, priority, provider, `credential_path`, rate limit) has existed since T-012 with nothing reading `credential_path` at all.

Scope:

1. `messgr-dispatcher` startup queries `provider_config` for the tenant's channel/priority list (the "ordered list even at length 1" §12.1 already requires) and resolves each entry's `credential_path` via the tenant's `KeyStore`/Vault mount (`src/keystore.rs`, wired since T-004/T-008), rather than an env var per channel.
2. `HttpSender::new`'s signature stays the same (`api_key: String`) — the caller now sources that string from Vault instead of the environment; no change needed to the `Sender` trait itself.
3. Remove the `DISPATCHER_<CH>_API_KEY` environment-variable path from `src/bin/dispatcher.rs` once the Vault path is proven; keep the direct-string constructor for tests (already used throughout `src/sender/http.rs`'s test module with a literal `"test-key"`).
4. This is also the first real use of `provider_config.priority`-ordered failover config outside a migration — confirm at refinement whether wiring actual failover-on-priority belongs in this ticket or is out of scope pending Still-open #4 (provider selection).

## Implementation Plan

<!-- empty until refined; must meet the READY gate before moving to 2-ready/ -->

## Review

<!-- empty until IN REVIEW -->

## Implementation Plan

<!-- empty until refined; must meet the READY gate before moving to 2-ready/ -->

## Review

<!-- empty until IN REVIEW -->

## History

- 2026-09-02 — created (TO DO). source: audit: design/implementation audit found messgr-dispatcher reads DISPATCHER_<CH>_API_KEY from the environment, violating §13's Vault-only secrets rule; provider_config.credential_path has had no reader since T-012.
