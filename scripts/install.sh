#!/bin/sh
# Install `eks` from a GitHub release, after checking the tarball against its
# published SHA-256 checksum.
#
#   curl -fsSL https://raw.githubusercontent.com/nmcginn/eks-wrangler/master/scripts/install.sh | sh
#   curl -fsSL .../install.sh | sh -s -- --version 0.2.0 --prefix /usr/local
#
# POSIX sh rather than bash, because `curl | sh` runs whatever `sh` is — dash on
# Debian and Ubuntu, BusyBox in Alpine. Nothing is written outside a temporary
# directory until the checksum has matched and the binary has started, so a
# failed install leaves an existing `eks` exactly as it was.
#
# Environment (each has a flag of the same meaning, which wins):
#   EKS_VERSION        release to install, e.g. 0.2.0 or v0.2.0 (default: latest)
#   EKS_PREFIX         install under PREFIX/bin and PREFIX/share (default: ~/.local)
#   EKS_TARGET         release target to fetch, overriding detection
#   EKS_DOWNLOAD_URL   releases base URL, for a mirror (default: GitHub's)
set -eu

repo_url="https://github.com/nmcginn/eks-wrangler"

usage() {
  cat <<EOF
Install eks from a GitHub release, verifying its SHA-256 checksum.

Usage: install.sh [--version VERSION] [--prefix DIR] [--target TRIPLE]

  --version VERSION  release to install, e.g. 0.2.0 (default: the latest)
  --prefix DIR       install into DIR/bin, DIR/share/man, ... (default: ~/.local)
  --target TRIPLE    release target to fetch instead of detecting this machine's
  -h, --help         show this help
EOF
}

say() { printf 'eks install: %s\n' "$*"; }
die() {
  printf 'eks install: %s\n' "$*" >&2
  exit 1
}
usage_error() {
  printf 'eks install: %s\n\n' "$*" >&2
  usage >&2
  exit 2
}

version="${EKS_VERSION:-latest}"
prefix="${EKS_PREFIX:-}"
target="${EKS_TARGET:-}"
base_url="${EKS_DOWNLOAD_URL:-$repo_url/releases}"

while [ "$#" -gt 0 ]; do
  case "$1" in
    --version | --prefix | --target)
      [ "$#" -ge 2 ] && [ -n "$2" ] || usage_error "$1 needs a value"
      case "$1" in
        --version) version="$2" ;;
        --prefix) prefix="$2" ;;
        --target) target="$2" ;;
      esac
      shift 2
      ;;
    --version=* ) version="${1#*=}"; shift ;;
    --prefix=* ) prefix="${1#*=}"; shift ;;
    --target=* ) target="${1#*=}"; shift ;;
    -h | --help)
      usage
      exit 0
      ;;
    *) usage_error "unknown option '$1'" ;;
  esac
done

if [ -z "$prefix" ]; then
  [ -n "${HOME:-}" ] || die "HOME is not set, so there is no default install location — pass --prefix DIR"
  prefix="$HOME/.local"
fi

from_source="build it from source instead: cargo install --locked --git $repo_url"

# Maps this machine to one of the targets release.yml publishes. x86_64 Linux
# gets the static musl build rather than the -gnu one: it runs on every
# distribution, glibc or not, and starts without a dynamic loader. aarch64
# Linux has only a -gnu build, so a musl system (Alpine) is told so up front
# rather than handed a binary that cannot start.
detect_target() {
  os="$(uname -s)"
  arch="$(uname -m)"
  case "$arch" in
    x86_64 | amd64) arch=x86_64 ;;
    arm64 | aarch64) arch=aarch64 ;;
    *) die "there is no release build for $arch processors — $from_source" ;;
  esac
  case "$os" in
    Darwin)
      # A shell running under Rosetta reports x86_64 on Apple silicon; the
      # native build is the one to install there.
      if [ "$arch" = x86_64 ] && [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || true)" = 1 ]; then
        arch=aarch64
      fi
      echo "$arch-apple-darwin"
      ;;
    Linux)
      if [ "$arch" = x86_64 ]; then
        echo "x86_64-unknown-linux-musl"
      elif (ldd --version 2>&1 || true) | grep -qi musl; then
        die "there is no release build for aarch64 Linux with musl libc (Alpine) — $from_source"
      else
        echo "aarch64-unknown-linux-gnu"
      fi
      ;;
    *) die "there is no release build for $os — $from_source" ;;
  esac
}

if [ -z "$target" ]; then
  target="$(detect_target)"
fi
case "$target" in
  x86_64-unknown-linux-gnu | x86_64-unknown-linux-musl | aarch64-unknown-linux-gnu | \
    aarch64-apple-darwin | x86_64-apple-darwin) ;;
  *) usage_error "'$target' is not a release target — pick one of x86_64-unknown-linux-gnu, x86_64-unknown-linux-musl, aarch64-unknown-linux-gnu, aarch64-apple-darwin, x86_64-apple-darwin" ;;
esac

asset="eks-$target.tar.gz"
case "$version" in
  latest) url="$base_url/latest/download/$asset" ;;
  v[0-9]* | [0-9]*)
    version="${version#v}"
    url="$base_url/download/v$version/$asset"
    ;;
  *) usage_error "'$version' is not a version — pass one like 0.2.0, or latest" ;;
