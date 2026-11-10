#!/usr/bin/env bash
# Builds PenTouch-x86_64.AppImage.
#
# The binary is linked against an OLD glibc (2.17) using zig as the linker, so the AppImage
# runs on practically every distro from the last ~10 years (Ubuntu 18.04+, Debian 9+, Fedora,
# Arch, openSUSE, ...). The window talks to X11/Wayland/OpenGL through the system's own
# libraries at run time, so nothing graphical needs to be bundled.
#
# Needs: Rust (cargo), python3 + pip (for zig), curl.
#   cargo install cargo-zigbuild --locked      # one time
#   pip install ziglang                        # one time (provides zig)
set -euo pipefail
cd "$(dirname "$0")"

ARCH="${ARCH:-x86_64}"
GLIBC="${GLIBC:-2.17}"
TARGET="${ARCH}-unknown-linux-gnu"
OUT="PenTouch-${ARCH}.AppImage"

echo "==> building (glibc ${GLIBC})"
cargo zigbuild --release --target "${TARGET}.${GLIBC}"

echo "==> assembling AppDir"
rm -rf AppDir
mkdir -p AppDir/usr/bin AppDir/usr/share/icons/hicolor/256x256/apps
install -m755 "target/${TARGET}/release/pen-touch" AppDir/usr/bin/pen-touch
install -m755 packaging/AppRun AppDir/AppRun
install -m644 packaging/pen-touch.desktop AppDir/pen-touch.desktop
install -m644 packaging/pen-touch-256.png AppDir/pen-touch.png
install -m644 packaging/pen-touch-256.png AppDir/usr/share/icons/hicolor/256x256/apps/pen-touch.png
ln -sf pen-touch.png AppDir/.DirIcon

echo "==> packing"
TOOL="${APPIMAGETOOL:-}"
if [ -z "$TOOL" ]; then
  if command -v appimagetool >/dev/null; then TOOL="$(command -v appimagetool)"
  else
    curl -fsSL -o appimagetool.AppImage \
      "https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-${ARCH}.AppImage"
    chmod +x appimagetool.AppImage
    # extract so it also works where FUSE is not available (containers, CI)
    ./appimagetool.AppImage --appimage-extract >/dev/null
    TOOL="$PWD/squashfs-root/AppRun"
  fi
fi
ARCH="$ARCH" "$TOOL" --no-appstream AppDir "$OUT"
chmod +x "$OUT"
echo "==> done: $OUT ($(du -h "$OUT" | cut -f1))"
