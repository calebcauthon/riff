#!/usr/bin/env bash
#
# riff output hook: remove filler "um" / "uh" tokens from the transcript.
#
# Invoked by riff as:
#   remove_fillers.sh <transcript_path> <metadata_path>
#
# $1 (transcript_path) is a temp file holding the current transcript. Edit it
#    in place; riff reads it back as the new transcript.
# $2 (metadata_path) is a temp file holding a read-only JSON blob of session
#    metadata (session id, screenshots, clipboard, transcription info, etc.).
#
# Removes any standalone filler word from the "um"/"uh" families (case
# insensitive) — um, umm, uh, uhh, uhm, ... — along with a trailing comma or
# period, then tidies up the leftover spacing. Only whole-word matches are
# touched, so real words like "hum", "uhura", or "museum" are left alone.
set -euo pipefail

transcript="${1:?transcript path required}"

perl -0777 -i -pe '
    s/\b(?:u+h+m+|u+m+|u+h+)\b[,.]?//gi; # drop um/uh fillers and a trailing , or .
    s/[ \t]{2,}/ /g;                     # collapse doubled spaces left behind
    s/[ \t]+([,.!?])/$1/g;               # pull punctuation back against the word
    s/^[ \t]+//mg;                       # trim leading spaces per line
    s/[ \t]+$//mg;                       # trim trailing spaces per line
' "$transcript"
