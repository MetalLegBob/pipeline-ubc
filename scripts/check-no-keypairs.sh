#!/usr/bin/env bash
# Fails if any tracked or staged file looks like a Solana keypair (a JSON array of 64
# byte values, the format solana-keygen writes). Run by the pre-commit hook and CI.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

files=$( { git ls-files; git diff --cached --name-only --diff-filter=ACMR; } | sort -u | grep -E '\.json$' || true)
found=0
for f in $files; do
  [[ -f "$f" ]] || continue
  if python3 - "$f" <<'PY'
import json, sys
try:
    data = json.load(open(sys.argv[1]))
except Exception:
    sys.exit(1)
ok = isinstance(data, list) and len(data) == 64 and all(isinstance(b, int) and 0 <= b <= 255 for b in data)
sys.exit(0 if ok else 1)
PY
  then
    echo "Refusing: $f looks like a Solana keypair. Keep keys in ~/.config/pipeline/keys/." >&2
    found=1
  fi
done
exit $found
