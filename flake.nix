{
  description = "Maki - AI coding agent";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    crane.url = "github:ipetkov/crane";
    rust-overlay.url = "github:oxalica/rust-overlay";
    rust-overlay.inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs =
    {
      nixpkgs,
      crane,
      rust-overlay,
      ...
    }:
    let
      lib = nixpkgs.lib;
      cargoToml = fromTOML (builtins.readFile ./Cargo.toml);
      packageName = cargoToml.package.name;
      version = cargoToml.workspace.package.version;
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];
      forEachSystem =
        f:
        lib.genAttrs systems (
          system:
          f system (
            import nixpkgs {
              inherit system;
              overlays = [ (import rust-overlay) ];
            }
          )
        );

      mkCraneLib =
        pkgs: pickToolchain:
        (crane.mkLib pkgs).overrideToolchain (pickToolchain pkgs.rust-bin.stable."1.99.0");

      mkWorkspaceSrc =
        craneLib:
        lib.cleanSourceWith {
          filter =
            path: type:
            (craneLib.filterCargoSources path type)
            || (builtins.match ".*/plugins/.*" path != null)
            || (builtins.match ".*/prompts/.*" path != null)
            || (builtins.match ".*/themes/.*" path != null)
            || (builtins.match ".*/words/.*" path != null)
            || (lib.hasSuffix ".lua" path);
          src = lib.cleanSource ./.;
        };

      # The minimal toolchain skips rust-docs, clippy and rustfmt, which no build
      # needs. Dependencies are built without the `cargo check` and `cargo test
      # --no-run` passes crane runs by default: no build here runs tests.
      mkMaki =
        pkgs: profile:
        let
          craneLib = mkCraneLib pkgs (toolchains: toolchains.minimal);
          src = mkWorkspaceSrc craneLib;
          commonArgs = {
            pname = packageName;
            inherit src version;
            CARGO_PROFILE = profile;
            doCheck = false;
            nativeBuildInputs = with pkgs; [
              pkg-config
              python3
            ];
            buildInputs = with pkgs; [
              stdenv.cc.cc.lib
            ];
            cargoVendorDir = craneLib.vendorCargoDeps {
              inherit src;
              outputHashes = gitDepHashes;
            };
          };
        in
        craneLib.buildPackage (
          commonArgs
          // {
            cargoArtifacts = craneLib.buildDepsOnly (
              commonArgs // { buildPhaseCargoCommand = "cargoWithProfile build --locked"; }
            );
            cargoExtraArgs = "--package ${packageName}";
          }
        );

      cargoLockParsed = builtins.fromTOML (builtins.readFile ./Cargo.lock);

      # Exact Cargo.lock source strings (with fragment) of all git deps.
      # A dep bump changes the key: replace it with the new Cargo.lock source
      # string, set the hash to "", rebuild, and paste the hash from the error.
      # A missing key falls back to fetchGit with an eval warning; a wrong hash
      # fails the build with the real hash printed.
      gitDepSources = lib.unique (
        builtins.filter (lib.hasPrefix "git+") (map (p: p.source or "") cargoLockParsed.package)
      );

      gitDepHashes = {
        "git+https://github.com/crossterm-rs/crossterm?rev=94ed7154e35d24a2f643c61760e1eb05b8ca9e63#94ed7154e35d24a2f643c61760e1eb05b8ca9e63" =
          "sha256-r2Xsxw8+eqESsJKaax5pb/rIWshkoqi5E81Dkh8snZ8=";
        "git+https://github.com/tontinton/syntect?rev=01df275f6f25da670e5ba5b7128cb89d03795119#01df275f6f25da670e5ba5b7128cb89d03795119" =
          "sha256-ZszhXL+Bd0zZGOfOodvP4z6dLD3jJr3pO07riwFUC70=";
      };

      # Fail loudly at eval when a Cargo.lock git dep has no pinned hash: the
      # fetchGit fallback only warns, and a silent warning is how this rots.
      assertGitDepHashes =
        lib.foldr (
          source: acc:
          if builtins.hasAttr source gitDepHashes then
            acc
          else
            builtins.throw "flake.nix gitDepHashes is missing an entry for ${source} (add with hash \"\", rebuild, paste the hash from the error)"
        ) null gitDepSources;
    in
    assert assertGitDepHashes == null;
    {
      packages = forEachSystem (_: pkgs: { default = mkMaki pkgs "release"; });

      checks = forEachSystem (
        _: pkgs: {
          # Catches what only breaks under Nix (source filter, vendoring, native
          # inputs) in a fraction of the time of the fat LTO release build.
          build = mkMaki pkgs "dev";
        }
      );

      devShells = forEachSystem (
        _: pkgs:
        let
          craneLib = mkCraneLib pkgs (
            toolchains:
            toolchains.default.override {
              extensions = [
                "rust-src"
                "rust-analyzer"
              ];
            }
          );
          certs = "${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt";
        in
        {
          default = craneLib.devShell {
            packages = with pkgs; [
              cargo-machete
              cargo-nextest
              git
              just
              pkg-config
              ripgrep
              stylua
            ];

            SSL_CERT_FILE = certs;
            NIX_SSL_CERT_FILE = certs;

            LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath [
              pkgs.stdenv.cc.cc.lib
            ];
          };
        }
      );

      formatter = forEachSystem (_: pkgs: pkgs.nixfmt);
    };
}
