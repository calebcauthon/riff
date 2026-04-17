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

RUNTIME_DIR="$ROOT_DIR/runtime/python"
SOURCE_PYTHON=""
PYTHON_VERSION="3.12"
PACKAGES="nemo_toolkit[asr] torch soundfile"
SKIP_INSTALL=0
ALLOW_NONRELOCATABLE=0

usage() {
  cat <<EOF
Build a full bundled Python runtime (not a venv) for ispy.

Usage:
  $(basename "$0") [options]

Options:
  --runtime-dir <path>      Target runtime dir (default: $ROOT_DIR/runtime/python)
  --source-python <path>    Source python executable to copy from
  --python-version <ver>    Source version to resolve when --source-python is omitted (default: 3.12)
  --packages "<specs>"      Pip specs to install (default: "$PACKAGES")
  --skip-install            Copy runtime only; skip pip installs
  --allow-nonrelocatable    Allow source runtimes that keep absolute sys.prefix paths
  -h, --help                Show this help

Notes:
  - This copies the full source Python prefix into runtime/python.
  - The resulting runtime is architecture- and OS-specific.
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --runtime-dir)
      RUNTIME_DIR="${2:-}"
      shift 2
      ;;
    --source-python)
      SOURCE_PYTHON="${2:-}"
      shift 2
      ;;
    --python-version)
      PYTHON_VERSION="${2:-}"
      shift 2
      ;;
    --packages)
      PACKAGES="${2:-}"
      shift 2
      ;;
    --skip-install)
      SKIP_INSTALL=1
      shift
      ;;
    --allow-nonrelocatable)
      ALLOW_NONRELOCATABLE=1
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

if [[ -z "$SOURCE_PYTHON" ]]; then
  if [[ -x "$HOME/.local/bin/python${PYTHON_VERSION}" ]]; then
    SOURCE_PYTHON="$HOME/.local/bin/python${PYTHON_VERSION}"
  elif command -v "python${PYTHON_VERSION}" >/dev/null 2>&1; then
    SOURCE_PYTHON="$(command -v "python${PYTHON_VERSION}")"
  elif command -v python3 >/dev/null 2>&1; then
    SOURCE_PYTHON="$(command -v python3)"
  else
    echo "No Python executable found. Install Python ${PYTHON_VERSION} first." >&2
    exit 1
  fi
fi

if [[ ! -x "$SOURCE_PYTHON" ]]; then
  echo "Source python not executable: $SOURCE_PYTHON" >&2
  exit 1
fi

PY_INFO_RAW="$("$SOURCE_PYTHON" -c 'import sys; print(sys.version_info[0]); print(sys.version_info[1]); print(sys.version.split()[0]); print(sys.base_prefix); print(sys.executable)')"
PY_MAJOR="$(printf '%s\n' "$PY_INFO_RAW" | sed -n '1p')"
PY_MINOR="$(printf '%s\n' "$PY_INFO_RAW" | sed -n '2p')"
PY_VERSION_STR="$(printf '%s\n' "$PY_INFO_RAW" | sed -n '3p')"
SOURCE_PREFIX="$(printf '%s\n' "$PY_INFO_RAW" | sed -n '4p')"
SOURCE_EXECUTABLE="$(printf '%s\n' "$PY_INFO_RAW" | sed -n '5p')"

if [[ -z "$PY_MAJOR" || -z "$PY_MINOR" || -z "$PY_VERSION_STR" || -z "$SOURCE_PREFIX" || -z "$SOURCE_EXECUTABLE" ]]; then
  echo "Failed to inspect source python: $SOURCE_PYTHON" >&2
  exit 1
fi

if [[ "$PY_MAJOR" -ne 3 || "$PY_MINOR" -lt 10 || "$PY_MINOR" -gt 12 ]]; then
  echo "Parakeet/NeMo requires Python 3.10-3.12. Found: $PY_VERSION_STR ($SOURCE_PYTHON)" >&2
  exit 1
fi

if [[ ! -d "$SOURCE_PREFIX" ]]; then
  echo "Source prefix does not exist: $SOURCE_PREFIX" >&2
  exit 1
