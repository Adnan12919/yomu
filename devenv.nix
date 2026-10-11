{ pkgs, lib, ... }:

let
  # Libraries gpui links against or opens at runtime (via dlopen).
  runtimeLibs = with pkgs; [
    wayland
    libxkbcommon
    vulkan-loader
    libGL
    fontconfig
    freetype
    # X11 fallback, only used when there is no Wayland session
    libxcb
    libx11
    libxcursor
    libxi
    libxrandr
  ];
in
{
  # gpui needs a recent compiler (edition 2024, Rust 1.89+): the latest stable from rust-overlay,
  # which is also what flake.nix's dev shell uses.
  languages.rust = {
    enable = true;
    channel = "stable";
    components = [
      "rustc"
      "cargo"
      "clippy"
      "rustfmt"
      "rust-analyzer"
      "rust-src"
    ];
    # mold is on by default on x86_64 Linux and makes the big gpui link much faster.
    # If you ever hit a linker problem, set this to false.
    # mold.enable = false;
  };

  packages = [
    pkgs.pkg-config
    pkgs.cmake
  ] ++ runtimeLibs;

  # Without this, gpui cannot find libwayland / libvulkan at runtime on NixOS.
  env.LD_LIBRARY_PATH = lib.makeLibraryPath runtimeLibs;

  enterShell = ''
    echo "yomu dev shell: $(rustc --version)"
    echo "  cargo run --release   build and start the reader"
    echo "  devenv test           run the unit and UI tests"
  '';

  # `devenv test`
  enterTest = ''
    cargo test
  '';
}
