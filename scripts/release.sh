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
AUTO_COMMIT=0
CARGO_JOBS="${CARGO_BUILD_JOBS:-}"
SOURCE_MODE="${RIFF_RELEASE_SOURCE:-auto}"

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
  --auto-commit   Commit Cargo.toml, Cargo.lock, and VERSION before tagging
  --jobs <n>      Limit cargo build/test jobs (default: min(host CPUs, 4))
  --source <mode> Formula source: auto, tarball, or git (default: auto)
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
    --auto-commit)
      AUTO_COMMIT=1
      shift
      ;;
    --jobs)
      if [[ $# -lt 2 ]]; then
        echo "--jobs requires a positive integer argument" >&2
        exit 2
      fi
      CARGO_JOBS="$2"
      shift 2
      ;;
    --source)
      if [[ $# -lt 2 ]]; then
        echo "--source requires one of: auto, tarball, git" >&2
        exit 2
      fi
      SOURCE_MODE="$2"
      shift 2
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

# Make git network operations fail instead of sitting forever at credential prompts.
export GIT_TERMINAL_PROMPT=0
if [[ -z "${GIT_SSH_COMMAND:-}" ]]; then
  export GIT_SSH_COMMAND="ssh -o BatchMode=yes"
fi

if [[ -z "$CARGO_JOBS" ]]; then
  cpu_count="$(getconf _NPROCESSORS_ONLN 2>/dev/null || sysctl -n hw.ncpu 2>/dev/null || echo 4)"
  if [[ ! "$cpu_count" =~ ^[0-9]+$ ]] || [[ "$cpu_count" -lt 1 ]]; then
    cpu_count=4
  fi
  if [[ "$cpu_count" -gt 4 ]]; then
    CARGO_JOBS=4
  else
    CARGO_JOBS="$cpu_count"
  fi
fi

if [[ ! "$CARGO_JOBS" =~ ^[0-9]+$ ]] || [[ "$CARGO_JOBS" -lt 1 ]]; then
  echo "Invalid --jobs value '$CARGO_JOBS'. Expected a positive integer." >&2
  exit 2
fi

case "$SOURCE_MODE" in
  auto|tarball|git) ;;
  *)
    echo "Invalid --source value '$SOURCE_MODE'. Expected one of: auto, tarball, git." >&2
    exit 2
    ;;
esac

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

print_command() {
  printf '%q ' "$@"
  printf '\n'
}

run_or_echo() {
  if [[ "$DRY_RUN" -eq 1 ]]; then
    printf '[dry-run] '
    print_command "$@"
  else
    printf '[riff-release] + '
    print_command "$@"
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

update_formula_tarball_file() {
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
updated, url_count = re.subn(
    r'(?m)^  url .+$',
    f'  url "{tarball_url}"',
    text,
    count=1,
)
if url_count != 1:
    raise SystemExit(f"Failed to update url in {formula_path}")
updated, sha_count = re.subn(
    r'(?m)^  sha256 .+$',
    f'  sha256 "{sha}"',
    updated,
    count=1,
)
if sha_count == 0:
    updated, insert_count = re.subn(
        r'(?m)^(  url ".+".*)$',
        rf'\1\n  sha256 "{sha}"',
        updated,
        count=1,
    )
    if insert_count != 1:
        raise SystemExit(f"Failed to insert sha256 in {formula_path}")

if updated == text:
    raise SystemExit(f"No formula changes applied in {formula_path}")

formula_path.write_text(updated)
PY
}

update_formula_git_file() {
  local formula_file="$1"
  local git_url="$2"
  local tag="$3"
  local revision="$4"
  python3 - "$formula_file" "$git_url" "$tag" "$revision" <<'PY'
import pathlib
import re
import sys

formula_path = pathlib.Path(sys.argv[1])
git_url = sys.argv[2]
tag = sys.argv[3]
revision = sys.argv[4]

text = formula_path.read_text()
url_line = f'  url "{git_url}", tag: "{tag}", revision: "{revision}"'
updated, url_count = re.subn(r'(?m)^  url .+$', url_line, text, count=1)
if url_count != 1:
    raise SystemExit(f"Failed to update url in {formula_path}")
updated = re.sub(r'(?m)^  sha256 .+\n', '', updated, count=1)

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
tmp_dir="$(mktemp -d "${TMPDIR:-/tmp}/riff-release.${TAG}.XXXXXX")"
tmp_tarball="$tmp_dir/source.tar.gz"
cleanup() {
  rm -rf "$tmp_dir"
}
trap cleanup EXIT

echo "[riff-release] Preparing release $VERSION ($TAG)"
echo "[riff-release] Repo root: $ROOT_DIR"
echo "[riff-release] GitHub repo: $repo_slug"
echo "[riff-release] Cargo jobs: $CARGO_JOBS"
echo "[riff-release] Formula source mode: $SOURCE_MODE"

echo "[riff-release] Updating Cargo.toml and VERSION"
if [[ "$DRY_RUN" -eq 1 ]]; then
  echo "[dry-run] update version metadata to $VERSION"
else
  update_version_files "$CARGO_TOML" "$VERSION_FILE" "$VERSION"
fi

echo "[riff-release] Running cargo build --release"
run_or_echo cargo build --release --jobs "$CARGO_JOBS"

if [[ "$SKIP_TESTS" -eq 0 ]]; then
  echo "[riff-release] Running cargo test"
  run_or_echo cargo test --jobs "$CARGO_JOBS"
else
  echo "[riff-release] Skipping cargo test (--skip-tests)"
fi

if [[ "$DRY_RUN" -eq 0 ]]; then
  release_metadata_status="$(git -C "$ROOT_DIR" status --porcelain -- Cargo.toml Cargo.lock VERSION)"
  if [[ -n "$release_metadata_status" ]]; then
    if [[ "$AUTO_COMMIT" -eq 1 ]]; then
      echo "[riff-release] Committing release metadata"
      run_or_echo git -C "$ROOT_DIR" add Cargo.toml Cargo.lock VERSION
      run_or_echo git -C "$ROOT_DIR" commit -m "release: $TAG"
    else
      echo
      echo "[riff-release] Release metadata changed; not creating or pushing $TAG yet."
      echo "[riff-release] This prevents tagging the pre-release commit by accident."
      echo
      echo "Changed release files:"
      git -C "$ROOT_DIR" status --short -- Cargo.toml Cargo.lock VERSION
      echo
      echo "Next steps:"
      echo "  1) Review: git -C $ROOT_DIR diff -- Cargo.toml Cargo.lock VERSION"
      echo "  2) Commit: git -C $ROOT_DIR add Cargo.toml Cargo.lock VERSION && git -C $ROOT_DIR commit -m \"release: $TAG\""
      echo "  3) Rerun: $0${PUSH_TAG:+ --push-tag} $TAG"
      echo
      echo "Or rerun with --auto-commit to let this script make the release commit."
      exit 0
    fi
  fi
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

if [[ "$DRY_RUN" -eq 1 ]]; then
  remote_tag_sha=""
else
  remote_tag_sha="$(git -C "$ROOT_DIR" ls-remote --tags origin "refs/tags/$TAG" | awk '{print $1}' || true)"
fi
if [[ -z "$remote_tag_sha" ]]; then
  if [[ "$DRY_RUN" -eq 1 ]]; then
    echo "[dry-run] would check remote tag $TAG and push if missing before checksum fetch"
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

release_revision="$(git -C "$ROOT_DIR" rev-parse "$TAG^{}" 2>/dev/null || git -C "$ROOT_DIR" rev-parse HEAD)"
resolved_source_mode="$SOURCE_MODE"
if [[ "$resolved_source_mode" == "auto" ]]; then
  if curl --fail --head --location --silent --show-error --connect-timeout 10 --max-time 20 "https://github.com/$repo_slug" >/dev/null 2>&1; then
    resolved_source_mode="tarball"
  else
    resolved_source_mode="git"
  fi
fi

echo "[riff-release] Resolved formula source: $resolved_source_mode"

if [[ "$resolved_source_mode" == "tarball" ]]; then
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
      if curl --fail --location --silent --show-error --connect-timeout 20 --max-time 120 "$tarball_url" --output "$tmp_tarball"; then
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
      echo "If this GitHub repo is private, use --source git or make the repo public." >&2
      echo "Private GitHub repos return 404 for unauthenticated archive URLs, which is what Homebrew tarball formulas use." >&2
      echo "You can verify with:" >&2
      echo "  curl -I -L $tarball_url" >&2
      exit 1
    fi
    sha256_value="$(shasum -a 256 "$tmp_tarball" | awk '{print $1}')"
  fi

  echo "[riff-release] Updating tap formula at $FORMULA_FILE"
  if [[ "$DRY_RUN" -eq 1 ]]; then
    echo "[dry-run] set formula url=$tarball_url sha256=$sha256_value"
  else
    update_formula_tarball_file "$FORMULA_FILE" "$tarball_url" "$sha256_value"
  fi
else
  git_source_url="$origin_url"
  if [[ "$origin_url" == https://github.com/* ]]; then
    git_source_url="git@github.com:${repo_slug}.git"
  fi
  echo "[riff-release] Using git source because tarball URLs require a public repo"
  echo "[riff-release] Git source: $git_source_url tag=$TAG revision=$release_revision"
  echo "[riff-release] Updating tap formula at $FORMULA_FILE"
  if [[ "$DRY_RUN" -eq 1 ]]; then
    echo "[dry-run] set formula git url=$git_source_url tag=$TAG revision=$release_revision"
  else
    update_formula_git_file "$FORMULA_FILE" "$git_source_url" "$TAG" "$release_revision"
  fi
fi

echo "[riff-release] Committing and pushing tap repo"
run_or_echo git -C "$TAP_DIR" add Formula/riff.rb
run_or_echo git -C "$TAP_DIR" commit -m "release: $TAG"
run_or_echo git -C "$TAP_DIR" push origin HEAD

if [[ "$DRY_RUN" -eq 0 ]]; then
  echo
  echo "Release prep complete for $TAG."
  echo "Next steps:"
  echo "  1) Push riff commit if needed: git -C $ROOT_DIR push origin HEAD"
  echo "  2) Push/verify tag: git -C $ROOT_DIR push origin $TAG"
  echo "  3) Install: brew tap calebcauthon/riff && brew install calebcauthon/riff/riff"
else
  echo
  echo "Dry-run complete. No files or tags were changed."
  echo "Run without --dry-run once ready."
fi
