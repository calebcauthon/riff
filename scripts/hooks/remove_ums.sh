#!/usr/bin/env bash
#
# riff output hook: remove filler "um" tokens from the transcript.
#
# Invoked by riff as:
#   remove_ums.sh <transcript_path> <metadata_path>
#
# $1 (transcript_path) is a temp file holding the current transcript. Edit it
#    in place; riff reads it back as the new transcript.
# $2 (metadata_path) is a temp file holding a read-only JSON blob of session
#    metadata (session id, screenshots, clipboard, transcription info, etc.).
#
# Removes any standalone "um" (case-insensitive) along with a trailing comma or
# period and surrounding whitespace, then tidies up the leftover spacing.
set -euo pipefail

transcript="${1:?transcript path required}"

perl -0777 -i -pe '
    s/\bum\b[,.]?//gi;   # drop "um", "um,", "um." anywhere it stands alone
    s/[ \t]{2,}/ /g;     # collapse doubled spaces left behind
    s/[ \t]+([,.!?])/$1/g; # pull punctuation back against the previous word
    s/^[ \t]+//mg;       # trim leading spaces per line
' "$transcript"
