# The yomu package. Used by the flake (`nix build`, the overlay) and usable directly:
#
#   pkgs.callPackage ./nix/package.nix { }
{
  lib,
  rustPlatform,
  pkg-config,
  cmake,
  makeWrapper,
  fontconfig,
  freetype,
  libxkbcommon,
  wayland,
  vulkan-loader,
  libGL,
  libxcb,
  libx11,
  libxcursor,
  libxi,
  libxrandr,
}:

let
  # Libraries gpui links against or opens at runtime (via dlopen).
  runtimeLibs = [
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
  cargoToml = lib.importTOML ../Cargo.toml;
in
rustPlatform.buildRustPackage {
  pname = "yomu";
  version = cargoToml.package.version;

  # Only the files the build needs, so editing the README does not trigger a rebuild.
  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ../themes.example.toml
      ../src
      ../vendor
      ../packaging
    ];
  };

  cargoLock.lockFile = ../Cargo.lock;

  nativeBuildInputs = [
    pkg-config
    cmake
    makeWrapper
  ];
  buildInputs = runtimeLibs;

  # The UI tests need gpui's test platform; run them with `cargo test` in the dev shell.
  doCheck = false;

  postInstall = ''
    install -Dm644 packaging/yomu.desktop $out/share/applications/yomu.desktop
  '';

  # Without this, gpui cannot find libwayland / libvulkan at runtime on NixOS.
  postFixup = ''
    wrapProgram $out/bin/yomu --prefix LD_LIBRARY_PATH : ${lib.makeLibraryPath runtimeLibs}
  '';

  meta = {
    description = "Minimal, GPU-accelerated, Wayland-native manga reader";
    mainProgram = "yomu";
    platforms = lib.platforms.linux;
    license = lib.licenses.mit;
  };
}
