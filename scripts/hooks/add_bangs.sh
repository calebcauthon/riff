#!/usr/bin/env bash
#
# riff TEST output hook: append five exclamation marks to the transcript.
#
# Invoked by riff as:
#   add_bangs.sh <transcript_path> <metadata_path>
#
# $1 (transcript_path) is a temp file holding the current transcript. Edit it
#    in place; riff reads it back as the new transcript.
# $2 (metadata_path) is a temp file holding a read-only JSON blob of session
#    metadata.
#
# Handy for confirming that a hook (or the --with-post-hook flag) actually ran.
set -euo pipefail

transcript="${1:?transcript path required}"

perl -0777 -i -pe 's/\s+\z//; s/\z/!!!!!/' "$transcript"
