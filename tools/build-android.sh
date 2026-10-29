#!/usr/bin/env bash
# Compila en el Mac todo lo que se instala en los teléfonos y lo deja en
# android/dist/: pc-agent, llama-server y rpc-server (llama.cpp fijado), la app
# Android (phone-cluster.apk, que lleva dentro los tres anteriores), el modelo
# y macos/llama-server, la cabeza del modo dividido que corre en el Mac.
#
#   tools/build-android.sh                 # todo
#   MODEL_URL=... tools/build-android.sh   # otro modelo .gguf
#
# Solo hace falta ejecutarlo una vez (o tras cambiar el código del agente).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DIST="$ROOT/android/dist"
BUILD="$ROOT/android/build"
LLAMA_TAG="${LLAMA_TAG:-b11963}"
API="${ANDROID_API:-28}"
MODEL_URL="${MODEL_URL:-https://huggingface.co/Qwen/Qwen2.5-1.5B-Instruct-GGUF/resolve/main/qwen2.5-1.5b-instruct-q4_k_m.gguf}"
SDK="${ANDROID_HOME:-$HOME/Library/Android/sdk}"

NDK="${ANDROID_NDK_HOME:-}"
if [[ -z "$NDK" ]]; then
  NDK="$(ls -d "$SDK"/ndk/* 2>/dev/null | sort -V | tail -1 || true)"
fi
[[ -d "$NDK" ]] || { echo "No encuentro el Android NDK. Instálalo desde Android Studio (SDK Manager → NDK) o define ANDROID_NDK_HOME." >&2; exit 1; }
CLANG_DIR="$(ls -d "$NDK"/toolchains/llvm/prebuilt/*/bin | head -1)"
CMAKE_BIN="$(command -v cmake || ls -d "$SDK"/cmake/*/bin/cmake 2>/dev/null | sort -V | tail -1)"
NINJA_BIN="$(command -v ninja || ls -d "$SDK"/cmake/*/bin/ninja 2>/dev/null | sort -V | tail -1)"
[[ -x "$CMAKE_BIN" && -x "$NINJA_BIN" ]] || { echo "Falta cmake/ninja (SDK Manager → CMake)." >&2; exit 1; }

mkdir -p "$DIST" "$BUILD"
echo "NDK: $NDK"

step() { printf '\n== %s\n' "$*"; }

step "pc-agent (aarch64-linux-android)"
rustup target add aarch64-linux-android >/dev/null
CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="$CLANG_DIR/aarch64-linux-android${API}-clang" \
  cargo build --manifest-path "$ROOT/Cargo.toml" --release -p pc-agent --target aarch64-linux-android
cp "$ROOT/target/aarch64-linux-android/release/pc-agent" "$DIST/pc-agent"

step "llama-server y rpc-server $LLAMA_TAG"
SRC="$BUILD/llama.cpp"
if [[ ! -d "$SRC/.git" ]]; then
  git clone --quiet --depth 1 --branch "$LLAMA_TAG" https://github.com/ggml-org/llama.cpp "$SRC"
fi
# Dos variantes: la optimizada (dotprod, ARMv8.2+) y una genérica para
# procesadores ARMv8.0 como el Snapdragon 680, que con la otra mueren por
# instrucción ilegal. La app elige según /proc/cpuinfo (D-57).
build_android() { # <directorio> <arquitectura> <sufijo>
  "$CMAKE_BIN" -S "$SRC" -B "$SRC/$1" -G Ninja \
    -DCMAKE_MAKE_PROGRAM="$NINJA_BIN" \
    -DCMAKE_TOOLCHAIN_FILE="$NDK/build/cmake/android.toolchain.cmake" \
    -DANDROID_ABI=arm64-v8a -DANDROID_PLATFORM="android-$API" -DANDROID_STL=c++_static \
    -DCMAKE_BUILD_TYPE=Release -DBUILD_SHARED_LIBS=OFF \
    -DGGML_OPENMP=OFF -DGGML_NATIVE=OFF -DGGML_CPU_ARM_ARCH="$2" \
    -DGGML_RPC=ON \
    -DLLAMA_CURL=OFF -DLLAMA_OPENSSL=OFF -DLLAMA_BUILD_TESTS=OFF -DLLAMA_BUILD_EXAMPLES=OFF
  "$CMAKE_BIN" --build "$SRC/$1" --target llama-server ggml-rpc-server -j
  cp "$SRC/$1/bin/llama-server" "$DIST/llama-server$3"
  cp "$SRC/$1/bin/ggml-rpc-server" "$DIST/rpc-server$3"
  "$CLANG_DIR/llvm-strip" "$DIST/llama-server$3" "$DIST/rpc-server$3" 2>/dev/null || true
}
build_android build-android "${GGML_CPU_ARM_ARCH:-armv8.2-a+dotprod}" ""
build_android build-android-v8 armv8-a "-v8"
"$CLANG_DIR/llvm-strip" "$DIST/pc-agent" 2>/dev/null || true

