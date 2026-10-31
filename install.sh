#!/bin/sh
#
# Install prebuilt PidMesh binaries from a GitHub release.
#
# This script downloads one release tarball, verifies its SHA-256 before
# unpacking it, and installs `pidmesh` and `pidmesh-mcp`. It fails closed:
# an unsupported platform, a failed download, a missing checksum or a
# checksum mismatch aborts before anything is written outside a temporary
# directory that is removed on every exit path.
#
# It makes exactly two kinds of network request, both to GitHub: the release
# metadata API, and the release asset downloads. It sends no telemetry and
# reports nothing anywhere.
#
# Usage:
#   sh install.sh [--version <tag>] [--dir <path>]
#
# Environment:
#   PIDMESH_VERSION      release tag to install (default: the latest release)
#   PIDMESH_INSTALL_DIR  where to put the binaries (default: ~/.local/bin)
#   PIDMESH_REPO         owner/name to install from (default: Apurva3509/pidmesh)
#   PIDMESH_API_BASE     GitHub API root (default: https://api.github.com).
#                        Set this for a GitHub Enterprise host, or to point the
#                        script at a test server.

set -eu

REPO="${PIDMESH_REPO:-Apurva3509/pidmesh}"
VERSION="${PIDMESH_VERSION:-latest}"
INSTALL_DIR="${PIDMESH_INSTALL_DIR:-$HOME/.local/bin}"
API_BASE="${PIDMESH_API_BASE:-https://api.github.com}"
BINARIES="pidmesh pidmesh-mcp"

TMPDIR_PIDMESH=""

# Every exit path runs this, so a failure midway leaves nothing behind.
cleanup() {
	if [ -n "$TMPDIR_PIDMESH" ] && [ -d "$TMPDIR_PIDMESH" ]; then
		rm -rf "$TMPDIR_PIDMESH"
	fi
}
trap cleanup EXIT HUP INT TERM

say() {
	printf '%s\n' "$*"
}

# Failures go to stderr and exit non-zero, so a caller that wraps this script
# in `&&` behaves correctly.
die() {
	printf 'install.sh: %s\n' "$*" >&2
	exit 1
}

usage() {
	cat <<'EOF'
Install prebuilt PidMesh binaries from a GitHub release.

Usage:
  sh install.sh [--version <tag>] [--dir <path>]

Options:
  --version <tag>   Release tag to install, for example v1.4.0.
                    Default: the latest published release.
  --dir <path>      Directory to install into. Default: ~/.local/bin
  --help            Show this message.

Environment variables PIDMESH_VERSION, PIDMESH_INSTALL_DIR and PIDMESH_REPO
set the same three values. PIDMESH_API_BASE points the script at a GitHub
Enterprise host instead of api.github.com.

The tarball's SHA-256 is always verified before it is unpacked. If the
checksum cannot be established the script installs nothing.
EOF
}

while [ $# -gt 0 ]; do
	case "$1" in
	--version)
		[ $# -ge 2 ] || die "--version needs a release tag, for example --version v1.4.0"
		VERSION="$2"
		shift 2
		;;
	--version=*)
		VERSION="${1#--version=}"
		shift
		;;
	--dir)
		[ $# -ge 2 ] || die "--dir needs a path"
		INSTALL_DIR="$2"
		shift 2
		;;
	--dir=*)
		INSTALL_DIR="${1#--dir=}"
		shift
		;;
	--help | -h)
		usage
		exit 0
		;;
	*)
		die "unknown option '$1'. Run 'sh install.sh --help' for usage."
		;;
	esac
done

# ---------------------------------------------------------------------------
# Tools
# ---------------------------------------------------------------------------

have() {
	command -v "$1" >/dev/null 2>&1
}

if have curl; then
	DOWNLOADER=curl
elif have wget; then
	DOWNLOADER=wget
else
	die "need curl or wget to download anything, and found neither."
fi

# Fetch $1 into file $2. Returns non-zero on any HTTP error rather than
# writing an error page to disk and letting it fail later as a bad checksum.
fetch() {
	if [ "$DOWNLOADER" = curl ]; then
		curl --fail --silent --show-error --location --retry 3 \
			--output "$2" "$1"
	else
		wget --quiet --tries=3 --output-document="$2" "$1"
	fi
}

