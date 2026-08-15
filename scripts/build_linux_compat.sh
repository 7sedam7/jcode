#!/usr/bin/env bash
set -euo pipefail

# Build a Linux release artifact matching the host architecture against the
# CentOS 7 / manylinux2014 glibc 2.17 baseline. Set JCODE_COMPAT_ARCH to
# x86_64 or aarch64 to override host detection.

repo_root="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
out_dir="${1:-$repo_root/dist}"

if [[ "$#" -gt 1 ]]; then
  echo "Usage: $0 [out-dir]" >&2
  exit 1
fi

if [[ "$out_dir" != /* ]]; then
  out_dir="$repo_root/$out_dir"
fi

profile="${JCODE_COMPAT_PROFILE:-release}"
cache_root="${JCODE_COMPAT_CACHE_DIR:-$HOME/.cache/jcode-linux-compat}"

arch="${JCODE_COMPAT_ARCH:-$(uname -m)}"
case "$arch" in
  x86_64|amd64)
    arch="x86_64"
    docker_platform="linux/amd64"
    default_artifact="jcode-linux-x86_64"
    default_image="quay.io/pypa/manylinux2014_x86_64"
    target="x86_64-unknown-linux-gnu"
    ;;
  arm64|aarch64)
    arch="aarch64"
    docker_platform="linux/arm64"
    default_artifact="jcode-linux-aarch64"
    default_image="quay.io/pypa/manylinux2014_aarch64"
    target="aarch64-unknown-linux-gnu"
    ;;
  *)
    echo "Unsupported Linux compatibility architecture: $arch" >&2
    echo "Set JCODE_COMPAT_ARCH to x86_64 or aarch64." >&2
    exit 1
    ;;
esac

artifact="${JCODE_COMPAT_ARTIFACT:-$default_artifact}"
image="${JCODE_COMPAT_IMAGE:-$default_image}"

mkdir -p "$out_dir" \
  "$cache_root/cargo-registry" \
  "$cache_root/cargo-git" \
  "$cache_root/rustup-$arch"

host_uid="$(id -u)"
host_gid="$(id -g)"

# Compute git build metadata on the HOST and hand it to the container via a
# metadata file (read by jcode-build-meta/build.rs through
# JCODE_BUILD_METADATA_FILE). The repo is bind-mounted into the container and
# owned by the host UID while git inside the container runs as root, so any
# in-container `git` call trips git's "dubious ownership" guard
# (CVE-2022-24765) and fails. That previously zeroed out the embedded git hash,
# date, AND changelog, shipping release binaries that report
# "vX.Y.Z (unknown) (unknown)" with an empty /changelog overlay. Computing the
# values here makes the embedded metadata independent of container-git. This
# mirrors scripts/remote_build.sh.
git_hash=""
git_date=""
git_tag=""
git_dirty="0"
changelog_raw=""
if command -v git >/dev/null 2>&1 && git -C "$repo_root" rev-parse --git-dir >/dev/null 2>&1; then
  git_hash="$(git -C "$repo_root" rev-parse --short HEAD 2>/dev/null || true)"
  git_date="$(git -C "$repo_root" log -1 --format=%ci 2>/dev/null || true)"
  git_tag="$(git -C "$repo_root" describe --tags --always 2>/dev/null || true)"
  changelog_raw="$(git -C "$repo_root" log -700 --format='%h|%ct|%D|%s' 2>/dev/null || true)"
  if [[ -n "$(git -C "$repo_root" status --porcelain 2>/dev/null || true)" ]]; then
    git_dirty="1"
  fi
else
  echo "warning: git metadata unavailable on host; embedded changelog/version may be empty" >&2
fi

metadata_file="$(mktemp)"
trap 'rm -f "$metadata_file"' EXIT
{
  printf 'git_hash=%s\n' "$git_hash"
  printf 'git_date=%s\n' "$git_date"
  printf 'git_tag=%s\n' "$git_tag"
  printf 'git_dirty=%s\n' "$git_dirty"
  printf 'changelog_raw<<JCODE_CHANGELOG_EOF\n%s\nJCODE_CHANGELOG_EOF\n' "$changelog_raw"
} > "$metadata_file"

echo "Building portable Linux release in Docker image: $image"
echo "Architecture: $arch ($docker_platform, Rust target $target)"
echo "Output dir: $out_dir"
echo "Embedding git metadata: hash=${git_hash:-<none>} tag=${git_tag:-<none>} dirty=$git_dirty changelog_lines=$(printf '%s' "$changelog_raw" | grep -c '' || true)"

docker run --rm --platform "$docker_platform" \
  -e CARGO_TERM_COLOR=always \
  -e JCODE_RELEASE_BUILD="${JCODE_RELEASE_BUILD:-1}" \
  -e JCODE_BUILD_SEMVER="${JCODE_BUILD_SEMVER:-}" \
  -e JCODE_BUILD_METADATA_FILE=/jcode-build-meta \
  -e JCODE_BUILD_GIT_HASH="$git_hash" \
  -e JCODE_BUILD_GIT_DATE="$git_date" \
  -e JCODE_BUILD_GIT_TAG="$git_tag" \
  -e JCODE_BUILD_GIT_DIRTY="$git_dirty" \
  -e JCODE_COMPAT_PROFILE="$profile" \
  -e JCODE_COMPAT_TARGET="$target" \
  -e HOST_UID="$host_uid" \
  -e HOST_GID="$host_gid" \
  -v "$repo_root:/work" \
  -v "$metadata_file:/jcode-build-meta:ro" \
  -v "$out_dir:/out" \
  -v "$cache_root/cargo-registry:/root/.cargo/registry" \
  -v "$cache_root/cargo-git:/root/.cargo/git" \
  -v "$cache_root/rustup-$arch:/root/.rustup" \
  -w /work \
  "$image" \
  bash -lc '
    set -euo pipefail
    if command -v apt-get >/dev/null 2>&1; then
      export DEBIAN_FRONTEND=noninteractive
      apt-get update -qq
      apt-get install -y -qq \
        build-essential \
        ca-certificates \
        curl \
        git \
        libssl-dev \
        perl \
        pkg-config
    elif command -v yum >/dev/null 2>&1; then
      yum install -y \
        ca-certificates \
        curl \
        gcc \
        gcc-c++ \
        git \
        make \
        openssl-devel \
        perl-core \
        pkgconfig \
        tar \
        gzip
      update-ca-trust || true
    else
      echo "Unsupported build image: expected apt-get or yum" >&2
      exit 1
    fi

    # The OpenSSL 3 Configure script imports core modules that minimal
    # manylinux2014 images split into separate RPMs. Installing perl-core keeps
    # the build from failing one missing module at a time as OpenSSL evolves.
    perl -MIPC::Cmd -MTime::Piece -e 1

    if [[ ! -x /root/.cargo/bin/cargo ]]; then
      curl https://sh.rustup.rs -sSf | sh -s -- -y --profile minimal --default-toolchain stable
    fi
	    source /root/.cargo/env

	    # Belt-and-suspenders: the host-computed metadata file
	    # (JCODE_BUILD_METADATA_FILE=/jcode-build-meta) is the primary source of
	    # git hash/date/changelog, but mark the bind-mounted repo as a safe
	    # directory so any in-container git fallback still works despite the
	    # host-UID/root-git ownership mismatch (CVE-2022-24765 guard).
	    git config --global --add safe.directory /work 2>/dev/null || true

	    export CARGO_TARGET_DIR="/work/target/linux-compat-$JCODE_COMPAT_TARGET"
	    export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-1}"
	    export CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS="${CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS:--C link-arg=-static-libgcc}"
	    export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUSTFLAGS="${CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUSTFLAGS:--C link-arg=-static-libgcc}"
	    cargo build --profile "$JCODE_COMPAT_PROFILE" --target "$JCODE_COMPAT_TARGET" \
	      -p jcode --bin jcode --features linux-compat-vendored-openssl

	    cp "$CARGO_TARGET_DIR/$JCODE_COMPAT_TARGET/$JCODE_COMPAT_PROFILE/jcode" "/out/'"$artifact"'.bin"
	    chmod +x "/out/'"$artifact"'.bin"
	    cat > "/out/'"$artifact"'" <<WRAPPER
#!/usr/bin/env sh
set -eu
self_path=\$0
if command -v readlink >/dev/null 2>&1; then
  resolved=\$(readlink -f -- "\$0" 2>/dev/null || true)
  if [ -n "\$resolved" ]; then
    self_path=\$resolved
  fi
fi
case "\$self_path" in
  */*) self_dir=\$(CDPATH= cd -- "\$(dirname -- "\$self_path")" && pwd) ;;
  *) self_dir=\$(pwd) ;;
