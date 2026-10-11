{
  description = "yomu: a minimal, GPU-accelerated, Wayland-native manga reader";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, flake-utils, rust-overlay }:
    flake-utils.lib.eachSystem [ "x86_64-linux" "aarch64-linux" ] (system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ (import rust-overlay) ];
        };
        inherit (pkgs) lib;

        # gpui needs a recent compiler (edition 2024), so use rust-overlay's latest stable.
        rustToolchain = pkgs.rust-bin.stable.latest.default.override {
          extensions = [ "rust-src" ];
        };
        rustPlatform = pkgs.makeRustPlatform {
          cargo = rustToolchain;
          rustc = rustToolchain;
        };

        # Libraries gpui opens at runtime (via dlopen) and links against at build time.
        runtimeLibs = with pkgs; [
          wayland
          libxkbcommon
          vulkan-loader
          libGL
          fontconfig
          freetype
          # X11 fallback, only used if there is no Wayland session
          libxcb
          libx11
          libxcursor
          libxi
          libxrandr
        ];
        runtimeLibPath = lib.makeLibraryPath runtimeLibs;

        nativeBuildInputs = with pkgs; [
          pkg-config
          cmake
        ];
      in
      {
        packages.default = rustPlatform.buildRustPackage {
          pname = "yomu";
          version = "0.1.0";
          src = lib.cleanSource ./.;
          cargoLock.lockFile = ./Cargo.lock;

          nativeBuildInputs = nativeBuildInputs ++ [ pkgs.makeWrapper ];
          buildInputs = runtimeLibs;

          postInstall = ''
            install -Dm644 packaging/yomu.desktop $out/share/applications/yomu.desktop
          '';
          postFixup = ''
            wrapProgram $out/bin/yomu --prefix LD_LIBRARY_PATH : ${runtimeLibPath}
          '';

          meta = {
            description = "Minimal, GPU-accelerated, Wayland-native manga reader";
            mainProgram = "yomu";
            platforms = lib.platforms.linux;
          };
        };

        apps.default = flake-utils.lib.mkApp { drv = self.packages.${system}.default; };

        devShells.default = pkgs.mkShell {
          inherit nativeBuildInputs;
          buildInputs = [ rustToolchain ] ++ runtimeLibs;

          # Without this, wgpu/blade cannot find libvulkan / libwayland at runtime on NixOS.
          LD_LIBRARY_PATH = runtimeLibPath;
          RUST_SRC_PATH = "${rustToolchain}/lib/rustlib/src/rust/library";

          shellHook = ''
            echo "yomu dev shell: $(rustc --version)"
            echo "  cargo run --release   build and start the reader"
          '';
        };
      });
}
