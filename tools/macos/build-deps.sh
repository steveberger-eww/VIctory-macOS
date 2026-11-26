#!/bin/zsh
# Builds x86_64 copies of GMP, Nettle, GnuTLS, FreeType and SDL2 into the
# prefix given as the first argument. Wine loads GnuTLS for TLS, FreeType for
# fonts and SDL2 for game controllers: SDL presents PlayStation, Switch and
# other pads to XInput with the Xbox layout. Homebrew on Apple Silicon only ships arm64 builds, which an x86_64
# Wine cannot load. The prefix path must have no spaces in it.
set -eu
PREFIX="$1"
SRC="${PREFIX:h}/deps-src"
WORK="${PREFIX:h}/deps-build"
BREW="${HOMEBREW_PREFIX:-/opt/homebrew}"
JOBS="$(sysctl -n hw.ncpu)"
. "${0:A:h}/native-build-tools.sh"
x86() { arch -x86_64 /usr/bin/env PATH="$NATIVE_TOOLS:$BREW/opt/bison/bin:$BREW/opt/flex/bin:$BREW/bin:/usr/bin:/bin:/usr/sbin:/sbin" "$@"; }

if [[ -f $PREFIX/lib/libgnutls.30.dylib && -f $PREFIX/lib/libfreetype.6.dylib && -f $PREFIX/lib/libSDL2-2.0.0.dylib ]]; then
  echo "libraries already built"
  exit 0
fi

# Each entry is a tarball, its SHA-256 and the mirrors that serve it.
DEPS=(
  "gmp-6.3.0.tar.xz a3c2b80201b89e68616f4ad30bc66aee4927c3ce50e33929ca819d5c43538898 https://ftpmirror.gnu.org/gnu/gmp https://mirrors.kernel.org/gnu/gmp https://ftp.gnu.org/gnu/gmp"
  "nettle-3.10.tar.gz b4c518adb174e484cb4acea54118f02380c7133771e7e9beb98a0787194ee47c https://ftpmirror.gnu.org/gnu/nettle https://mirrors.kernel.org/gnu/nettle https://ftp.gnu.org/gnu/nettle"
  "gnutls-3.8.13.tar.xz ffed8ec1bf09c2426d4f14aae377de4753b53e537d685e604e99a8b16ca9c97e https://www.gnupg.org/ftp/gcrypt/gnutls/v3.8 https://mirrors.dotsrc.org/gcrypt/gnutls/v3.8 https://ftp.heanet.ie/mirrors/ftp.gnupg.org/gcrypt/gnutls/v3.8"
  "SDL2-2.32.10.tar.gz 5f5993c530f084535c65a6879e9b26ad441169b3e25d789d83287040a9ca5165 https://github.com/libsdl-org/SDL/releases/download/release-2.32.10 https://www.libsdl.org/release"
  "freetype-2.14.3.tar.xz 36bc4f1cc413335368ee656c42afca65c5a3987e8768cc28cf11ba775e785a5f https://download.savannah.gnu.org/releases/freetype https://downloads.sourceforge.net/project/freetype/freetype2/2.14.3 https://mirrors.sarata.com/non-gnu/freetype"
)
mkdir -p "$SRC" "$WORK"
# Each tarball is tried from every mirror in turn, and a broken download is
# resumed. The checksum decides what is kept, so a mirror cannot change it.
for entry in $DEPS; do
  parts=(${=entry}); name="${parts[1]}"; sum="${parts[2]}"; file="$SRC/$name"
  ok=0
  for mirror in ${parts[3,-1]}; do
    if [[ -f $file ]] && echo "$sum  $file" | shasum -a 256 -c --quiet 2> /dev/null; then ok=1; break; fi
    echo "downloading $name from ${mirror}"
    curl -fL --retry 5 --retry-all-errors --connect-timeout 20 -C - -o "$file.part" "$mirror/$name" || { rm -f "$file.part"; continue; }
    mv "$file.part" "$file"
    if echo "$sum  $file" | shasum -a 256 -c --quiet 2> /dev/null; then ok=1; break; fi
    echo "checksum mismatch for $name from this mirror, trying the next one"
    rm -f "$file"
  done
  (( ok )) || { echo "could not download $name from any mirror" >&2; exit 1; }
  tar -xf "$file" -C "$WORK"
done

common=(--host=x86_64-apple-darwin --build=aarch64-apple-darwin --prefix="$PREFIX")
export CC="clang -arch x86_64" CXX="clang++ -arch x86_64"
export PKG_CONFIG_PATH="$PREFIX/lib/pkgconfig" CPPFLAGS="-I$PREFIX/include" LDFLAGS="-L$PREFIX/lib"
build() {
  local dir="$WORK/$1"; shift
  echo "building ${dir:t}"
  ( cd "$dir" && CONFIG_SITE=/dev/null x86 ./configure "${common[@]}" "$@" > configure.log 2>&1 \
      && x86 make -j"$JOBS" > make.log 2>&1 && x86 make install > install.log 2>&1 ) \
    || { echo "building ${dir:t} failed, see the logs in $dir" >&2; exit 1; }
}
# Each library is skipped once it is installed, so a prefix from before SDL
# was added only builds SDL.
built() { [[ -f $PREFIX/lib/$1 ]] && echo "${2} already built"; }
built libgmp.10.dylib gmp-6.3.0 ||
build gmp-6.3.0 --disable-assembly --enable-shared --enable-static
built libnettle.8.dylib nettle-3.10 ||
build nettle-3.10 --disable-documentation --disable-assembler --enable-shared --enable-static
built libgnutls.30.dylib gnutls-3.8.13 ||
build gnutls-3.8.13 --with-included-libtasn1 --with-included-unistring --without-p11-kit \
  --disable-doc --disable-tests --disable-tools --without-idn --without-brotli --without-zstd \
  --disable-guile --disable-cxx --disable-nls --disable-libdane --disable-openssl-compatibility \
  --disable-hardware-acceleration --enable-shared --enable-static=no
built libfreetype.6.dylib freetype-2.14.3 ||
build freetype-2.14.3 --without-harfbuzz --without-png --without-bzip2 --without-brotli
# Wine only asks SDL for game controllers and haptics, so video, audio and
# the rest stay out.
built libSDL2-2.0.0.dylib SDL2-2.32.10 ||
build SDL2-2.32.10 --disable-video --disable-audio --disable-render --disable-sensor \
  --disable-power --disable-filesystem --disable-locale --disable-misc --disable-static
echo "libraries installed in $PREFIX"
