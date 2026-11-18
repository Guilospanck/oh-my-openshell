//! What an agent needs to decide, and to write, a sandbox image: what the sandbox can and cannot do
//! at run time, which needs call for a new image, and the rules a Dockerfile must follow. Served
//! by the `image_guide` tool next to the Dockerfiles osh ships, as examples of the house style.

pub const GUIDE: &str = r#"WHEN A NEW IMAGE IS NEEDED

A sandbox runs as an ordinary user (`node` in osh's images) with no root and no sudo. Its network is
closed except for the hosts a provider or a toolchain group opens (sandbox_options lists them).
So at run time you can only install what a plain user can, from hosts that are open. An image is
built on the user's machine by Docker, as root and with open network, so it is the place for
anything that needs root or a system package.

Make a list of what the work needs, then sort each item:

NEEDS A NEW IMAGE (root, a system package, or a compiler toolchain):
- apt packages of any kind: compilers and linkers (gcc, g++, make, cmake, clang), `-dev`/header
  packages (libssl-dev, libpq-dev, libsqlite3-dev, libffi-dev), pkg-config, build-essential;
- Rust (cargo needs a C linker, `cc`, which the shipped images lack), and any language whose
  normal build needs a C compiler (cgo in Go, Python or Node packages that compile native code);
- browsers and their libraries (chromium, playwright/puppeteer dependencies), fonts, ffmpeg,
  imagemagick, graphviz, LaTeX, and other large system tools;
- databases and services (postgres, redis, mysql clients and servers), anything that needs to
  write under /usr, /etc, /opt or /var;
- a different base system or a specific Linux version, or tools baked in for speed (a project
  that installs many packages on every start).

WORKS AT RUN TIME, NO IMAGE NEEDED (user level, from open hosts):
- Python packages: `pip install --user`, a venv, or `uv` (pure Python and wheels; open the `pypi`
  provider or toolchain group first). A package that must compile native code needs the image.
- Node packages: `npm install` in the project or with `--prefix ~/.local` (open `npm`/`node`).
- Go modules and pure-Go builds with CGO_ENABLED=0, with the Go toolchain unpacked into ~/.local
  (open `go`); cgo needs the image.
- A prebuilt single binary downloaded into ~/.local/bin (check its checksum), a script installed
  under $HOME.
- Anything the shipped image already has: git, gh, curl, python3, node, ripgrep, tmux, just, uv,
  claude (see the Dockerfiles below; `about` in sandbox_options says what each holds).
Tell the user which side each need falls on and why, then let them choose: a new image (slower
first build, then everything works) or installing at run time (nothing to build, within the limits
above). When in doubt, ask for the image: a failed install in the sandbox costs the user more.

WRITING THE DOCKERFILE (image_build takes the whole text)

1. Start from an image the user agrees to. The usual base is claude-code:latest (an image osh
   builds from its own Dockerfile; it holds Claude Code, git, gh, tmux, just, uv, python3 and
   node) so the new image keeps all of that. `FROM claude-code:latest` works because osh builds
   that base first when it is missing. A public image also works but lacks those tools.
2. Switch to root to install, then back: `USER root`, the installs, then `USER node` as the last
   USER line. An image that ends as root is flagged.
3. Debian trixie (apt): `apt-get update && apt-get install --yes --no-install-recommends ... &&
   rm -rf /var/lib/apt/lists/*` in one RUN. Use `dpkg --print-architecture` for downloads, since
   the user's machine can be arm64 or amd64.
4. Pin and check what you download: a fixed version, and `sha256sum --check` against a checksum
   published by the project. No `curl | sh` in a RUN.
5. Never put a credential, token, key or personal file in the image; nothing is available to
   COPY (the build context is empty), so fetch with RUN. Do not use ADD for local files.
6. Do not set an ENTRYPOINT or CMD that starts a long process. Leave the user's WORKDIR and
   HOME as the base has them.
7. Keep it small and quiet: install only what the work needs, clean caches, one concern per RUN
   layer where it helps rebuilds.
8. A sandbox does NOT apply the Dockerfile's ENV (nor shell profiles): its shell and sandbox_exec
   start with a plain environment, so a tool that works in the image only through ENV PATH or
   ENV SOME_HOME fails there ("command not found", exit 127). Make every tool work with no
   variables: put (or symlink) the binaries in /usr/local/bin, and when a tool needs a variable,
   either make its default location the right one or put a small wrapper script in
   /usr/local/bin that exports the variable and execs the real binary. Make the files readable
   and any cache or state folder writable by the non-root user (`chmod -R a+rwX`).
9. Name it for what it holds (`claude-code-rust`). The image is NAME:latest. osh never overwrites
   an existing image: pick a new name for a change.

RESEARCH FIRST, SO THE FIRST IMAGE IS RIGHT

The user must never learn after the build that something was missing. Before you write the
Dockerfile, for every tool the work needs, find out from its official install documentation
(search the web) how it installs and what it needs to RUN: the PATH entry, environment
variables, default home, config and cache folders, system libraries and a linker or compiler, TLS
certificates, and the hosts it contacts at run time (those must be open: sandbox_options). Put
all of it in the Dockerfile, applying rule 8. If you cannot find out, say so to the user rather
than guess. The same holds for a run-time install: after it, run the tool by name and by absolute
path, since ~/.local/bin or a cargo or npm bin folder is not necessarily on PATH.

BEFORE BUILDING

osh runs the runtime's own parser and lint rules over it first (`build --check`): a syntax error comes back as an error to fix, lint warnings as a `note` to fix or explain.
Call image_build with dry_run true and PASTE the complete Dockerfile and the verify commands to
the user in a code block, with the base and name you chose: a summary is not enough, and tool
output is collapsed in their view. Build only after they agree; a build takes minutes and
downloads a lot. `verify` is required for a build: the commands the work needs (`cargo --version`,
`cc --version`, a hello-world compile, `python -c "import x"`). osh runs each in the new image with
a clean environment, as the sandbox will, and removes the image if one fails, so you fix the
Dockerfile and call again with the same name. After a verified build, create the sandbox with
spec.image set to the new tag and run the work's command inside it once.

EXAMPLE (Rust, in the same style): the shipped Dockerfiles below show the conventions: pinned
versions, checksums, `dpkg --print-architecture`, apt cleanup. A Rust image adds, as root: gcc,
libc6-dev, make, pkg-config, libssl-dev from apt; rustup-init from
https://static.rust-lang.org/rustup/dist/<triple>/ checked against its `.sha256`, installed with
RUSTUP_HOME=/usr/local/rustup and CARGO_HOME=/usr/local/cargo, the minimal profile plus clippy
and rustfmt, `chmod -R a+rwX` on both, then, because ENV does not reach the sandbox, wrapper
scripts /usr/local/bin/cargo, rustc, rustup and so on that export both variables and exec the real
binaries from /usr/local/cargo/bin; verify `cargo --version` and `rustc --version`; then USER node.
"#;
