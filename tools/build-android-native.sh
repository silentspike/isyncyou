#!/usr/bin/env bash
set -euo pipefail
umask 077

ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
JNI_DIR="$ROOT/android/app/src/main/jniLibs"
NDK_VERSION=${ISY_NDK_VERSION:-27.3.13750724}
RUST_TOOLCHAIN=${ISY_RUST_TOOLCHAIN:-1.95.0}
REMOTE_NDK=${ISY_REMOTE_ANDROID_NDK_HOME:-/opt/android-ndk-r27d}
BUILDER=${ISY_ANDROID_NATIVE_BUILDER:-remote}
ABIS=${ISY_ANDROID_ABIS:-arm64-v8a}
FEATURES=${ISY_CARGO_FEATURES:-}

die() {
  printf 'android native build failed: %s\n' "$1" >&2
  exit 2
}

contains() {
  local needle=$1
  shift
  local value
  for value in "$@"; do
    [[ $value == "$needle" ]] && return 0
  done
  return 1
}

allowed_features=(
  agent-account-lifecycle-device-test-hooks
  agent-credential-store-self-test
  agent-network-device-test-hooks
  agent-session-kdf-bench
  mobile-job-device-test-hooks
)

IFS=',' read -r -a abi_values <<<"$ABIS"
IFS=',' read -r -a feature_values <<<"$FEATURES"
(( ${#abi_values[@]} > 0 )) || die "at least one ABI is required"

declare -A targets=(
  [arm64-v8a]=aarch64-linux-android
  [x86_64]=x86_64-linux-android
)
declare -A linkers=(
  [arm64-v8a]=aarch64-linux-android24-clang
  [x86_64]=x86_64-linux-android24-clang
)

for abi in "${abi_values[@]}"; do
  [[ -n $abi && ${targets[$abi]+present} ]] || die "unsupported ABI '$abi'"
done
for feature in "${feature_values[@]}"; do
  [[ -z $feature ]] && continue
  contains "$feature" "${allowed_features[@]}" || die "unsupported feature '$feature'"
done

mapfile -t canonical_abis < <(printf '%s\n' "${abi_values[@]}" | LC_ALL=C sort -u)
if (( ${#canonical_abis[@]} != ${#abi_values[@]} )); then
  die "duplicate ABI requested"
fi
if [[ -n $FEATURES ]]; then
  mapfile -t canonical_features < <(printf '%s\n' "${feature_values[@]}" | LC_ALL=C sort -u)
  if (( ${#canonical_features[@]} != ${#feature_values[@]} )); then
    die "duplicate feature requested"
  fi
else
  canonical_features=()
fi

cd "$ROOT"
native_status=$(git status --porcelain --untracked-files=all -- Cargo.toml Cargo.lock crates gui/webui)
[[ -z $native_status ]] || die "Rust/WebUI inputs are dirty; commit them before producing a bound APK artifact"
source_commit=$(git rev-parse HEAD)

stage=$(mktemp -d "$ROOT/android/app/src/main/.jniLibs-stage.XXXXXX")
trap 'rm -rf -- "$stage"' EXIT INT TERM

feature_args=()
if (( ${#canonical_features[@]} > 0 )); then
  feature_args=(--features "$(IFS=,; printf '%s' "${canonical_features[*]}")")
fi

for abi in "${canonical_abis[@]}"; do
  target=${targets[$abi]}
  mkdir -p "$stage/$abi"

  case $BUILDER in
    remote)
      remote_bin="$REMOTE_NDK/toolchains/llvm/prebuilt/linux-x86_64/bin"
      mkdir -p "$ROOT/target/$target/release"
      target_env=$(printf 'RUST_BACKTRACE=1 RUSTUP_TOOLCHAIN=%s CARGO_TARGET_%s_LINKER=%s/%s CC_%s=%s/%s AR_%s=%s/llvm-ar' \
        "$RUST_TOOLCHAIN" "$(printf '%s' "$target" | tr '[:lower:]-' '[:upper:]_')" \
        "$remote_bin" "${linkers[$abi]}" \
        "$(printf '%s' "$target" | tr '-' '_')" "$remote_bin" "${linkers[$abi]}" \
        "$(printf '%s' "$target" | tr '-' '_')" "$remote_bin")
      cargo remote --no-copy-lock -d "$RUST_TOOLCHAIN" -b "$target_env" \
        -c "$target/release/libisyncyou_mobile.so" -- \
        build --locked --release -p isyncyou-mobile --target "$target" "${feature_args[@]}"
      source_library="$ROOT/target/$target/release/libisyncyou_mobile.so"
      ;;
    github-actions)
      [[ ${CI:-} == true && ${GITHUB_ACTIONS:-} == true ]] || \
        die "the github-actions backend is forbidden outside GitHub Actions"
      local_ndk=${ANDROID_NDK_HOME:-${ANDROID_NDK_ROOT:-${ANDROID_HOME:-}/ndk/$NDK_VERSION}}
      [[ -n $local_ndk ]] || die "ANDROID_NDK_HOME is required on the GitHub runner"
      cargo "+$RUST_TOOLCHAIN" ndk -t "$abi" -o "$stage" \
        build --locked --release -p isyncyou-mobile "${feature_args[@]}"
      source_library="$stage/$abi/libisyncyou_mobile.so"
      ;;
    *)
      die "unknown ISY_ANDROID_NATIVE_BUILDER '$BUILDER'"
      ;;
  esac

  [[ -s $source_library ]] || die "native library was not produced for '$abi'"
  if [[ $source_library != "$stage/$abi/libisyncyou_mobile.so" ]]; then
    cp -- "$source_library" "$stage/$abi/libisyncyou_mobile.so"
  fi
done

{
  printf 'schema=1\n'
  printf 'source_commit=%s\n' "$source_commit"
  printf 'abis=%s\n' "$(IFS=,; printf '%s' "${canonical_abis[*]}")"
  printf 'features=%s\n' "$(IFS=,; printf '%s' "${canonical_features[*]}")"
  printf 'ndk_version=%s\n' "$NDK_VERSION"
  printf 'rust_toolchain=%s\n' "$RUST_TOOLCHAIN"
  printf 'builder=%s\n' "$BUILDER"
  for abi in "${canonical_abis[@]}"; do
    printf 'sha256.%s=%s\n' "$abi" "$(sha256sum "$stage/$abi/libisyncyou_mobile.so" | awk '{print $1}')"
  done
} >"$stage/isyncyou-native.properties"

rm -rf -- "$JNI_DIR"
mv -- "$stage" "$JNI_DIR"
trap - EXIT INT TERM
printf 'android native artifact: PASS (%s; %s)\n' \
  "$(IFS=,; printf '%s' "${canonical_abis[*]}")" "$BUILDER"
