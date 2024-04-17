#!/bin/bash

#
# Copyright 2020-2021 Signal Messenger, LLC.
# SPDX-License-Identifier: AGPL-3.0-only
#

set -euo pipefail

SCRIPT_DIR=$(dirname "$0")
cd "${SCRIPT_DIR}"/..
. bin/build_helpers.sh

export CARGO_PROFILE_RELEASE_DEBUG=1 # enable line tables
export CFLAGS="-DOPENSSL_SMALL ${CFLAGS:-}" # use small BoringSSL curve tables to reduce binary size

if [[ -n "${CARGO_BUILD_TARGET:-}" ]]; then
  # Avoid overriding RUSTFLAGS for host builds, because that resets the incremental build.
  export RUSTFLAGS="--cfg aes_armv8 --cfg polyval_armv8 ${RUSTFLAGS:-}" # Enable ARMv8 cryptography acceleration when available
fi

# Work around cc crate bug with Catalyst targets
export CFLAGS_aarch64_apple_ios_macabi="--target=arm64-apple-ios-macabi ${CFLAGS}"
export CFLAGS_x86_64_apple_ios_macabi="--target=x86_64-apple-ios-macabi ${CFLAGS}"

# For some reason, 'ring' symbols (rustls dependency) don't get linked properly without using LTO.
export CARGO_PROFILE_DEV_LTO=thin

if [[ "${CARGO_BUILD_TARGET:-}" =~ -ios(-sim|-macabi)?$ ]]; then
  export IPHONEOS_DEPLOYMENT_TARGET=13
  # Use full LTO to reduce binary size
  export CARGO_PROFILE_RELEASE_LTO=fat
  export CFLAGS="-flto=full ${CFLAGS:-}"
else
  # Matches the "dev" setting above.
  export CARGO_PROFILE_RELEASE_LTO=thin
fi

if [[ "${CARGO_BUILD_TARGET:-}" != "aarch64-apple-ios" ]]; then
  FEATURES="testing-fns"
fi

usage() {
  cat >&2 <<END
Usage: $(basename "$0") [-d|-r] [-v] [--generate-ffi|--verify-ffi|--build-std]

Options:
  -d -- debug build (default)
  -r -- release build
  -v -- verbose build

  --generate-ffi -- regenerate ffi headers
  --verify-ffi   -- verify that ffi headers are up to date
  --build-std    -- use Cargo's -Zbuild-std to compile for a tier 3 target

Use CARGO_BUILD_TARGET for cross-compilation (such as for iOS).
END
}

check_cbindgen() {
  if ! command -v cbindgen > /dev/null; then
    echo 'error: cbindgen not found in PATH' >&2
    if command -v cargo > /dev/null; then
      echo 'note: get it by running' >&2
      printf "\n\t%s\n\n" "cargo install cbindgen --vers '^0.16'" >&2
    fi
    exit 1
  fi
}


RELEASE_BUILD=
VERBOSE=
SHOULD_CBINDGEN=
CBINDGEN_VERIFY=
BUILD_STD=

while [ "${1:-}" != "" ]; do
  case $1 in
    -d | --debug )
      RELEASE_BUILD=
      ;;
    -r | --release )
      RELEASE_BUILD=1
      ;;
    -v | --verbose )
      VERBOSE=1
      ;;
    --generate-ffi )
      SHOULD_CBINDGEN=1
      ;;
    --verify-ffi )
      SHOULD_CBINDGEN=1
      CBINDGEN_VERIFY=1
      ;;
    --build-std)
      BUILD_STD=1
      ;;
    -h | --help )
      usage
      exit
      ;;
    * )
      usage
      exit 2
  esac
  shift
done

check_rust

if [[ -n "${DEVELOPER_SDK_DIR:-}" ]]; then
  # Assume we're in Xcode, which means we're probably cross-compiling.
  # In this case, we need to add an extra library search path for build scripts and proc-macros,
  # which run on the host instead of the target.
  # (macOS Big Sur does not have linkable libraries in /usr/lib/.)
  export LIBRARY_PATH="${DEVELOPER_SDK_DIR}/MacOSX.sdk/usr/lib:${LIBRARY_PATH:-}"
fi

if [[ -n "${BUILD_STD:-}" ]]; then
  RUSTUP_TOOLCHAIN=${RUSTUP_TOOLCHAIN:-$(cat ./rust-toolchain)}
  if ! rustup "+${RUSTUP_TOOLCHAIN}" component list --installed | grep -q rust-src; then
    echo 'error: rust-src component not installed' >&2
    echo 'note: get it by running' >&2
    printf "\n\t%s\n\n" "rustup +${RUSTUP_TOOLCHAIN} component add rust-src" >&2
    exit 1
  fi
fi

echo_then_run cargo build -p libsignal-ffi ${RELEASE_BUILD:+--release} ${VERBOSE:+--verbose} ${CARGO_BUILD_TARGET:+--target $CARGO_BUILD_TARGET} ${FEATURES:+--features $FEATURES} ${BUILD_STD:+-Zbuild-std}

FFI_HEADER_PATH=swift/Sources/SignalFfi/signal_ffi.h

if [[ -n "${SHOULD_CBINDGEN}" ]]; then
  check_cbindgen
  cbindgen --version
  if [[ -n "${CBINDGEN_VERIFY}" ]]; then
    echo diff -u "${FFI_HEADER_PATH}" "<(cbindgen -q ${RELEASE_BUILD:+--profile release} rust/bridge/ffi)"
    if ! diff -u "${FFI_HEADER_PATH}"  <(cbindgen -q ${RELEASE_BUILD:+--profile release} rust/bridge/ffi); then
      echo
      echo 'error: signal_ffi.h not up to date; run' "$0" '--generate-ffi' >&2
      exit 1
    fi
  else
    echo cbindgen ${RELEASE_BUILD:+--profile release} -o "${FFI_HEADER_PATH}" rust/bridge/ffi
    # Use sed to ignore irrelevant cbindgen warnings.
    # ...and then disable the shellcheck warning about literal backticks in single-quotes
    # shellcheck disable=SC2016
    cbindgen ${RELEASE_BUILD:+--profile release} -o "${FFI_HEADER_PATH}" rust/bridge/ffi 2>&1 |
      sed '/WARN: Missing `\[defines\]` entry for `feature = "ffi"` in cbindgen config\./ d' >&2
  fi
fi
