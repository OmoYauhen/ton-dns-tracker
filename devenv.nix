{ pkgs, ... }:

{
  languages.rust.enable = true;

  packages = [
    pkgs.diesel-cli
    pkgs.pkg-config
    pkgs.openssl
    pkgs.libpq
  ];

  env.PKG_CONFIG_PATH = "${pkgs.openssl.dev}/lib/pkgconfig";
  # pq-sys (used by diesel's postgres feature) looks for this exact var to
  # find libpq.so at link time -- nixpkgs' `libpq` package has no pg_config
  # for it to fall back to, so without this the linker fails with
  # "cannot find -lpq".
  env.PQ_LIB_DIR = "${pkgs.libpq}/lib";

  # Load DATABASE_URL (and anything else) from .env so it's the single
  # source of truth -- NOT hardcoded here, which would silently shadow
  # whatever's in .env (dotenvy, used by diesel_cli and the binaries,
  # never overrides an env var that's already set).
  dotenv.enable = true;

  # `devenv up` starts this (and stops it again on ctrl-c, or via
  # `devenv processes down`). The cluster's data lives under
  # .devenv/state/postgres -- delete that directory for a full reset.
  #
  # Port 5433, not the Postgres default 5432, to avoid colliding with a
  # system-wide Postgres. Must match DATABASE_URL in .env.
  services.postgres = {
    enable = true;
    listen_addresses = "127.0.0.1";
    port = 5433;
    initialDatabases = [{ name = "ton_dns_tracker"; }];
    initialScript = ''
      CREATE ROLE ton_dns_tracker WITH LOGIN SUPERUSER;
    '';
  };

  scripts.db-migrate.exec = "diesel migration run";
  scripts.db-reset.exec = "diesel database reset";

  enterShell = ''
    echo "ton-dns-tracker devenv shell"
    echo "  devenv up                       # start postgres (ctrl-c to stop)"
    echo "  devenv --profile fullstack up   # also start the indexer + ui"
    echo "  db-migrate                      # apply diesel migrations"
    echo "  db-reset                        # drop + recreate the db and re-run migrations"
    echo "  cargo build --workspace"
  '';

  # `devenv --profile fullstack up` starts postgres, then (once it's
  # actually accepting connections, not just started -- `after` without a
  # suffix defaults to waiting on the target's `ready` probe, and
  # services.postgres registers its own pg_isready-based one under the
  # process name "postgres") runs migrations, then (once those finish,
  # `@completed`) the indexer and ui. Plain `devenv up` (no profile) only
  # starts postgres, same as before -- these processes only exist when
  # the profile is active.
  profiles.fullstack.module = {
    processes = {
      migrate = {
        exec = "diesel migration run";
        after = [ "devenv:processes:postgres" ];
      };
      indexer = {
        exec = "cargo run -p ton-dns-indexer";
        after = [ "devenv:processes:migrate@completed" ];
      };
      ui = {
        exec = "cargo run -p ton-dns-ui";
        after = [ "devenv:processes:migrate@completed" ];
      };
    };
  };
}