esac
if [ -n "\${LD_LIBRARY_PATH:-}" ]; then
  export LD_LIBRARY_PATH="\$self_dir:\$LD_LIBRARY_PATH"
else
  export LD_LIBRARY_PATH="\$self_dir"
fi
exec "\$self_dir/'"$artifact"'.bin" "\$@"
WRAPPER
	    chmod +x "/out/'"$artifact"'"

	    # Preserve the OpenSSL runtime libraries used by the build image. Some
	    # Terminal-Bench containers are older than the build host and either lack
	    # libssl entirely or expose a different SONAME. The Harbor adapter uploads
	    # these sibling libraries and sets LD_LIBRARY_PATH for the jcode process.
	    ldd "/out/'"$artifact"'.bin" \
	      | awk "/lib(ssl|crypto)[.]so/ { print \$3 }" \
	      | while read -r lib; do
	          if [[ -n "$lib" && -f "$lib" ]]; then
	            cp -L "$lib" /out/
	          fi
	        done

	    # Linux artifacts must be validated inside Docker when the host is macOS.
	    if [[ -n "$JCODE_BUILD_GIT_HASH" ]]; then
	      if ! embedded=$("/out/'"$artifact"'" --no-update --no-selfdev version 2>/dev/null \
	        | awk -F"\t" "\$1 == \"version\" { print \$2; exit }"); then
	        echo "error: built Linux artifact could not report its version" >&2
	        exit 1
	      fi
	      if [[ -z "$embedded" ]]; then
	        echo "error: built Linux artifact reported no version metadata" >&2
	        exit 1
	      fi
	      if [[ "$embedded" != *"$JCODE_BUILD_GIT_HASH"* ]]; then
	        echo "error: embedded build metadata reports $embedded but the tree is at $JCODE_BUILD_GIT_HASH" >&2
	        echo "       (stale build cache; re-run after cargo clean or bump JCODE_BUILD_GIT_HASH)" >&2
	        exit 1
	      fi
	    fi

		    extra_libs=()
		    for pattern in libssl.so\* libcrypto.so\*; do
		      for lib in $pattern; do
		        [[ -e "$lib" ]] && extra_libs+=("$lib")
		      done
		    done

		    if (( ${#extra_libs[@]} > 0 )); then
		      (cd /out && tar czf '"$artifact"'.tar.gz '"$artifact"' '"$artifact"'.bin "${extra_libs[@]}")
		    else
		      (cd /out && tar czf '"$artifact"'.tar.gz '"$artifact"' '"$artifact"'.bin)
		    fi

		    chown_inputs=("/out/'"$artifact"'" "/out/'"$artifact"'.bin" "/out/'"$artifact"'.tar.gz")
		    if (( ${#extra_libs[@]} > 0 )); then
		      for lib in "${extra_libs[@]}"; do
		        chown_inputs+=("/out/$lib")
		      done
		    fi
		    chown "$HOST_UID:$HOST_GID" "${chown_inputs[@]}" 2>/dev/null || true
		  '

# The docker invocation above is the last command in the script, so without an
# explicit check a failed build still exits 0: the in-container `bash -lc` can
# fail (daemon down, compile error) while this script reports success and leaves
# a stale or empty out_dir behind. Benchmark launchers pin artifact SHA-256s and
# would happily bank a previous build's binary, so verify the outputs exist
# before claiming success.
for required in "$out_dir/$artifact" "$out_dir/$artifact.bin" "$out_dir/$artifact.tar.gz"; do
  if [[ ! -s "$required" ]]; then
    echo "error: build did not produce $required" >&2
    exit 1
  fi
done

echo "Built artifacts:"
ls -lh "$out_dir/$artifact" "$out_dir/$artifact.tar.gz"
