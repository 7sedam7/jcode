#!/usr/bin/env bash
set -euo pipefail

repo_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
mkdir -p "$tmp/bin" "$tmp/out-arm64" "$tmp/out-x86_64"

cat > "$tmp/bin/uname" <<'EOF'
#!/usr/bin/env bash
if [ "${1:-}" = "-m" ]; then
  printf '%s\n' "${TEST_UNAME_M:-arm64}"
else
  /usr/bin/uname "$@"
fi
EOF
chmod +x "$tmp/bin/uname"

cat > "$tmp/bin/docker" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail

printf '%s\n' "$*" >> "$DOCKER_ARGS_LOG"

inner_script=""
for arg in "$@"; do
  inner_script="$arg"
done
bash -n -c "$inner_script"

out_dir=""
image_seen=0
platform_seen=0
expected_platform="${TEST_EXPECTED_PLATFORM:?}"
expected_image="${TEST_EXPECTED_IMAGE:?}"
expected_target="${TEST_EXPECTED_TARGET:?}"
expected_artifact="${TEST_EXPECTED_ARTIFACT:?}"
expected_cache_arch="${TEST_EXPECTED_CACHE_ARCH:?}"
case "$inner_script" in
  *'/work/target/linux-compat-$JCODE_COMPAT_TARGET'*) ;;
  *)
    echo "expected architecture-isolated Cargo target directory" >&2
    exit 1
    ;;
esac
rustup_cache_seen=0
while [ "$#" -gt 0 ]; do
  case "$1" in
    --platform)
      [ "$image_seen" -eq 0 ] || {
        echo "Docker platform must be specified before the image" >&2
        exit 1
      }
      [ "${2:-}" = "$expected_platform" ] || {
        echo "expected $expected_platform Docker platform" >&2
        exit 1
      }
      platform_seen=1
      shift 2
      ;;
    -e)
      case "$2" in
        JCODE_COMPAT_TARGET=*)
          [ "${2#*=}" = "$expected_target" ] || {
            echo "unexpected Rust target: ${2#*=}" >&2
            exit 1
          }
          ;;
      esac
      shift 2
      ;;
    -v)
      case "$2" in
        *:/out) out_dir=${2%:/out} ;;
        *"/rustup-$expected_cache_arch:/root/.rustup") rustup_cache_seen=1 ;;
      esac
      shift 2
      ;;
    "$expected_image")
      image_seen=1
      shift
      ;;
    *)
      shift
      ;;
  esac
done

[ "$platform_seen" -eq 1 ] || {
  echo "missing explicit Docker platform" >&2
  exit 1
}
[ "$image_seen" -eq 1 ] || {
  echo "missing manylinux build image" >&2
  exit 1
}
[ "$rustup_cache_seen" -eq 1 ] || {
  echo "missing architecture-isolated rustup cache" >&2
  exit 1
}
[ -n "$out_dir" ] || {
  echo "missing /out bind mount" >&2
  exit 1
}

printf '#!/usr/bin/env sh\nexit 97\n' > "$out_dir/$expected_artifact"
chmod +x "$out_dir/$expected_artifact"

# A Linux ELF cannot be executed by the macOS host. Leaving this file
# non-executable makes the regression reproducible on any development host.
printf '\177ELFfake-linux-binary\n' > "$out_dir/$expected_artifact.bin"
printf 'fake archive\n' > "$out_dir/$expected_artifact.tar.gz"
EOF
chmod +x "$tmp/bin/docker"

(
  cd "$repo_dir"
  unset JCODE_COMPAT_ARCH JCODE_COMPAT_IMAGE JCODE_COMPAT_ARTIFACT
  PATH="$tmp/bin:$PATH" \
  DOCKER_ARGS_LOG="$tmp/docker-arm64.log" \
  TEST_UNAME_M=arm64 \
  TEST_EXPECTED_PLATFORM=linux/arm64 \
  TEST_EXPECTED_IMAGE=quay.io/pypa/manylinux2014_aarch64 \
  TEST_EXPECTED_TARGET=aarch64-unknown-linux-gnu \
  TEST_EXPECTED_ARTIFACT=jcode-linux-aarch64 \
  TEST_EXPECTED_CACHE_ARCH=aarch64 \
  bash scripts/build_linux_compat.sh "$tmp/out-arm64" >/dev/null
)

grep -q -- '--platform linux/arm64' "$tmp/docker-arm64.log"
grep -q -- 'quay.io/pypa/manylinux2014_aarch64' "$tmp/docker-arm64.log"
grep -q -- 'JCODE_COMPAT_TARGET=aarch64-unknown-linux-gnu' "$tmp/docker-arm64.log"
grep -q -- '--no-update --no-selfdev version' "$tmp/docker-arm64.log"

(
  cd "$repo_dir"
  unset JCODE_COMPAT_ARCH JCODE_COMPAT_IMAGE JCODE_COMPAT_ARTIFACT
  PATH="$tmp/bin:$PATH" \
  DOCKER_ARGS_LOG="$tmp/docker-x86_64.log" \
  TEST_UNAME_M=arm64 \
  TEST_EXPECTED_PLATFORM=linux/amd64 \
  TEST_EXPECTED_IMAGE=quay.io/pypa/manylinux2014_x86_64 \
  TEST_EXPECTED_TARGET=x86_64-unknown-linux-gnu \
  TEST_EXPECTED_ARTIFACT=jcode-linux-x86_64 \
  TEST_EXPECTED_CACHE_ARCH=x86_64 \
  JCODE_COMPAT_ARCH=x86_64 \
  bash scripts/build_linux_compat.sh "$tmp/out-x86_64" >/dev/null
)

grep -q -- '--platform linux/amd64' "$tmp/docker-x86_64.log"
grep -q -- 'quay.io/pypa/manylinux2014_x86_64' "$tmp/docker-x86_64.log"
grep -q -- 'JCODE_COMPAT_TARGET=x86_64-unknown-linux-gnu' "$tmp/docker-x86_64.log"

if (
  cd "$repo_dir"
  PATH="$tmp/bin:$PATH" \
  JCODE_COMPAT_ARCH=powerpc64 \
  bash scripts/build_linux_compat.sh "$tmp/out-unsupported" >/dev/null 2>&1
); then
  echo "expected unsupported architecture to fail" >&2
  exit 1
fi
