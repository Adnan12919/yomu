#!/usr/bin/env bash
set -euo pipefail

# Generates all platform-specific icon formats from assets/logo.png:
# - Linux: assets/icons/<size>x<size>.png (standard XDG sizes: 16 to 1024)
# - Windows: assets/yomu.ico (multi-resolution embedded PE icon)
# - macOS: assets/yomu.icns (Apple Icon Image for .app bundles)

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

if [ ! -f "assets/logo.png" ]; then
  echo "Error: assets/logo.png not found" >&2
  exit 1
fi

echo "1. Generating Linux XDG PNG sizes..."
mkdir -p assets/icons
for sz in 16 24 32 48 64 128 256 512 1024; do
  magick assets/logo.png -resize "${sz}x${sz}" "assets/icons/${sz}x${sz}.png"
done

echo "2. Generating Windows icon (assets/yomu.ico)..."
magick assets/logo.png -define icon:auto-resize=256,128,64,48,32,24,16 assets/yomu.ico

echo "3. Generating macOS icon (assets/yomu.icns)..."
RUSTC="rustc"
if ! command -v rustc >/dev/null 2>&1 && command -v nix >/dev/null 2>&1; then
  RUSTC="nix develop --command rustc"
fi

$RUSTC - << 'EOF' -o /tmp/make_icns && /tmp/make_icns && rm -f /tmp/make_icns
use std::fs;
use std::io::Write;

fn main() -> std::io::Result<()> {
    let chunks: &[(&[u8; 4], &str)] = &[
        (b"icp4", "assets/icons/16x16.png"),
        (b"icp5", "assets/icons/32x32.png"),
        (b"ic11", "assets/icons/32x32.png"),
        (b"icp6", "assets/icons/64x64.png"),
        (b"ic12", "assets/icons/64x64.png"),
        (b"ic07", "assets/icons/128x128.png"),
        (b"ic08", "assets/icons/256x256.png"),
        (b"ic13", "assets/icons/256x256.png"),
        (b"ic09", "assets/icons/512x512.png"),
        (b"ic14", "assets/icons/512x512.png"),
        (b"ic10", "assets/icons/1024x1024.png"),
    ];

    let mut body = Vec::new();
    for (tag, path) in chunks {
        if let Ok(data) = fs::read(path) {
            body.extend_from_slice(*tag);
            let chunk_len = (8 + data.len()) as u32;
            body.extend_from_slice(&chunk_len.to_be_bytes());
            body.extend_from_slice(&data);
        }
    }

    let total_len = (8 + body.len()) as u32;
    let mut out = fs::File::create("assets/yomu.icns")?;
    out.write_all(b"icns")?;
    out.write_all(&total_len.to_be_bytes())?;
    out.write_all(&body)?;
    Ok(())
}
EOF

echo "Done! All platform icons updated from assets/logo.png."