esac

# Downloads $1 to $2, with curl or else wget. `-f` turns a 404 into a failure
# rather than an HTML error page saved under the tarball's name.
fetch() {
  if command -v curl > /dev/null 2>&1; then
    curl -fsSL --proto '=https' --tlsv1.2 --retry 3 -o "$2" "$1"
  elif command -v wget > /dev/null 2>&1; then
    wget -q --https-only -O "$2" "$1"
  else
    die "neither curl nor wget is installed — install one of them and run this again"
  fi
}

sha256_of() {
  if command -v sha256sum > /dev/null 2>&1; then
    sha256sum "$1" | cut -d ' ' -f 1
  elif command -v shasum > /dev/null 2>&1; then
    shasum -a 256 "$1" | cut -d ' ' -f 1
  else
    return 1
  fi
}

work="$(mktemp -d 2> /dev/null || mktemp -d -t eks-install)"
trap 'rm -rf "$work"' EXIT
trap 'exit 130' INT TERM

say "downloading $asset ($version)"
fetch "$url" "$work/$asset" ||
  die "could not download $url — check that release $version exists at $repo_url/releases and that this machine can reach github.com"
fetch "$url.sha256" "$work/$asset.sha256" ||
  die "could not download the checksum at $url.sha256, so the download cannot be verified — nothing was installed"

# The checksum file is `shasum` output: the digest, then the file name. Only
# the digest is read, so the name inside it (with or without a directory)
# does not have to match where the tarball was saved.
expected="$(cut -d ' ' -f 1 < "$work/$asset.sha256" | head -n 1 | tr 'A-F' 'a-f')"
case "$expected" in
  *[!0-9a-f]* | "") die "$url.sha256 does not hold a SHA-256 digest, so the download cannot be verified — nothing was installed" ;;
esac
[ "${#expected}" -eq 64 ] ||
  die "$url.sha256 does not hold a SHA-256 digest, so the download cannot be verified — nothing was installed"

actual="$(sha256_of "$work/$asset")" ||
  die "neither sha256sum nor shasum is installed, so the download cannot be verified — install coreutils (or perl, for shasum) and run this again"
if [ "$actual" != "$expected" ]; then
  die "checksum mismatch for $asset: expected $expected, got $actual. The download is corrupt or was altered in transit — nothing was installed. Run this again; if it keeps happening, report it at $repo_url/issues"
fi
say "checksum verified ($actual)"

tar -xzf "$work/$asset" -C "$work" || die "$asset is not a readable tarball — nothing was installed"
unpacked="$work/eks-$target"
[ -f "$unpacked/eks" ] || die "$asset has no eks binary inside it — nothing was installed; report it at $repo_url/issues"

# Starting it before installing it catches the binary that cannot run here —
# a glibc too old, the wrong architecture forced through --target — while the
# old `eks`, if any, is still in place.
if ! ran="$("$unpacked/eks" --version 2>&1)"; then
  die "the downloaded eks does not run on this machine ($ran) — nothing was installed. $from_source"
fi

# Copies $1 to $2 through a temporary name in $2's own directory, then renames
# it into place: a rename is atomic, so a running `eks` is never overwritten
# half-way and a failed copy never leaves a truncated file at $2.
place() {
  mkdir -p "$(dirname "$2")" || die "could not create $(dirname "$2") — pick a writable location with --prefix DIR"
  cp "$1" "$2.tmp.$$" && chmod "$3" "$2.tmp.$$" && mv -f "$2.tmp.$$" "$2" || {
    rm -f "$2.tmp.$$"
    die "could not write $2 — pick a writable location with --prefix DIR"
  }
}

bin="$prefix/bin/eks"
place "$unpacked/eks" "$bin" 755
say "installed $ran to $bin"

# The one tarball built without completions or a man page (x86_64-apple-darwin,
# cross-compiled on a runner that cannot run it) gets them from the binary
# itself, which by now has proven it runs here.
extra() {
  if [ -f "$unpacked/$1" ]; then
    place "$unpacked/$1" "$2" 644
  elif "$bin" $3 > "$work/generated" 2> /dev/null; then
    place "$work/generated" "$2" 644
  else
    say "skipped $2: this release could not produce it"
    return 0
  fi
  say "installed $2"
}
share="$prefix/share"
extra eks.1 "$share/man/man1/eks.1" "man"
extra completions/eks.bash "$share/bash-completion/completions/eks" "completions bash"
extra completions/_eks "$share/zsh/site-functions/_eks" "completions zsh"
extra completions/eks.fish "$share/fish/vendor_completions.d/eks.fish" "completions fish"

case ":${PATH:-}:" in
  *":$prefix/bin:"*) ;;
  *)
    say "$prefix/bin is not on your PATH. Add it to your shell's startup file:"
    say "  export PATH=\"$prefix/bin:\$PATH\""
    ;;
esac
case "${SHELL:-}" in
  */zsh)
    say "for zsh completions, add this before compinit in ~/.zshrc:"
    say "  fpath=($share/zsh/site-functions \$fpath)"
    ;;
esac
say "done — run 'eks' to open the dashboard"
