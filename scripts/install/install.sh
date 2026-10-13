#!/bin/sh
# Bootstrap the native Fai installer from one immutable GitHub release.

fai_install() {
  set -eu
  release='@VERSION@'
  version=$release
  modify_path=yes
  while [ "$#" -gt 0 ]; do
    case "$1" in
      --version)
        [ "$#" -ge 2 ] || { echo '--version requires a value' >&2; return 2; }
        version=$2; shift 2 ;;
      --home)
        [ "$#" -ge 2 ] || { echo '--home requires a directory' >&2; return 2; }
        FAI_HOME=$2; export FAI_HOME; shift 2 ;;
      --no-modify-path) modify_path=no; shift ;;
      --help|-h)
        echo 'Usage: install.sh [--version VERSION|stable] [--home DIRECTORY] [--no-modify-path]'
        return 0 ;;
      *) echo "Unknown installer option: $1" >&2; return 2 ;;
    esac
  done
  os=$(uname -s)
  arch=$(uname -m)
  case "$arch" in x86_64|amd64) arch=x86_64 ;; arm64|aarch64) arch=aarch64 ;; *) echo "Unsupported architecture: $arch" >&2; return 1 ;; esac
  case "$os" in
    Linux)
      [ "$(getconf LONG_BIT)" = 64 ] || { echo 'Fai requires a 64-bit userland' >&2; return 1; }
      libc=$(getconf GNU_LIBC_VERSION 2>/dev/null) || { echo 'This release requires glibc 2.35 or newer' >&2; return 1; }
      libc=${libc#* }; major=${libc%%.*}; minor=${libc#*.}; minor=${minor%%.*}
      if [ "$major" -lt 2 ] || { [ "$major" -eq 2 ] && [ "$minor" -lt 35 ]; }; then
        echo 'This release requires glibc 2.35 or newer' >&2; return 1
      fi
      target=$arch-unknown-linux-gnu ;;
    Darwin)
      if [ "$(sysctl -n hw.optional.arm64 2>/dev/null || true)" = 1 ]; then arch=aarch64; fi
      system=$(sw_vers -productVersion)
      [ "${system%%.*}" -ge 15 ] || { echo 'This release requires macOS 15 or newer' >&2; return 1; }
      target=$arch-apple-darwin ;;
    *) echo 'Use install.ps1 on Windows; this installer supports Linux and macOS' >&2; return 1 ;;
  esac
  base=${FAI_RELEASE_BASE_URL:-https://github.com/Krzysztof-Cieslak/fai/releases}
  case "$base" in
    https://*) protocols='=https' ;;
    http://127.0.0.1:*|http://localhost:*) protocols='=http,https' ;;
    *) echo 'Release URLs require HTTPS' >&2; return 1 ;;
  esac
  temporary=$(mktemp -d "${TMPDIR:-/tmp}/fai-install.XXXXXXXX")
  trap 'rm -rf "$temporary"' 0
  trap 'exit 1' 1 2 15
  download() {
    if command -v curl >/dev/null 2>&1; then
      curl --proto "$protocols" --tlsv1.2 -fLsS --retry 3 --connect-timeout 20 --max-time 300 "$1" -o "$2"
    elif command -v wget >/dev/null 2>&1; then
      if [ "$protocols" = '=https' ]; then
        wget --https-only -q --timeout=30 -O "$2" "$1"
      else
        wget -q --timeout=30 -O "$2" "$1"
      fi
    else
      echo 'Install curl or wget to download Fai' >&2; return 1
    fi
  }
  asset=fai-installer-$release-$target.tar.gz
  download "$base/download/v$release/SHA256SUMS" "$temporary/SHA256SUMS"
  download "$base/download/v$release/$asset" "$temporary/$asset"
  expected=$(awk -v name="$asset" '$2 == name { print $1; count++ } END { if (count != 1) exit 1 }' "$temporary/SHA256SUMS")
  if command -v sha256sum >/dev/null 2>&1; then
    actual=$(sha256sum "$temporary/$asset"); actual=${actual%% *}
  elif command -v shasum >/dev/null 2>&1; then
    actual=$(shasum -a 256 "$temporary/$asset"); actual=${actual%% *}
  else
    echo 'Install sha256sum or shasum to verify Fai' >&2; return 1
  fi
  [ "$expected" = "$actual" ] || { echo 'Installer checksum mismatch' >&2; return 1; }
  members=$(tar -tzf "$temporary/$asset" | LC_ALL=C sort)
  [ "$members" = "$(printf 'LICENSE\nbin/fai\nmetadata.json')" ] || { echo 'Invalid installer archive members' >&2; return 1; }
  tar -tvzf "$temporary/$asset" | awk 'substr($1,1,1) != "-" { exit 1 }'
  tar -xzf "$temporary/$asset" -C "$temporary"
  if [ "$modify_path" = yes ]; then
    "$temporary/bin/fai" installer setup --version "$version"
  else
    "$temporary/bin/fai" installer setup --version "$version" --no-modify-path
  fi
  rm -rf "$temporary"
  trap - 0 1 2 15
}

fai_install "$@"
