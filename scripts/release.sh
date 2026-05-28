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

VERSION_INPUT=""
ALLOW_DIRTY=0
DRY_RUN=0
RETAG=0
PUSH_TAG=0
SKIP_TESTS=0

usage() {
  cat <<EOF
Prepare a Homebrew-friendly release for riff.

Usage:
  $(basename "$0") [options] <version>

Examples:
  $(basename "$0") 0.2.0
  $(basename "$0") v0.2.0
  $(basename "$0") --dry-run --allow-dirty v0.2.0

Options:
  --dry-run       Print planned actions without mutating files/tags
  --allow-dirty   Allow running with a dirty git tree
  --retag         Force-update existing local tag to current HEAD
  --push-tag      Push release tag to origin if missing remotely
  --skip-tests    Skip cargo test sanity check
  -h, --help      Show this help text
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --dry-run)
      DRY_RUN=1
      shift
      ;;
    --allow-dirty)
      ALLOW_DIRTY=1
      shift
      ;;
    --retag)
      RETAG=1
      shift
      ;;
    --push-tag)
      PUSH_TAG=1
      shift
      ;;
    --skip-tests)
      SKIP_TESTS=1
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    -*)
      echo "Unknown option: $1" >&2
      usage >&2
      exit 2
      ;;
    *)
      if [[ -n "$VERSION_INPUT" ]]; then
        echo "Unexpected extra argument: $1" >&2
        usage >&2
        exit 2
      fi
      VERSION_INPUT="$1"
      shift
      ;;
  esac
done

if [[ -z "$VERSION_INPUT" ]]; then
  echo "Version argument is required." >&2
  usage >&2
  exit 2
fi

RAW_VERSION="$VERSION_INPUT"
if [[ "$RAW_VERSION" == v* ]]; then
  VERSION="${RAW_VERSION#v}"
else
  VERSION="$RAW_VERSION"
fi
TAG="v$VERSION"

if [[ ! "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+([.-][0-9A-Za-z]+)*$ ]]; then
  echo "Invalid version '$VERSION'. Expected semver-like value such as 0.2.0 or 1.2.3-rc1." >&2
  exit 2
fi

require_tool() {
  local tool="$1"
  if ! command -v "$tool" >/dev/null 2>&1; then
    echo "Missing required tool: $tool" >&2
    exit 1
  fi
}

for tool in git cargo curl shasum python3; do
  require_tool "$tool"
done

if [[ ! -d "$ROOT_DIR/.git" ]]; then
  echo "Not a git repository root: $ROOT_DIR" >&2
  exit 1
fi

CARGO_TOML="$ROOT_DIR/Cargo.toml"
VERSION_FILE="$ROOT_DIR/VERSION"
TAP_DIR="${RIFF_TAP_DIR:-$HOME/Code/riff-tap}"
FORMULA_FILE="$TAP_DIR/Formula/riff.rb"

for path in "$CARGO_TOML" "$VERSION_FILE" "$FORMULA_FILE"; do
  if [[ ! -f "$path" ]]; then
    echo "Required file not found: $path" >&2
    exit 1
  fi
done

if [[ "$ALLOW_DIRTY" -eq 0 ]]; then
  if [[ -n "$(git -C "$ROOT_DIR" status --porcelain)" ]]; then
    echo "Git working tree is not clean. Commit/stash changes or rerun with --allow-dirty." >&2
    exit 1
  fi
fi

run_or_echo() {
  if [[ "$DRY_RUN" -eq 1 ]]; then
    printf '[dry-run] %s\n' "$*"
  else
    "$@"
  fi
}

update_version_files() {
  local cargo_toml="$1"
  local version_file="$2"
  local version="$3"
  python3 - "$cargo_toml" "$version_file" "$version" <<'PY'
import pathlib
import re
import sys

cargo_path = pathlib.Path(sys.argv[1])
version_path = pathlib.Path(sys.argv[2])
version = sys.argv[3]

cargo_text = cargo_path.read_text()
updated_cargo, count = re.subn(
    r'(?m)^version\s*=\s*".*"$',
    f'version = "{version}"',
    cargo_text,
    count=1,
)
if count != 1:
    raise SystemExit(f"Failed to update version in {cargo_path}")

cargo_path.write_text(updated_cargo)
version_path.write_text(version + "\n")
PY
}

update_formula_file() {
  local formula_file="$1"
  local tarball_url="$2"
  local sha="$3"
  python3 - "$formula_file" "$tarball_url" "$sha" <<'PY'
import pathlib
import re
import sys

formula_path = pathlib.Path(sys.argv[1])
tarball_url = sys.argv[2]
sha = sys.argv[3]

text = formula_path.read_text()
updated = re.sub(r'(?m)^  url ".*"$', f'  url "{tarball_url}"', text, count=1)
updated = re.sub(r'(?m)^  sha256 .*$',
                 f'  sha256 "{sha}"',
                 updated,
                 count=1)

if updated == text:
    raise SystemExit(f"No formula changes applied in {formula_path}")

formula_path.write_text(updated)
PY
}

origin_url="$(git -C "$ROOT_DIR" remote get-url origin 2>/dev/null || true)"
if [[ -z "$origin_url" ]]; then
  echo "Could not resolve git remote 'origin'." >&2
  exit 1
fi

repo_slug="$(python3 - "$origin_url" <<'PY'
import re
import sys

origin = sys.argv[1].strip()
patterns = [
    r'^git@github\.com:([^/]+/[^/]+?)(?:\.git)?$',
    r'^https://github\.com/([^/]+/[^/]+?)(?:\.git)?$',
    r'^ssh://git@github\.com/([^/]+/[^/]+?)(?:\.git)?$',
]
for p in patterns:
    m = re.match(p, origin)
    if m:
        print(m.group(1))
        raise SystemExit(0)
raise SystemExit(1)
PY
)" || {
  echo "Origin remote is not a GitHub repo URL: $origin_url" >&2
  exit 1
}

