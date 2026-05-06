{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/549bd84d6279f9852cae6225e372cc67fb91a4c1";
    rust-overlay.url = "github:oxalica/rust-overlay/adf987c76af8d17b8256d23631bcf203f81e1a63";
    flake-parts.url = "github:hercules-ci/flake-parts/0678d8986be1661af6bb555f3489f2fdfc31f6ff";
    v_flakes.url = "github:valeratrades/v_flakes/a5dc46fbbf76ae92fb6f96147a008d0429f9848d";
  };

  outputs = inputs@{ self, nixpkgs, rust-overlay, flake-parts, v_flakes, ... }:
    flake-parts.lib.mkFlake { inherit inputs; } {
      systems = nixpkgs.lib.systems.flakeExposed;

      perSystem = { config, self', inputs', system, ... }:
        let
          pkgs = import nixpkgs {
            inherit system;
            overlays = [ (import rust-overlay) ];
          };
          inherit (pkgs) lib;
          pname = "pluely";
          version = "0.1.9";

          rust = pkgs.rust-bin.stable.latest.default.override {
            extensions = [ "rust-src" "rust-analyzer" ];
          };

          files = v_flakes.files;

          github = v_flakes.github {
            inherit pkgs pname;
            syncFork = true;
          };

          linuxDeps = with pkgs; [
            # Tauri/WebKit runtime
            webkitgtk_4_1
            gtk3
            cairo
            gdk-pixbuf
            glib
            dbus
            openssl
            librsvg
            libsoup_3
            libayatana-appindicator
            # Audio (cpal + libpulse-binding in Cargo.toml)
            alsa-lib
            libpulseaudio
          ];

          systemDeps = lib.optionals pkgs.stdenv.isLinux linuxDeps;
        in
        {
          _module.args.pkgs = pkgs;

          # `nix build` — runs npm install then tauri build
          packages.default = pkgs.stdenv.mkDerivation {
            inherit pname version;
            src = ./.;

            # Update these hashes after the first failed `nix build` run:
            #   npm hash: copy from the error output of fetchNpmDeps
            #   cargo hash: copy from the error output of fetchCargoVendor
            npmDeps = pkgs.fetchNpmDeps {
              src = ./.;
              fetcherVersion = 2;
              hash = "sha256-CNeyHQqGhm112a59+mTgWHkTxvkyM8iSNc35/XQZ4Po=";
            };

            npmFlags = [ "--legacy-peer-deps" ];
            cargoDeps = pkgs.rustPlatform.fetchCargoVendor {
              src = ./src-tauri;
              hash = "sha256-C3a4ZId21VRN6EDbbAVLRRPiX+B3MLQ2R/24fLUeXGE=";
            };

            nativeBuildInputs = [
              rust
              pkgs.nodejs_22
              pkgs.pkg-config
              pkgs.npmHooks.npmConfigHook
              pkgs.rustPlatform.cargoSetupHook
            ] ++ lib.optionals pkgs.stdenv.isLinux [ pkgs.wrapGAppsHook3 ];

            buildInputs = systemDeps;

            cargoRoot = "src-tauri";

            buildPhase = ''
              npm run tauri build -- --no-bundle
            '';

            installPhase = ''
              install -Dm755 src-tauri/target/release/${pname} $out/bin/${pname}
            '';
          };

          devShells.default = pkgs.mkShell {
            packages = [
              rust
              pkgs.nodejs_22
              pkgs.pkg-config
              pkgs.openssl
            ] ++ systemDeps ++ github.enabledPackages;

            env = {
              RUST_BACKTRACE = 1;
              RUST_LIB_BACKTRACE = 0;
            };

            shellHook = github.shellHook + ''
              cp -f ${(files.gitattributes) { inherit pkgs; lfs = false; }} ./.gitattributes
            '';
          };
        };
    };
}
