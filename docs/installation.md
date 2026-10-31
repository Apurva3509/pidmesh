# Installation

Everything below installs the same two binaries:

- `pidmesh` — the CLI.
- `pidmesh-mcp` — the MCP server, launched by an agent harness over stdio
  rather than run by hand. It takes no arguments at all, so `--version` and
  `--help` do nothing: it starts speaking MCP on stdin immediately and sits
  there waiting for an `initialize` request. A terminal that appears to hang
  is a working `pidmesh-mcp`, not a broken one. Ctrl-C to get out, and use
  `pidmesh --version` to check which build you have.

Nothing here contacts anything but GitHub, and nothing is sent anywhere.
PidMesh has no telemetry, no account, and no network call at runtime.

## Which archive is yours

Release archives are named `pidmesh-<tag>-<target>.tar.gz` and each contains
`pidmesh`, `pidmesh-mcp`, `LICENSE` and `README.md`.

| Your machine | Target in the file name | Published? |
| --- | --- | --- |
| macOS, Apple Silicon (M1 and later) | `aarch64-apple-darwin` | yes |
| macOS, Intel | `x86_64-apple-darwin` | yes |
| Linux, x86-64 | `x86_64-unknown-linux-gnu` | yes |
| Linux, ARM64 (Graviton, Ampere, ARM boards) | `aarch64-unknown-linux-gnu` | **not yet** — build from source |

`uname -sm` tells you which row you are on.

The ARM Linux leg exists in the release workflow but was added after the most
recent release was cut, so no published release carries that archive. It will
appear in the next release. Until then the installer refuses to guess a URL
and tells you to build from source.

## The installer

```bash
curl -fsSL -o pidmesh-install.sh https://raw.githubusercontent.com/Apurva3509/pidmesh/main/install.sh
less pidmesh-install.sh
sh pidmesh-install.sh
```

`install.sh` is POSIX `sh`, about 350 lines, and meant to be read before it is
run. In order it: works out your target from `uname`, reads the release
metadata from the GitHub API, finds the archive for your target, downloads it,
establishes its expected SHA-256, compares that against the file it actually
got, and only then unpacks and installs.

| Option | Environment variable | Default |
| --- | --- | --- |
| `--version <tag>` | `PIDMESH_VERSION` | the latest release |
| `--dir <path>` | `PIDMESH_INSTALL_DIR` | `~/.local/bin` |
| — | `PIDMESH_REPO` | `Apurva3509/pidmesh` |
| — | `PIDMESH_API_BASE` | `https://api.github.com` |

`PIDMESH_API_BASE` points the script at a GitHub Enterprise host.

It installs into a user directory, so it never asks for `sudo`. If
`~/.local/bin` is not on your `PATH` it says so and prints the line to add.

### What it refuses to do

The script fails closed. Each of these aborts with a message on stderr, a
non-zero exit, and nothing written outside a temporary directory that is
removed on every exit path:

- an operating system or CPU it has no target for;
- a release that publishes no archive for your target — it lists what the
  release *does* publish;
- a release tag that does not exist, or any failed download;
- an archive whose SHA-256 does not match the expected digest;
- a release where no checksum can be established at all;
- a machine with no `sha256sum`, `shasum` or `openssl` to check with;
- an install directory it cannot write to.

Binaries are staged next to their destination and moved into place at the end,
so a run that dies halfway never leaves a half-written `pidmesh` somewhere your
shell would find it.

There is deliberately no `curl … | sh` one-liner, in this document or the
README. Piping into a shell executes the script before you have a chance to
read it, which is exactly the trust you are trying to avoid extending.

## Verifying by hand

If you would rather not run someone else's shell script, the whole thing is
four commands.

```bash
TAG=v1.4.0
FILE=pidmesh-$TAG-aarch64-apple-darwin.tar.gz   # your row from the table above

curl -fsSLO "https://github.com/Apurva3509/pidmesh/releases/download/$TAG/$FILE"
```

**Releases carrying a checksum file.** The release workflow writes a `.sha256`
next to each archive using `sha256sum` on Linux runners and `shasum -a 256` on
macOS runners. Both emit the same `<digest>  <name>` line, which both tools
accept back:

```bash
curl -fsSLO "https://github.com/Apurva3509/pidmesh/releases/download/$TAG/$FILE.sha256"

shasum -a 256 -c "$FILE.sha256"   # macOS
sha256sum -c "$FILE.sha256"       # Linux
```

A pass prints `<name>: OK`.

**Releases without one.** `v1.4.0` and everything before it were built before
the workflow learned to publish checksums, so no `.sha256` exists for them.
Compare instead against the digest GitHub recorded for the stored asset, which
is served from `api.github.com` rather than the download host:

```bash
gh release view "$TAG" --repo Apurva3509/pidmesh \
  --json assets -q ".assets[] | select(.name==\"$FILE\") | .digest"

shasum -a 256 "$FILE"             # macOS
sha256sum "$FILE"                 # Linux
```

The first prints `sha256:<digest>`; the second prints the same digest if the
download is intact.

Once, and only once, the two agree:

```bash
tar -xzf "$FILE"
mkdir -p ~/.local/bin
install -m 755 pidmesh pidmesh-mcp ~/.local/bin/
pidmesh --version
```

### What a checksum is and is not worth here

Both the archive and its `.sha256` are uploaded by the same workflow run to
the same release. Anyone able to replace one could replace the other, so
neither proves the release was built from the source you think it was. The
GitHub-recorded digest has the same limitation.

What they do catch, reliably: a truncated download, a corrupted one, a proxy
or CDN that served something else, and a half-written file. Those are the
failures you will actually hit, which is why the installer treats a missing
checksum as a reason to stop rather than a formality to skip.

## From source