tarball_url="https://github.com/$repo_slug/archive/refs/tags/$TAG.tar.gz"
tmp_tarball="$(mktemp "${TMPDIR:-/tmp}/riff-release.${TAG}.XXXXXX.tar.gz")"
cleanup() {
  rm -f "$tmp_tarball"
}
trap cleanup EXIT

echo "[riff-release] Preparing release $VERSION ($TAG)"
echo "[riff-release] Repo root: $ROOT_DIR"
echo "[riff-release] GitHub repo: $repo_slug"

echo "[riff-release] Updating Cargo.toml and VERSION"
if [[ "$DRY_RUN" -eq 1 ]]; then
  echo "[dry-run] update version metadata to $VERSION"
else
  update_version_files "$CARGO_TOML" "$VERSION_FILE" "$VERSION"
fi

echo "[riff-release] Running cargo build --release"
run_or_echo cargo build --release

if [[ "$SKIP_TESTS" -eq 0 ]]; then
  echo "[riff-release] Running cargo test"
  run_or_echo cargo test
else
  echo "[riff-release] Skipping cargo test (--skip-tests)"
fi

local_tag_sha="$(git -C "$ROOT_DIR" rev-parse -q --verify "refs/tags/$TAG" 2>/dev/null || true)"
head_sha="$(git -C "$ROOT_DIR" rev-parse HEAD)"
if [[ -z "$local_tag_sha" ]]; then
  echo "[riff-release] Creating local tag $TAG"
  run_or_echo git -C "$ROOT_DIR" tag "$TAG"
else
  peeled_tag_sha="$(git -C "$ROOT_DIR" rev-parse "$TAG^{}" 2>/dev/null || echo "$local_tag_sha")"
  if [[ "$peeled_tag_sha" == "$head_sha" ]]; then
    echo "[riff-release] Local tag $TAG already points at HEAD"
  elif [[ "$RETAG" -eq 1 ]]; then
    echo "[riff-release] Retagging $TAG to current HEAD"
    run_or_echo git -C "$ROOT_DIR" tag -f "$TAG"
  else
    echo "Local tag $TAG already exists and does not point at HEAD." >&2
    echo "Use --retag to force-update it, or release a different version." >&2
    exit 1
  fi
