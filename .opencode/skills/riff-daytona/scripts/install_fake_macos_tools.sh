#!/usr/bin/env bash
set -euo pipefail

dir="${1:-/tmp/riff-fakebin}"
mkdir -p "$dir"

cat > "$dir/ffmpeg" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
if [[ "$*" == *"-list_devices true"* ]]; then
  echo "AVFoundation audio devices"
  echo "[0] Built-in Microphone"
  exit 0
fi
out="${@: -1}"
mkdir -p "$(dirname "$out")"
: > "$out"
trap 'exit 0' INT TERM
while true; do sleep 1; done
EOF

cat > "$dir/screencapture" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
out="${@: -1}"
mkdir -p "$(dirname "$out")"
printf '%b' '\x89\x50\x4E\x47\x0D\x0A\x1A\x0A\x00\x00\x00\x0D\x49\x48\x44\x52\x00\x00\x00\x01\x00\x00\x00\x01\x08\x06\x00\x00\x00\x1F\x15\xC4\x89\x00\x00\x00\x0A\x49\x44\x41\x54\x78\x9C\x63\x00\x01\x00\x00\x05\x00\x01\x0D\x0A\x2D\xB4\x00\x00\x00\x00\x49\x45\x4E\x44\xAE\x42\x60\x82' > "$out"
exit 0
EOF

cat > "$dir/osascript" <<'EOF'
#!/usr/bin/env bash
printf 'TestApp\tcom.example.TestApp\t4242\tExample Window\n'
exit 0
EOF

for tool in afplay open pbcopy ps; do
  printf '#!/usr/bin/env bash\nexit 0\n' > "$dir/$tool"
done

chmod +x "$dir"/*
echo "stub macOS tools installed in $dir"