Needs a Rust toolchain at or above the MSRV declared in `Cargo.toml`
(`rust-version`, currently 1.88), which CI builds against on every change.

```bash
cargo install --git https://github.com/Apurva3509/pidmesh --locked
```

This compiles SQLite from C — `rusqlite`'s `bundled` feature — so it is not
instant. A clean build of the current tree took **69 seconds** on an M-series
Mac with the crate registry already populated; a first-ever `cargo` run on a
cold machine also downloads the dependency tree and takes longer. Keep
`--locked` so you build the dependency versions that were tested.

This path works on any platform Rust targets, including ARM Linux, and is the
answer whenever a release has no archive for you.

## Troubleshooting

**`pidmesh: command not found` after a successful install.** `~/.local/bin` is
not on your `PATH`. Add `export PATH="$HOME/.local/bin:$PATH"` to your shell
profile, then start a new shell.

**The checksum does not match.** Do not unpack it. Delete the file and try
again — a truncated download is the usual cause. If it fails repeatedly,
report it at <https://github.com/Apurva3509/pidmesh/issues> rather than
working around it.

**macOS refuses to run the binary.** Downloading through a browser attaches a
quarantine flag to the file, and these binaries are not notarised. Fetching
with `curl`, `wget` or the installer does not set that flag, so the simplest
fix is to download with one of those. For a file already quarantined:
`xattr -d com.apple.quarantine ./pidmesh`.

**The installed binary reports an older version than the repository.** Releases
lag `main`. `pidmesh --version` tells you what you have; the from-source path
always gives you the current tree.

## Uninstalling

```bash
rm -f ~/.local/bin/pidmesh ~/.local/bin/pidmesh-mcp
```

Or `cargo uninstall pidmesh` if you installed with `cargo`. PidMesh keeps its
state in a SQLite database inside the workspace it is used in, so removing the
binaries leaves no daemon, service or global configuration behind.

---

# Not yet wired up

Two further install paths are plausible and neither is available today. Both
are the owner's call, and both are recorded here so the decision is a decision
rather than a rediscovery.

## crates.io — `cargo install pidmesh`

**Status:** the crate has never been published. The name `pidmesh` was
unregistered on crates.io as of 2026-10-31.

**It would work.** CI already runs `cargo publish --dry-run --locked` on every
change (the `package` job) and it passes. The dashboard assets the binary
embeds with `include_str!` are committed to the repository rather than
generated by a build script, so they are inside the published `.crate` and the
verify build succeeds without `bun` or Node.

**What publishing takes:** a crates.io account linked to the owner's GitHub, a
token from `cargo login`, then `cargo publish --locked` from a clean checkout
of the tagged commit. Version `1.5.0` would be the first published.

**What it buys:** `cargo install pidmesh --locked` — a shorter, stabler command
than the `--git` form, with real version resolution, so `--version 1.4` means
something. It is still a from-source compile, so it is no faster than the
`--git` path. The prebuilt binaries remain the quick way in.

**Worth knowing first:** publishing is close to irreversible. A version can be
yanked but never deleted, and the name cannot be transferred away casually.
It also publishes the owner's name and email as recorded in the manifest.
Nothing in this branch publishes anything.

## Homebrew — `brew install pidmesh`

**Status:** no formula is committed, deliberately.

A Homebrew formula cannot work without a tap repository to live in, and
`Apurva3509/homebrew-pidmesh` does not exist. A formula file sitting in this
repository would be installable by nobody, would pin checksums for one release
and go stale at the next, and could not be tested by anyone — dead weight. The
recipe is more useful than the artefact:

1. Create a public repository named **`Apurva3509/homebrew-pidmesh`**. The
   `homebrew-` prefix is required; `brew tap Apurva3509/pidmesh` derives the
   repository name from it.
2. Add `Formula/pidmesh.rb` there. For prebuilt binaries it is a `url`,
   `sha256` and `bin.install` per platform, inside `on_macos` / `on_linux` and
   `on_arm` / `on_intel` blocks — four combinations, which is why the missing
   `aarch64-unknown-linux-gnu` archive needs to exist first or that block has
   to be omitted.
3. Point each `url` at the release download and each `sha256` at the matching
   digest. These are the same values the `.sha256` files carry.
4. `brew install --build-from-source` the local formula, then `brew audit
   --strict --new pidmesh` before pushing.
5. Add a release-workflow step that rewrites the formula's URLs and digests on
   every tag and pushes to the tap. Without it the formula silently serves an
   old version, which is worse than having none.

Users would then run:

```bash
brew tap Apurva3509/pidmesh
brew install pidmesh
```

Homebrew's own `homebrew-core` is a separate and much higher bar — it wants
notable usage and builds from source — and is not worth pursuing yet.

---

# For the maintainer: cutting a release

The release workflow fires on a published GitHub release, checks the tag
against `Cargo.toml`, builds all four targets natively, smoke-tests each
binary, and uploads `pidmesh-<tag>-<target>.tar.gz` alongside
`pidmesh-<tag>-<target>.tar.gz.sha256`.

Two things about the current state are worth knowing before the next release:

1. **No published release has checksum files or an ARM Linux archive.** Both
   arrived with the CI change that landed after `v1.4.0` was tagged. The next
   release is the first that will carry them, and it will fix both gaps at
   once. Nothing needs changing in the workflow for that to happen.
2. **`Cargo.toml` is at `1.5.0` and the newest release is `v1.4.0`.** The
   binaries anyone downloads today are a version behind `main`. Tagging
   `v1.5.0` is what closes that, and the workflow's version check means the tag
   has to match the manifest exactly.

To cut one: tag the commit `v1.5.0`, publish a GitHub release for that tag,
and let the workflow do the rest. Then confirm the release has eight assets —
four archives and four checksum files — before announcing it.
