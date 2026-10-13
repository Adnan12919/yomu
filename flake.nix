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
    {
      # For a NixOS system flake: nixpkgs.overlays = [ inputs.yomu.overlays.default ];
      # then `pkgs.yomu` is available.
      overlays.default = final: _prev: {
        yomu = final.callPackage ./nix/package.nix { };
      };
    }
    // flake-utils.lib.eachSystem [ "x86_64-linux" "aarch64-linux" ] (system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ (import rust-overlay) ];
        };

        # GPUI Kit 0.7.3 needs a recent compiler, so the dev shell uses rust-overlay's latest
        # stable, the same toolchain devenv.nix uses.
        rustToolchain = pkgs.rust-bin.stable.latest.default.override {
          extensions = [ "rust-src" ];
        };

        yomu = pkgs.callPackage ./nix/package.nix { };
      in
      {
        packages = {
          default = yomu;
          inherit yomu;
        };

        apps.default = flake-utils.lib.mkApp { drv = yomu; };

        # `nix develop` still works; `devenv shell` is the main way (see devenv.nix).
        devShells.default = pkgs.mkShell {
          inputsFrom = [ yomu ];
          packages = [ rustToolchain ];

          # Without this, wgpu/blade cannot find libvulkan / libwayland at runtime on NixOS.
          LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath yomu.buildInputs;
          RUST_SRC_PATH = "${rustToolchain}/lib/rustlib/src/rust/library";

          shellHook = ''
            echo "yomu dev shell: $(rustc --version)"
          '';
        };
      });
}
