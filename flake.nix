{
  description = "ton-dns-tracker: indexer + web UI for .ton DNS domains";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay.url = "github:oxalica/rust-overlay";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, rust-overlay, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ rust-overlay.overlays.default ];
        };

        rustToolchain = pkgs.rust-bin.stable.latest.default.override {
          extensions = [ "rust-src" "rust-analyzer" ];
        };
      in
      {
        devShells.default = pkgs.mkShell {
          packages = [
            rustToolchain
            pkgs.diesel-cli
            pkgs.postgresql_16
            pkgs.pkg-config
            pkgs.openssl
            pkgs.libpq
          ];

          # diesel_cli and anything linking against libpq needs these at build time.
          PKG_CONFIG_PATH = "${pkgs.openssl.dev}/lib/pkgconfig";
          # pq-sys (diesel's postgres feature) looks for this exact var name
          # to find libpq.so at link time -- nixpkgs' `libpq` has no
          # pg_config for it to fall back to otherwise.
          PQ_LIB_DIR = "${pkgs.libpq}/lib";

          shellHook = ''
            echo "ton-dns-tracker dev shell"
            echo "  cargo build --workspace     # build indexer + ui"
            echo "  diesel migration run        # apply migrations (needs DATABASE_URL)"
            echo "  cp .env.example .env        # then edit DATABASE_URL / TONCENTER_API_KEY"
          '';
        };
      });
}