step "llama-server para el Mac (cabeza del modo dividido)"
# Sin Metal ni BLAS: así -ngl manda todas las capas a los teléfonos (D-56).
"$CMAKE_BIN" -S "$SRC" -B "$SRC/build-mac" -G Ninja \
  -DCMAKE_MAKE_PROGRAM="$NINJA_BIN" -DCMAKE_BUILD_TYPE=Release -DBUILD_SHARED_LIBS=OFF \
  -DGGML_METAL=OFF -DGGML_BLAS=OFF -DGGML_RPC=ON -DGGML_NATIVE=OFF -DCMAKE_OSX_ARCHITECTURES=arm64 \
  -DLLAMA_CURL=OFF -DLLAMA_OPENSSL=OFF -DLLAMA_BUILD_TESTS=OFF -DLLAMA_BUILD_EXAMPLES=OFF
"$CMAKE_BIN" --build "$SRC/build-mac" --target llama-server ggml-rpc-server -j
mkdir -p "$DIST/macos"
cp "$SRC/build-mac/bin/llama-server" "$DIST/macos/llama-server"
cp "$SRC/build-mac/bin/ggml-rpc-server" "$DIST/macos/rpc-server"

step "app Android (APK)"
APP="$ROOT/android-app"
JBR="/Applications/Android Studio.app/Contents/jbr/Contents/Home"
export JAVA_HOME="${JAVA_HOME:-$JBR}"
[[ -x "$JAVA_HOME/bin/java" ]] || { echo "Falta un JDK 17+: instala Android Studio o define JAVA_HOME." >&2; exit 1; }
echo "sdk.dir=$SDK" > "$APP/local.properties"
# Los ejecutables viajan dentro del APK como lib*.so (ver DECISIONS.md, D-43).
mkdir -p "$APP/app/src/main/jniLibs/arm64-v8a"
cp "$DIST/pc-agent" "$APP/app/src/main/jniLibs/arm64-v8a/libpcagent.so"
cp "$DIST/llama-server" "$APP/app/src/main/jniLibs/arm64-v8a/libllamaserver.so"
cp "$DIST/rpc-server" "$APP/app/src/main/jniLibs/arm64-v8a/librpcserver.so"
cp "$DIST/llama-server-v8" "$APP/app/src/main/jniLibs/arm64-v8a/libllamaserver_v8.so"
cp "$DIST/rpc-server-v8" "$APP/app/src/main/jniLibs/arm64-v8a/librpcserver_v8.so"
(cd "$APP" && ./gradlew -q assembleRelease)
cp "$APP/app/build/outputs/apk/release/app-release.apk" "$DIST/phone-cluster.apk"

step "modelo"
MODEL_FILE="$DIST/$(basename "${MODEL_URL%%\?*}")"
if [[ -s "$MODEL_FILE" ]]; then
  echo "ya descargado: $(basename "$MODEL_FILE")"
else
  curl -fL --retry 3 -C - -o "$MODEL_FILE.part" "$MODEL_URL"
  mv "$MODEL_FILE.part" "$MODEL_FILE"
fi
# Solo un modelo en dist/: el coordinador instala el .gguf que encuentre.
find "$DIST" -maxdepth 1 -name '*.gguf' ! -path "$MODEL_FILE" -print -delete

step "listo"
ls -lh "$DIST"
