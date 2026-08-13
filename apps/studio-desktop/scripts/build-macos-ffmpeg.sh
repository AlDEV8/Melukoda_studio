#!/usr/bin/env bash
set -euo pipefail

# Builds the small, release-only macOS encoder resource. Run on the target
# macOS architecture before `tauri build --config src-tauri/tauri.release.json`.
# FFmpeg and SRT are pinned so that a release can be reproduced and audited.
ffmpeg_tag="n8.1.2"
srt_tag="v1.5.4"
script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
resource_dir="${script_dir}/../src-tauri/resources"
build_root="${MELUKODA_ENCODER_BUILD_DIR:-$(mktemp -d)}"
cleanup_build="${MELUKODA_ENCODER_BUILD_DIR:+false}"

cleanup() { [ "$cleanup_build" = false ] || rm -rf "$build_root"; }
trap cleanup EXIT
mkdir -p "$resource_dir" "$build_root"

command -v cmake >/dev/null || { echo "cmake is required" >&2; exit 1; }
command -v pkg-config >/dev/null || { echo "pkg-config is required" >&2; exit 1; }
command -v curl >/dev/null || { echo "curl is required" >&2; exit 1; }
llvm_ar="${LLVM_AR:-$(command -v llvm-ar || true)}"
[ -n "$llvm_ar" ] || llvm_ar="/opt/homebrew/opt/llvm/bin/llvm-ar"
[ -n "$llvm_ar" ] || { echo "llvm-ar is required (brew install llvm)" >&2; exit 1; }

curl -fsSLo "$build_root/srt.tar.gz" "https://github.com/Haivision/srt/archive/refs/tags/${srt_tag}.tar.gz"
tar -xzf "$build_root/srt.tar.gz" -C "$build_root"
cmake -S "$build_root/srt-${srt_tag#v}" -B "$build_root/srt-build" \
  -DCMAKE_BUILD_TYPE=Release -DENABLE_SHARED=OFF -DENABLE_STATIC=ON \
  -DENABLE_APPS=OFF -DENABLE_UNITTESTS=OFF -DENABLE_ENCRYPTION=ON \
  -DCMAKE_POLICY_VERSION_MINIMUM=3.5 \
  -DCMAKE_INSTALL_PREFIX="$build_root/prefix"
cmake --build "$build_root/srt-build" --config Release --parallel
cmake --install "$build_root/srt-build"

curl -fsSLo "$build_root/ffmpeg.tar.gz" "https://github.com/FFmpeg/FFmpeg/archive/refs/tags/${ffmpeg_tag}.tar.gz"
tar -xzf "$build_root/ffmpeg.tar.gz" -C "$build_root"
pushd "$build_root/FFmpeg-${ffmpeg_tag}" >/dev/null
AR="$llvm_ar" PKG_CONFIG_PATH="$build_root/prefix/lib/pkgconfig" ./configure \
  --prefix="$build_root/ffmpeg-prefix" --disable-shared --enable-static --disable-autodetect \
  --disable-doc --disable-debug --enable-small --disable-ffplay --disable-ffprobe \
  --disable-avdevice --disable-audiotoolbox --disable-videotoolbox \
  --enable-ffmpeg --enable-libsrt \
  --pkg-config-flags="--static" --extra-cflags="-I$build_root/prefix/include" \
  --extra-ldflags="-L$build_root/prefix/lib" --extra-libs="-lc++"
make -j"$(sysctl -n hw.ncpu)" ffmpeg
cp ffmpeg "$resource_dir/ffmpeg"
chmod 755 "$resource_dir/ffmpeg"
popd >/dev/null
openssl_libdir="$(pkg-config --variable=libdir openssl)"
for library in libssl.3.dylib libcrypto.3.dylib; do
  cp "$openssl_libdir/$library" "$resource_dir/$library"
done
ssl_reference="$(otool -L "$resource_dir/ffmpeg" | awk '/libssl\.3\.dylib/{print $1; exit}')"
crypto_reference="$(otool -L "$resource_dir/ffmpeg" | awk '/libcrypto\.3\.dylib/{print $1; exit}')"
ssl_crypto_reference="$(otool -L "$resource_dir/libssl.3.dylib" | awk '/libcrypto\.3\.dylib/{print $1; exit}')"
install_name_tool -change "$ssl_reference" "@loader_path/libssl.3.dylib" "$resource_dir/ffmpeg"
install_name_tool -change "$crypto_reference" "@loader_path/libcrypto.3.dylib" "$resource_dir/ffmpeg"
install_name_tool -change "$ssl_crypto_reference" "@loader_path/libcrypto.3.dylib" "$resource_dir/libssl.3.dylib"
cat > "$resource_dir/FFMPEG-NOTICE.txt" <<'NOTICE'
FFmpeg n8.1.2 with Haivision SRT v1.5.4, built for Melukoda Studio.
FFmpeg source: https://github.com/FFmpeg/FFmpeg
SRT source: https://github.com/Haivision/srt
NOTICE
echo "Prepared macOS FFmpeg encoder at $resource_dir/ffmpeg"
