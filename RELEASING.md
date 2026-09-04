# Releasing messgr

`messgr` is distributed as cross-compiled static-ish binaries via a hand-rolled GitHub
Actions workflow, a GitHub release per tag, and a Homebrew formula published to messgr's own
tap (`github.com/umbrella-org/homebrew-tap` → `brew install umbrella-org/tap/messgr`).

> This process mirrors `pickle`'s / morty's / summer's `RELEASING.md` in shape — CHANGELOG
> discipline, a tag-driven release, a decoupled docs-attach step, a Homebrew tap push — with
> three deliberate differences:
>
> 1. **No goreleaser.** messgr is Rust, not Go; `release.yml` cross-compiles with `cargo
>    build --target ...` directly and hand-renders the Homebrew formula instead of using
>    goreleaser's `brews:` block. `cargo-dist` (the closest Rust equivalent) is not wired up —
>    revisit if the build matrix grows past four platforms or the formula-rendering script
>    gets unwieldy.
> 2. **The tap is public; the repo it points at is not.** `umbrella-org/homebrew-tap` is
>    public. Its `messgr.rb` formula will therefore publicly name `umbrella-org/messgr` and
>    its release tags, even though the repo itself and its release assets stay private —
>    `brew install umbrella-org/tap/messgr` still needs a `HOMEBREW_GITHUB_API_TOKEN` with
>    read access to `umbrella-org/messgr` to actually download an archive. Worth a second
>    look before the first tag if that metadata leak (repo name + version history, not
>    content) is unacceptable for this project.
> 3. **A dedicated tap, not the shared one.** Unlike pickle/morty/summer, which all publish
>    to the shared `codcod/homebrew-tap`, messgr publishes to its own
>    `umbrella-org/homebrew-tap` — a separate repo and a separate token
>    (`HOMEBREW_TAP_UMBRELLA_ORG_GITHUB_TOKEN`), scoped to this org rather than mixed in with
>    unrelated projects' formulas.
>
> Unlike morty/summer, this workflow does **not** re-run the test suite — `ci.yml` already
> gates every push to `main` (with live Postgres + Vault services); re-running the same suite
> once per build-matrix leg on the tagged commit would be pure duplication.

## Cutting a release

Update [`CHANGELOG.md`](CHANGELOG.md): retitle the `[Unreleased]` section to
`[X.Y.Z] - YYYY-MM-DD`, add a fresh empty `[Unreleased]` above it, update the link
references at the bottom, and commit — the tag should include the changelog. Reconcile it by
hand against the T-series tickets that shipped since the last tag (`tickets/6-done/`) —
messgr has no `pickle changelog check` equivalent.

Bump the version in [`Cargo.toml`](Cargo.toml) to match the tag before tagging — the release
workflow reads the version from the pushed tag, not from `Cargo.toml`, but a mismatch between
the two is confusing and worth avoiding.

Then everything is tag-driven — the [`release`](.github/workflows/release.yml) workflow runs
on any `v*` tag:

```sh
git tag v0.1.0
git push origin v0.1.0
```

The workflow can also be re-run manually for an **existing** tag via *Actions → release →
Run workflow* (`workflow_dispatch` with the tag name), for example after fixing a secret.
Every publish step (`gh release create/upload --clobber`, the Homebrew formula commit) is
safe to re-run: assets are overwritten rather than rejected with `422 already_exists`, and an
unchanged formula is a no-op commit.

That produces, for `darwin`/`linux` × `amd64`/`arm64`:

- a **GitHub release** with `.tar.gz` archives (one per platform, containing every binary the
  workspace currently defines — just `messgr-control` today, growing as `messgr-ingest`,
  `messgr-dispatcher`, etc. land) + `checksums.txt`;
- an updated **Homebrew formula** committed to `umbrella-org/homebrew-tap`;
- once that release run **succeeds**, [`docs-release.yml`](.github/workflows/docs-release.yml)
  runs next, builds the AsciiDoc user manual with `snowball`, and attaches the PDF/EPUB to
  the same release — soft-failing (a broken manual never unpublishes or blocks the release).
  It triggers on `release.yml` **completing** (`workflow_run`), not on the `release:
  published` event the release job raises: that event is created with the default
  `secrets.GITHUB_TOKEN`, and GitHub does not chain further workflow runs off events raised
  by that token (confirmed live on morty — this is the same reasoning, not re-derived here).
  `docs-release.yml` also runs on `macos-latest`, not `ubuntu-latest`, because it needs a
  preinstalled Homebrew.

No compile-time `sqlx::query!`/`query_as!` macros are used in this codebase, so cross-compiling
needs no live database at build time.

## Validating locally (no publish)

```sh
just build-prod                                        # native-target release build
cargo build --release --target aarch64-apple-darwin     # or any other target, if the toolchain
                                                          # has it installed (rustup target add)
```

There is no `goreleaser check`/`dist-snapshot` equivalent yet — the workflow has not been
exercised end-to-end. Treat the first real tag as the first real test of the pipeline, and
expect to iterate on `release.yml`/the formula-rendering step the way morty's `RELEASING.md`
records `mode: replace` + `replace_existing_artifacts` being found live during `v0.1.0`.

## One-time setup this depends on — NOT YET CONFIRMED

- **`HOMEBREW_TAP_UMBRELLA_ORG_GITHUB_TOKEN`** — a repository secret on `umbrella-org/messgr`:
  a PAT with `repo` scope on `umbrella-org/homebrew-tap`. Not yet confirmed present on this
  repo.
- Publishing `messgr` to its own `umbrella-org/homebrew-tap`, under a
  `umbrella-org/messgr`-sourced formula, is not yet confirmed wanted at the ownership level —
  see the metadata-leak note above.
- Consumers need `HOMEBREW_GITHUB_API_TOKEN` (read access to `umbrella-org/messgr`) set
  locally for `brew install umbrella-org/tap/messgr` to actually download the archive, since
  the formula's `url`/`sha256` point at a private repo's release assets.
- **`GITHUB_TOKEN`** — provided automatically by Actions; `release.yml` grants it
  `contents: write` to create the release and upload assets.
