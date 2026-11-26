#!/bin/zsh
# Builds CodeWeavers' CrossOver Wine (GPL source, published by CodeWeavers)
# for x86_64 under Rosetta and installs it into target/wine-cx. D3DMetal is
# made for this Wine, and its memory APIs are new enough for VIctory.
#
# DEPS names an x86_64 prefix with FreeType, GnuTLS and SDL2, as
# tools/macos/build-deps.sh builds.
set -eu
VERSION="${CROSSOVER_VERSION:-26.3.0}"
ROOT="${0:A:h:h:h}"
DEPS_REAL="${DEPS:?set DEPS to an x86_64 prefix with FreeType, GnuTLS and SDL2}"
# Wine's build splits flags at spaces, and both the project and the deps
# can live under folders with spaces in their names. Space-free links to
# them keep every path in the build free of spaces.
ALIAS="$HOME/Library/Caches/victory-macos"
mkdir -p "$ALIAS" "$ROOT/target"
ln -sfn "$ROOT/target" "$ALIAS/target"
# The deps are copied, and each library's recorded path is pointed at the
# copy, because a prefix built elsewhere may record a folder that is gone.
DEPS="$ALIAS/target/deps"
# A copy made before SDL2 was added is made again.
if [[ ! -f $DEPS/.relinked || ! -f $DEPS/lib/libSDL2-2.0.0.dylib ]]; then
  rm -rf "$DEPS"
  ditto "$DEPS_REAL" "$DEPS"
  for lib in "$DEPS"/lib/*.dylib(.N); do
    old_dir="$(otool -D "$lib" | sed -n 2p)"
    old_dir="${old_dir:h}"
    install_name_tool -id "$DEPS/lib/${lib:t}" "$lib" 2> /dev/null
    for ref in $(otool -L "$lib" | awk 'NR > 2 {print $1}' | grep -F "$old_dir/" || true); do
      install_name_tool -change "$ref" "$DEPS/lib/${ref:t}" "$lib" 2> /dev/null
    done
    codesign -f -s - "$lib" 2> /dev/null || true
  done
  touch "$DEPS/.relinked"
fi
WORK="$ALIAS/target/crossover"
OBJ="$WORK/build"
PREFIX="$ALIAS/target/wine-cx"
BREW="${BREW:-/opt/homebrew}"
. "${0:A:h}/native-build-tools.sh"
x86() { arch -x86_64 /usr/bin/env PATH="$NATIVE_TOOLS:$BREW/opt/bison/bin:$BREW/opt/flex/bin:$BREW/bin:/usr/bin:/bin:/usr/sbin:/sbin" "$@"; }

mkdir -p "$WORK" "$OBJ"
TAR="$WORK/crossover-sources-$VERSION.tar.gz"
if [[ ! -d $WORK/sources/wine ]]; then
  [[ -f $TAR ]] || curl -fL --retry 3 -o "$TAR" "https://media.codeweavers.com/pub/crossover/source/crossover-sources-$VERSION.tar.gz"
  if [[ $VERSION == 26.3.0 ]]; then
    echo "ac99c8ca4b3848f3e81784135f023df266b61c2345726ea55a50b3e030dd6872  $TAR" | shasum -a 256 -c --quiet || { echo "checksum mismatch for ${TAR:t}" >&2; exit 1; }
  fi
  tar -xzf "$TAR" -C "$WORK"
fi
SRC="$WORK/sources/wine"

# CrossOver's win32u names libvulkan even when Wine is built without
# Vulkan. A fallback name lets it compile, and loading it then fails
# cleanly at run time. D3DMetal does not use Vulkan.
VK="$SRC/dlls/win32u/vulkan.c"
if ! grep -q 'victory-macos fallback' "$VK"; then
  printf '%s\n' '/* victory-macos fallback */' '#ifndef SONAME_LIBVULKAN' '#define SONAME_LIBVULKAN "libvulkan.1.dylib"' '#endif' | cat - "$VK" > "$VK.new"
  mv "$VK.new" "$VK"
fi

# With SDL, winebus presents PlayStation, Switch and other pads to XInput
# with the Xbox layout. Wine wants PlayStation pads raw from a hidraw bus
# instead (IOHID here), drops SDL's copy of them, and on macOS never marks
# the raw copy as a gamepad, so XInput saw no pad. Hidraw is therefore off
# while SDL runs. It is turned off before the SDL bus starts, because the
# first pads arrive before its start returns, and back on if SDL fails to
# load. With SDL off, IOHID would also list every pad a second time, and an
# Xbox pad would take two XInput slots. `Enable SDL` = 0 in the registry
# brings back IOHID.
BUS="$SRC/dlls/winebus.sys/main.c"
if ! grep -q 'victory-macos sdl first' "$BUS"; then
  perl -0pi -e 's/\n        if \(!sdl_driver_init\(\)\) options\.disable_input = TRUE;\n/\n        \/* victory-macos sdl first *\/\n        {\n            BOOL disable_hidraw = options.disable_hidraw;\n            options.disable_hidraw = TRUE;\n            if (!sdl_driver_init()) options.disable_input = TRUE;\n            else options.disable_hidraw = disable_hidraw;\n        }\n/ or die "sdl_driver_init call not found\n"' "$BUS"
fi

cd "$OBJ"
# A build configured before SDL2 was added is configured again.
if [[ ! -f Makefile ]] || ! grep -q '^#define SONAME_LIBSDL2' include/config.h; then
  echo "configuring CrossOver Wine"
  # The prefix has spaces in it, so autoconf's default site file path would
  # be split at the first one. No site file is needed.
  CONFIG_SITE=/dev/null x86 "$SRC/configure" --prefix="$PREFIX" --enable-archs=x86_64 \
    CC="clang -arch x86_64" CXX="clang++ -arch x86_64" \
    CFLAGS="-g -O2 -I$DEPS/include -Wno-error=incompatible-function-pointer-types -Wno-error=int-conversion" \
    LDFLAGS="-L$DEPS/lib -Wl,-rpath,@loader_path/../.." \
    PKG_CONFIG_PATH="$DEPS/lib/pkgconfig" \
    FREETYPE_CFLAGS="-I$DEPS/include/freetype2" FREETYPE_LIBS="-L$DEPS/lib -lfreetype" \
    GNUTLS_CFLAGS="-I$DEPS/include" GNUTLS_LIBS="-L$DEPS/lib -lgnutls" \
    SDL2_CFLAGS="-I$DEPS/include/SDL2 -D_THREAD_SAFE" SDL2_LIBS="-L$DEPS/lib -lSDL2" \
    --without-gstreamer --without-cups --without-sane --without-krb5 \
    --without-netapi --without-pcap --without-usb --without-v4l2 --without-capi \
    --without-gphoto --without-x --without-wayland --without-pulse --without-alsa \
    --without-oss --without-udev --without-inotify --without-vulkan --without-opencl \
    --without-ffmpeg > configure.log 2>&1 || { tail -20 configure.log; exit 1; }
  grep -q '^#define SONAME_LIBSDL2' include/config.h || { grep -i sdl configure.log; echo "configure did not find SDL2 in $DEPS" >&2; exit 1; }
fi
echo "building CrossOver Wine"
x86 make -j"$(sysctl -n hw.ncpu)" > make.log 2>&1 || { grep -E 'error:|\*\*\*' make.log | grep -v Wno-error | head -5; exit 1; }
x86 make install > install.log 2>&1
cp -f "$DEPS"/lib/lib{gmp.10,nettle.8,hogweed.6,gnutls.30,freetype.6,SDL2-2.0.0}.dylib "$PREFIX/lib/"
echo "installed in $PREFIX"