fi

if [[ ! -x "$SOURCE_PREFIX/bin/python${PY_MAJOR}.${PY_MINOR}" && ! -x "$SOURCE_PREFIX/bin/python3" ]]; then
  echo "Source prefix does not look like a full Python distribution: $SOURCE_PREFIX" >&2
  exit 1
fi

mkdir -p "$(dirname "$RUNTIME_DIR")"
RUNTIME_PARENT="$(cd -P "$(dirname "$RUNTIME_DIR")" && pwd)"
RUNTIME_NAME="$(basename "$RUNTIME_DIR")"
TMP_DIR="$RUNTIME_PARENT/.${RUNTIME_NAME}.tmp"

mkdir -p "$RUNTIME_PARENT"
rm -rf "$TMP_DIR"

echo "[ispy] Source python: $SOURCE_PYTHON"
echo "[ispy] Source executable: $SOURCE_EXECUTABLE"
echo "[ispy] Source prefix: $SOURCE_PREFIX"
echo "[ispy] Building runtime at: $RUNTIME_DIR"

rsync -a --delete "$SOURCE_PREFIX/" "$TMP_DIR/"

if [[ ! -x "$TMP_DIR/bin/python" ]]; then
  if [[ -x "$TMP_DIR/bin/python${PY_MAJOR}.${PY_MINOR}" ]]; then
    ln -sf "python${PY_MAJOR}.${PY_MINOR}" "$TMP_DIR/bin/python"
  elif [[ -x "$TMP_DIR/bin/python3" ]]; then
    ln -sf "python3" "$TMP_DIR/bin/python"
  fi
fi

if [[ ! -x "$TMP_DIR/bin/python" ]]; then
  echo "Failed to create runtime python executable at $TMP_DIR/bin/python" >&2
  exit 1
fi

if [[ "$SKIP_INSTALL" -eq 0 ]]; then
  PY="$TMP_DIR/bin/python"
  echo "[ispy] Installing runtime packages: $PACKAGES"

  if ! "$PY" -m pip --version >/dev/null 2>&1; then
    "$PY" -m ensurepip --default-pip || true
  fi

  if ! "$PY" -m pip install --break-system-packages --upgrade pip; then
    "$PY" -m pip install --upgrade pip
  fi

  if ! "$PY" -m pip install --break-system-packages $PACKAGES; then
    "$PY" -m pip install $PACKAGES
  fi
fi

rm -rf "$RUNTIME_DIR"
mv "$TMP_DIR" "$RUNTIME_DIR"

BUILT_INFO_RAW="$("$RUNTIME_DIR/bin/python" -c 'import sys; print(sys.version.split()[0]); print(sys.prefix)')"
BUILT_VERSION="$(printf '%s\n' "$BUILT_INFO_RAW" | sed -n '1p')"
BUILT_PREFIX="$(printf '%s\n' "$BUILT_INFO_RAW" | sed -n '2p')"

if [[ -z "$BUILT_PREFIX" ]]; then
  echo "Failed to validate built runtime prefix." >&2
  exit 1
fi

if [[ "$BUILT_PREFIX" != "$RUNTIME_DIR"* ]]; then
  if [[ "$ALLOW_NONRELOCATABLE" -eq 0 ]]; then
    echo "Runtime is non-relocatable (sys.prefix=$BUILT_PREFIX)." >&2
    echo "Use a uv-managed Python source to build a no-system dependency bundle." >&2
    echo "Example:" >&2
    echo "  uv python install $PYTHON_VERSION" >&2
    echo "  $0 --source-python \$HOME/.local/bin/python$PYTHON_VERSION" >&2
    exit 1
  fi
  echo "[ispy] WARNING: runtime prefix is not relocatable: $BUILT_PREFIX"
fi

echo "[ispy] Bundled runtime ready: $BUILT_VERSION"
echo "[ispy] Runtime path: $RUNTIME_DIR/bin/python"
echo "[ispy] ispy will auto-use this runtime when ISPY_PYTHON_BIN is unset."