# The three common ways to get a SHA-256 on a Unix box. GNU coreutils has
# sha256sum, macOS has shasum, and openssl is the fallback when neither is
# installed. Printing just the digest keeps the comparison simple.
sha256_of() {
	if have sha256sum; then
		sha256sum "$1" | cut -d' ' -f1
	elif have shasum; then
		shasum -a 256 "$1" | cut -d' ' -f1
	elif have openssl; then
		openssl dgst -sha256 "$1" | sed 's/.*= *//'
	else
		die "need sha256sum, shasum or openssl to verify the download, and found none. Refusing to install an unverified binary."
	fi
}

have tar || die "need tar to unpack the release archive, and did not find it."

# ---------------------------------------------------------------------------
# Platform detection
# ---------------------------------------------------------------------------

os="$(uname -s)"
arch="$(uname -m)"

case "$os" in
Linux) os_part="unknown-linux-gnu" ;;
Darwin) os_part="apple-darwin" ;;
*)
	die "unsupported operating system '$os'. PidMesh publishes binaries for Linux and macOS only; build from source with 'cargo install --git https://github.com/$REPO --locked'."
	;;
esac

case "$arch" in
x86_64 | amd64) arch_part="x86_64" ;;
aarch64 | arm64) arch_part="aarch64" ;;
*)
	die "unsupported architecture '$arch'. PidMesh publishes binaries for x86_64 and aarch64 only; build from source with 'cargo install --git https://github.com/$REPO --locked'."
	;;
esac

TARGET="${arch_part}-${os_part}"

# ---------------------------------------------------------------------------
# Resolve the release
# ---------------------------------------------------------------------------

TMPDIR_PIDMESH="$(mktemp -d)" || die "could not create a temporary directory."

if [ "$VERSION" = latest ]; then
	api_url="$API_BASE/repos/$REPO/releases/latest"
else
	api_url="$API_BASE/repos/$REPO/releases/tags/$VERSION"
fi

release_json="$TMPDIR_PIDMESH/release.json"
fetch "$api_url" "$release_json" ||
	die "could not read release metadata from $api_url. Check the tag exists and that you are online."

# GitHub's JSON is normalised to one field per line so the field readers below
# do not depend on the API's pretty-printing. Values we read (tags, file
# names, URLs, digests) contain no commas, spaces or tabs.
flat_json="$TMPDIR_PIDMESH/release.flat"
tr ',' '\n' <"$release_json" | tr -d ' \t' >"$flat_json"

# Read a top-level string field, for example tag_name.
json_field() {
	awk -v field="$1" '
		index($0, "\"" field "\":\"") == 1 {
			line = $0
			sub(/^"[^"]*":"/, "", line)
			sub(/".*$/, "", line)
			print line
			exit
		}
	' "$flat_json"
}

# Read a field belonging to the asset whose "name" is $1. Asset objects list
# "name" before "digest" and "browser_download_url", and the nested uploader
# object has neither, so the first match after the name line is the right one.
asset_field() {
	awk -v name="$1" -v field="$2" '
		$0 == "\"name\":\"" name "\"" { found = 1; next }
		found && index($0, "\"" field "\":\"") == 1 {
			line = $0
			sub(/^"[^"]*":"/, "", line)
			sub(/".*$/, "", line)
			print line
			exit
		}
	' "$flat_json"
}

TAG="$(json_field tag_name)"
[ -n "$TAG" ] || die "could not find a release tag in the response from $api_url."

ARCHIVE="pidmesh-${TAG}-${TARGET}.tar.gz"
archive_url="$(asset_field "$ARCHIVE" browser_download_url)"

if [ -z "$archive_url" ]; then
	published="$(awk '
		index($0, "\"name\":\"pidmesh-") == 1 && index($0, ".tar.gz\"") > 0 {
			line = $0
			sub(/^"[^"]*":"/, "", line)
			sub(/".*$/, "", line)
			print "  " line
		}
	' "$flat_json")"
	say "Release $TAG does not include a binary for $TARGET." >&2
	if [ -n "$published" ]; then
		say "It publishes:" >&2
		say "$published" >&2
	else
		say "It publishes no binary archives at all." >&2
	fi
	say "" >&2
	die "no prebuilt binary for your platform. Build from source instead: cargo install --git https://github.com/$REPO --locked"