fi

remote_tag_sha="$(git -C "$ROOT_DIR" ls-remote --tags origin "refs/tags/$TAG" | awk '{print $1}' || true)"
if [[ -z "$remote_tag_sha" ]]; then
  if [[ "$DRY_RUN" -eq 1 ]]; then
    echo "[dry-run] remote tag $TAG missing; would require push before checksum fetch"
  elif [[ "$PUSH_TAG" -eq 1 ]]; then
    echo "[riff-release] Pushing tag $TAG to origin"
    run_or_echo git -C "$ROOT_DIR" push origin "$TAG"
  else
    echo "Remote tag $TAG is missing on origin, cannot compute GitHub tarball checksum yet." >&2
    echo "Run: git push origin $TAG" >&2
    echo "Then rerun this script (or rerun with --push-tag)." >&2
    exit 1
  fi
fi

if [[ "$DRY_RUN" -eq 1 ]]; then
  echo "[dry-run] fetch $tarball_url and compute sha256"
  sha256_value="<dry-run>"
else
  echo "[riff-release] Waiting 20s for GitHub to generate tarball..."
  sleep 20
  echo "[riff-release] Downloading GitHub tag tarball"
  attempts=0
  max_attempts=8
  until [[ "$attempts" -ge "$max_attempts" ]]; do
    attempts=$((attempts + 1))
    if curl --fail --location --silent --show-error "$tarball_url" --output "$tmp_tarball"; then
      break
    fi
    if [[ "$attempts" -lt "$max_attempts" ]]; then
      sleep_seconds=$((attempts * 15))
      echo "[riff-release] Tarball fetch failed (attempt $attempts/$max_attempts), retrying in ${sleep_seconds}s..."
      sleep "$sleep_seconds"
    fi
  done
  if [[ "$attempts" -ge "$max_attempts" ]]; then
    echo "Failed to fetch tarball after $max_attempts attempts: $tarball_url" >&2
    echo "You can retry later, or run manually:" >&2
    echo "  curl -L -o /tmp/riff-${TAG}.tar.gz $tarball_url" >&2
    echo "  shasum -a 256 /tmp/riff-${TAG}.tar.gz" >&2
    exit 1
  fi
  sha256_value="$(shasum -a 256 "$tmp_tarball" | awk '{print $1}')"
fi

echo "[riff-release] Updating tap formula at $FORMULA_FILE"
if [[ "$DRY_RUN" -eq 1 ]]; then
  echo "[dry-run] set formula url=$tarball_url sha256=$sha256_value"
else
  update_formula_file "$FORMULA_FILE" "$tarball_url" "$sha256_value"
fi

echo "[riff-release] Committing and pushing tap repo"
run_or_echo git -C "$TAP_DIR" add Formula/riff.rb
run_or_echo git -C "$TAP_DIR" commit -m "release: $TAG"
run_or_echo git -C "$TAP_DIR" push origin HEAD

if [[ "$DRY_RUN" -eq 0 ]]; then
  echo
  echo "Release prep complete for $TAG."
  echo "Next steps:"
  echo "  1) Review riff changes: git -C $ROOT_DIR status && git -C $ROOT_DIR diff"
  echo "  2) Commit riff release files: git -C $ROOT_DIR add Cargo.toml VERSION && git -C $ROOT_DIR commit -m \"release: $TAG\""
  echo "  3) Push riff commit: git -C $ROOT_DIR push origin HEAD"
  echo "  4) Push/verify tag: git -C $ROOT_DIR push origin $TAG"
  echo "  5) Install: brew tap calebcauthon/riff && brew install calebcauthon/riff/riff"
else
  echo
  echo "Dry-run complete. No files or tags were changed."
  echo "Run without --dry-run once ready."
fi
