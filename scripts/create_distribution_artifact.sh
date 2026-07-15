#!/usr/bin/env bash
set -euo pipefail

SOURCE="${BASH_SOURCE[0]}"
while [[ -L "$SOURCE" ]]; do
  DIR="$(cd -P "$(dirname "$SOURCE")" && pwd)"
  SOURCE="$(readlink "$SOURCE")"
  [[ "$SOURCE" != /* ]] && SOURCE="$DIR/$SOURCE"
done
SCRIPT_DIR="$(cd -P "$(dirname "$SOURCE")" && pwd)"
ROOT_DIR="$(cd -P "$SCRIPT_DIR/.." && pwd)"

OUTPUT_DIR="$ROOT_DIR/dist"
ARTIFACT_NAME=""
PYTHON_VERSION="3.12"
SOURCE_PYTHON=""
REQUIREMENTS_FILE="$ROOT_DIR/scripts/parakeet-requirements.txt"
SKIP_RUST_BUILD=0
SKIP_RUNTIME_BUILD=0
ALLOW_NONRELOCATABLE=0
KEEP_STAGING=0

usage() {
  cat <<EOF
Create a distributable riff artifact tarball.

Usage:
  $(basename "$0") [options]

Options:
  --output-dir <path>         Output directory (default: $ROOT_DIR/dist)
  --name <artifact-name>      Artifact base name (default: riff-<os>-<arch>-<short-sha>-<utcstamp>)
  --skip-rust-build           Skip 'cargo build --release'
  --skip-runtime-build        Reuse existing runtime/python if present
  --python-version <ver>      Python version for runtime builder (default: 3.12)
  --source-python <path>      Source python executable for runtime builder
  --requirements <path>       Requirements file to install in bundled runtime
  --allow-nonrelocatable      Pass through to runtime builder (not recommended)
  --keep-staging              Keep temporary staging directory for inspection
  -h, --help                  Show this help
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --output-dir)
      OUTPUT_DIR="${2:-}"
      shift 2
      ;;
    --name)
      ARTIFACT_NAME="${2:-}"
      shift 2
      ;;
    --skip-rust-build)
      SKIP_RUST_BUILD=1
      shift
      ;;
    --skip-runtime-build)
      SKIP_RUNTIME_BUILD=1
      shift
      ;;
    --python-version)
      PYTHON_VERSION="${2:-}"
      shift 2
      ;;
    --source-python)
      SOURCE_PYTHON="${2:-}"
      shift 2
      ;;
    --requirements)
      REQUIREMENTS_FILE="${2:-}"
      shift 2
      ;;
    --allow-nonrelocatable)
      ALLOW_NONRELOCATABLE=1
      shift
      ;;
    --keep-staging)
      KEEP_STAGING=1
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "Unknown argument: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

OS="$(uname -s | tr '[:upper:]' '[:lower:]')"
ARCH="$(uname -m | tr '[:upper:]' '[:lower:]')"
SHORT_SHA="$(git -C "$ROOT_DIR" rev-parse --short HEAD 2>/dev/null || echo "nogit")"
STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
if [[ -z "$ARTIFACT_NAME" ]]; then
  ARTIFACT_NAME="riff-${OS}-${ARCH}-${SHORT_SHA}-${STAMP}"
fi

if [[ "$SKIP_RUST_BUILD" -eq 0 ]]; then
  echo "[riff] Building Rust release binary..."
  (cd "$ROOT_DIR" && cargo build --release)
fi

if [[ "$SKIP_RUNTIME_BUILD" -eq 0 ]]; then
  RUNTIME_CMD=(
    "$ROOT_DIR/scripts/build_bundled_python_runtime.sh"
    --python-version "$PYTHON_VERSION"
    --requirements "$REQUIREMENTS_FILE"
  )
  if [[ -n "$SOURCE_PYTHON" ]]; then
    RUNTIME_CMD+=(--source-python "$SOURCE_PYTHON")
  fi
  if [[ "$ALLOW_NONRELOCATABLE" -eq 1 ]]; then
    RUNTIME_CMD+=(--allow-nonrelocatable)
  fi
  echo "[riff] Ensuring bundled runtime..."
  "${RUNTIME_CMD[@]}"
fi

REQUIRED_FILES=(
  "$ROOT_DIR/riff"
  "$ROOT_DIR/README.md"
  "$ROOT_DIR/target/release/riff"
  "$ROOT_DIR/scripts/parakeet_transcribe.py"
  "$ROOT_DIR/scripts/riff_web_server.py"
  "$ROOT_DIR/runtime/python/bin/python"
)

for f in "${REQUIRED_FILES[@]}"; do
  if [[ ! -e "$f" ]]; then
    echo "Missing required file for artifact: $f" >&2
    echo "If runtime is intentionally prebuilt, ensure runtime/python exists." >&2
    exit 1
  fi
done

if [[ ! -x "$ROOT_DIR/riff" || ! -x "$ROOT_DIR/target/release/riff" ]]; then
  echo "riff wrapper or release binary is not executable." >&2
  exit 1
fi

mkdir -p "$OUTPUT_DIR"
STAGING_PARENT="$(mktemp -d "${TMPDIR:-/tmp}/riff-dist.XXXXXX")"
STAGING_DIR="$STAGING_PARENT/$ARTIFACT_NAME"
mkdir -p "$STAGING_DIR/target/release" "$STAGING_DIR/runtime"

cp -a "$ROOT_DIR/riff" "$STAGING_DIR/"
cp -a "$ROOT_DIR/README.md" "$STAGING_DIR/"
cp -a "$ROOT_DIR/target/release/riff" "$STAGING_DIR/target/release/"
cp -a "$ROOT_DIR/scripts" "$STAGING_DIR/"
cp -a "$ROOT_DIR/runtime/python" "$STAGING_DIR/runtime/"

cat > "$STAGING_DIR/BUILD-INFO.txt" <<EOF
artifact_name=$ARTIFACT_NAME
built_at_utc=$STAMP
git_sha=$SHORT_SHA
platform=${OS}-${ARCH}
python_runtime=$("$ROOT_DIR/runtime/python/bin/python" -V 2>&1)
EOF

ARTIFACT_PATH="$OUTPUT_DIR/$ARTIFACT_NAME.tar.gz"
echo "[riff] Writing artifact: $ARTIFACT_PATH"
tar -C "$STAGING_PARENT" -czf "$ARTIFACT_PATH" "$ARTIFACT_NAME"

if command -v shasum >/dev/null 2>&1; then
  shasum -a 256 "$ARTIFACT_PATH" > "$ARTIFACT_PATH.sha256"
  echo "[riff] Wrote checksum: $ARTIFACT_PATH.sha256"
fi

if [[ "$KEEP_STAGING" -eq 1 ]]; then
  echo "[riff] Kept staging dir: $STAGING_PARENT"
else
  rm -rf "$STAGING_PARENT"
fi

echo "[riff] Artifact ready: $ARTIFACT_PATH"