fi

say "Installing PidMesh $TAG for $TARGET"

# ---------------------------------------------------------------------------
# Download and verify
# ---------------------------------------------------------------------------

archive_path="$TMPDIR_PIDMESH/$ARCHIVE"
fetch "$archive_url" "$archive_path" ||
	die "could not download $archive_url"

# Preferred source of truth: the .sha256 file the release workflow uploads
# next to the archive. Releases cut before that workflow existed do not have
# one, so fall back to the digest GitHub records for the stored asset. Both
# come from GitHub, so neither defends against a compromised repository; both
# do defend against a corrupted, truncated or tampered-with download.
expected=""
checksum_source=""
sidecar_url="$(asset_field "${ARCHIVE}.sha256" browser_download_url)"

if [ -n "$sidecar_url" ]; then
	sidecar_path="$TMPDIR_PIDMESH/${ARCHIVE}.sha256"
	fetch "$sidecar_url" "$sidecar_path" ||
		die "the release lists ${ARCHIVE}.sha256 but it could not be downloaded. Refusing to install without verifying the archive."
	expected="$(cut -d' ' -f1 <"$sidecar_path")"
	checksum_source="${ARCHIVE}.sha256 published with the release"
else
	digest="$(asset_field "$ARCHIVE" digest)"
	case "$digest" in
	sha256:*)
		expected="${digest#sha256:}"
		checksum_source="the SHA-256 GitHub records for the stored asset (this release predates the checksum files)"
		;;
	esac
fi

[ -n "$expected" ] ||
	die "could not establish a SHA-256 for $ARCHIVE from the release metadata. Refusing to install an unverified binary."

actual="$(sha256_of "$archive_path")"

if [ "$actual" != "$expected" ]; then
	say "Checksum mismatch for $ARCHIVE" >&2
	say "  expected: $expected" >&2
	say "  actual:   $actual" >&2
	say "  source:   $checksum_source" >&2
	say "" >&2
	die "the download does not match its checksum. Nothing was installed. Try again; if it keeps failing, report it at https://github.com/$REPO/issues rather than installing this file."
fi

say "Verified SHA-256 against $checksum_source"

# ---------------------------------------------------------------------------
# Unpack
# ---------------------------------------------------------------------------

unpack_dir="$TMPDIR_PIDMESH/unpacked"
mkdir -p "$unpack_dir"
tar -xzf "$archive_path" -C "$unpack_dir" ||
	die "could not unpack $ARCHIVE. Nothing was installed."

for binary in $BINARIES; do
	[ -f "$unpack_dir/$binary" ] ||
		die "$ARCHIVE does not contain '$binary'. Nothing was installed."
	chmod +x "$unpack_dir/$binary"
done

# ---------------------------------------------------------------------------
# Install
# ---------------------------------------------------------------------------

mkdir -p "$INSTALL_DIR" ||
	die "could not create $INSTALL_DIR"
[ -w "$INSTALL_DIR" ] ||
	die "$INSTALL_DIR is not writable. Choose another directory with --dir, or create it with the permissions you want."

# Staged first, then moved into place, so a half-written binary is never left
# where the shell might find and run it.
for binary in $BINARIES; do
	cp "$unpack_dir/$binary" "$INSTALL_DIR/.$binary.incoming" ||
		die "could not stage $binary in $INSTALL_DIR"
done

for binary in $BINARIES; do
	mv -f "$INSTALL_DIR/.$binary.incoming" "$INSTALL_DIR/$binary" ||
		die "could not install $binary into $INSTALL_DIR"
	say "Installed $INSTALL_DIR/$binary"
done

# ---------------------------------------------------------------------------
# Report
# ---------------------------------------------------------------------------

case ":$PATH:" in
*":$INSTALL_DIR:"*) ;;
*)
	say ""
	say "$INSTALL_DIR is not on your PATH. Add it:"
	say "    export PATH=\"$INSTALL_DIR:\$PATH\""
	;;
esac

say ""
say "Next: pidmesh quickstart"
