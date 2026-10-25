# ton-dns-tracker

Indexer + web UI for `.ton` DNS domains. Syncs domain NFT state from
[toncenter](https://toncenter.com/) into Postgres (`crates/indexer`), and
serves it over a small Axum/Askama web UI (`crates/ui`). Shared
decoding/model code lives in `crates/core`.

## Prerequisites

- [Nix](https://nixos.org/download) with flakes enabled
- [devenv](https://devenv.sh/getting-started/#2-installing-devenv) (recommended -- manages Postgres for you)
- Optionally [direnv](https://direnv.net/) to auto-load the shell on `cd`

## Quickstart (devenv)

[devenv](https://devenv.sh) runs a disposable, project-local Postgres
cluster plus the Rust toolchain and `diesel-cli`, so there's nothing to
install or configure by hand. `devenv.nix` loads `DATABASE_URL` (and
everything else) straight from `.env` -- so create that file *before*
entering the shell:

```sh
cp .env.example .env     # defaults already match the devenv postgres instance

# one-time: load the environment (or run `direnv allow` if you use direnv)
devenv shell

# start Postgres -- foreground, logs to the terminal, ctrl-c to stop
devenv up
```

In another terminal (same `devenv shell`):

```sh
db-migrate                # alias for `diesel migration run`
cargo build --workspace
cargo run -p ton-dns-indexer
cargo run -p ton-dns-ui
```

If you edit `.env` after already entering the shell, re-run `devenv shell`
(or let direnv reload) to pick up the change -- env vars are only read once,
at shell entry.

### Running everything with one command

`devenv up` only starts Postgres. To also start the indexer and UI, use the
`fullstack` profile instead:

```sh
devenv --profile fullstack up
```

This starts Postgres, waits for it to actually accept connections, runs
migrations, then starts `ton-dns-indexer` and `ton-dns-ui` -- all in one
`process-compose` session (ctrl-c stops everything). Plain `devenv up`
(no profile) is unaffected and still only starts Postgres, for when you
want to run the binaries yourself (e.g. under a debugger).

### Stopping / cleaning up Postgres

- Foreground: `ctrl-c` the `devenv up` process.
- Detached: `devenv up -d` to start in the background, `devenv processes down` to stop.
- Full reset (wipes all data): stop the service, then `rm -rf .devenv/state/postgres`.
- Reset just the schema/data, keep the cluster running: `db-reset` (alias for `diesel database reset`).

Postgres config lives in [`devenv.nix`](./devenv.nix) (`services.postgres`) --
it provisions the `ton_dns_tracker` role and database on first start so the
defaults in `.env.example` work unmodified.

### Port already in use

The project defaults to port **5433** (not Postgres's usual 5432)
specifically to avoid colliding with a system-wide Postgres you might
already have running. If 5433 is also taken, pick another free port and
update it in *both* places -- they're independent config files and have to
agree:

- `services.postgres.port` in [`devenv.nix`](./devenv.nix)
- `DATABASE_URL` in `.env`

## Alternative: plain `nix develop`

If you don't want devenv's process/service management, `flake.nix` provides
a plain dev shell with the Rust toolchain, `diesel-cli`, and the Postgres
binaries (no managed service -- you run `initdb`/`pg_ctl` yourself):

```sh
nix develop
cp .env.example .env   # edit DATABASE_URL to match whatever Postgres you run
diesel migration run
```

## Environment variables

See [`.env.example`](./.env.example):

| Variable | Description |
| --- | --- |
| `DATABASE_URL` | Postgres connection string used by `diesel_cli` and both binaries |
| `TONCENTER_API_KEY` | API key for [toncenter.com](https://toncenter.com/) |
| `TONCENTER_BASE_URL` | toncenter API base URL |
| `TONCENTER_RATE_LIMIT_RPS` | Client-side rate limit for toncenter requests |
| `DNS_COLLECTION_ADDRESS` | `.ton` DNS collection NFT contract address, aka "Root DNS address" / config param #4 (indexer only; temporary until config-param lookup is implemented; default in `.env.example` is the real mainnet address) |
| `BIND_ADDR` | Address the `ton-dns-ui` server binds to |

## Layout

```
crates/core/     shared models, cell decoding, toncenter client
crates/indexer/  syncs domain state into Postgres
crates/ui/       Axum + Askama web UI
migrations/      diesel migrations
```

## License

[MIT](./LICENSE)
