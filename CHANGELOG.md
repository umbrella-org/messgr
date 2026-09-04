# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
While the version is below `1.0.0`, breaking changes may land in a minor release.

## [Unreleased]

## [0.1.0] - 2026-09-04

Initial release. Single-tenant control plane, ledger/outbox pipeline, and first
SMS-channel dispatcher.

### Added

- Control database with tenant registry and single-tenant provisioning, including
  the `platform_audit` trail (T-001, T-002).
- Vault Transit integration: `KeyStore` trait, Transit client, dev-mode Vault in
  `compose.yml`, per-tenant Transit mount + AppRole creation wired into provisioning
  (T-003, T-004).
- Producer registry (register/disable) and mTLS-based identity resolution from
  client cert to producer/tenant identity (T-005, T-006).
- Typed `tenant_config` loading (T-007).
- Per-customer DEK lifecycle with LRU cache, pre-provisioning, and HMAC pepper
  (T-008).
- Ledger + outbox schema: `comms_request`, `outbox`, `comms_event`, idempotency
  (T-009).
- Immutable, versioned template store with approval metadata and a render path
  (T-010).
- `messgr-ingest`: `POST /comms` with idempotency replay, ledger + outbox write,
  and per-customer encryption (T-011).
- `Sender` trait, first SMS provider adapter, and `provider_config` (T-012).
- Minimal dispatcher: per-channel claim loop, LISTEN/NOTIFY wakeup, `comms_event`
  writes, and outbox lease lifecycle with retry/backoff (T-013, T-021).
- Ledger partition lifecycle: create-ahead, move to slow tablespace, detach + drop
  (T-014).
- Customer projection with resolution at ingest (T-015).
- Kill switches (T-016).
- `messgr-control stats` subcommand for per-tenant message volume (T-028).
- Cross-compiled release builds (darwin/linux × amd64/arm64) with GitHub release
  archives and a Homebrew formula published to `umbrella-org/homebrew-tap`.

### Fixed

- Two races on `customer_address`'s `(kind, value_hmac)` index that could orphan
  `customer_dek` rows or reject legitimate address-conflict sends (T-018).

[Unreleased]: https://github.com/umbrella-org/messgr/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/umbrella-org/messgr/releases/tag/v0.1.0
