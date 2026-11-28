#!/usr/bin/env bash
# pakem-check.sh - run the checks a project declares in its own pakem.yaml.
#
# Usage: pakem-check.sh [project-dir]   (default: current directory)
#
# The project declares its own commands. Pakem never guesses them.
# Expected pakem.yaml format:
#
#   pakem:
#     version: "1.2.0"
#   checks:
#     test: cargo test --workspace
#     lint: cargo clippy -- -D warnings
#     format: cargo fmt --check
#     build: cargo build
#
# Checks run in the order test, lint, format, build. Only the checks the
# project declares are run, and at least one must be declared.
#
# This script only reads and runs. It never writes files, never removes
# anything, never pushes. Exit 0 when every declared check passes,
# exit 1 when any check fails, exit 2 when pakem.yaml is missing or invalid.
#
# Requires: python3 with PyYAML (present on GitHub ubuntu runners and in
# this sandbox). The same script runs locally and in CI, so both report
# the same result.

set -u
PROJECT_DIR="${1:-.}"
PAKEM_FILE="$PROJECT_DIR/pakem.yaml"

if [ ! -f "$PAKEM_FILE" ]; then
  echo "FAIL: no pakem.yaml found in $PROJECT_DIR" >&2
  echo "Create one with this shape (commands are the project's own):" >&2
  echo "  pakem:" >&2
  echo "    version: \"1.2.0\"" >&2
  echo "  checks:" >&2
  echo "    test: <the project's test command>" >&2
  echo "    lint: <the project's linter command>" >&2
  echo "    format: <the project's formatter command>" >&2
  echo "    build: <the project's build command>" >&2
  exit 2
fi

if ! command -v python3 >/dev/null 2>&1; then
  echo "FAIL: python3 is required to read pakem.yaml" >&2
  exit 2
fi

# Parse pakem.yaml into lines of "name<TAB>command", in canonical order.
CHECKS="$(python3 - "$PAKEM_FILE" <<'PYEOF'
import sys
try:
    import yaml
except ImportError:
    print("FAIL: python3 PyYAML is required to read pakem.yaml", file=sys.stderr)
    sys.exit(2)

path = sys.argv[1]
try:
    with open(path, "r", encoding="utf-8") as f:
        data = yaml.safe_load(f)
except Exception as e:  # noqa: BLE001 - report the parse error, do not guess
    print(f"FAIL: cannot parse {path}: {e}", file=sys.stderr)
    sys.exit(2)

if not isinstance(data, dict) or not isinstance(data.get("checks"), dict):
    print(f"FAIL: {path} must have a top-level 'checks:' mapping", file=sys.stderr)
    sys.exit(2)

checks = data["checks"]
for name in ("test", "lint", "format", "build"):
    cmd = checks.get(name)
    if cmd is None:
        continue
    # YAML reads bare words like `false` as a boolean, not a command.
    # Coerce scalars to their command text so `lint: false` runs the
    # shell command `false`. Lists and mappings are still an error.
    if isinstance(cmd, bool):
        cmd = "true" if cmd else "false"
    elif isinstance(cmd, (int, float)):
        cmd = str(cmd)
    if not isinstance(cmd, str) or not cmd.strip():
        print(f"FAIL: checks.{name} must be a non-empty command string", file=sys.stderr)
        sys.exit(2)
    print(f"{name}\t{cmd.strip()}")
PYEOF
)"
PARSE_STATUS=$?
if [ "$PARSE_STATUS" -ne 0 ]; then
  printf '%s\n' "$CHECKS" >&2
  exit 2
fi

if [ -z "$CHECKS" ]; then
  echo "FAIL: $PAKEM_FILE declares no checks under 'checks:'" >&2
  exit 2
fi

VERSION="$(python3 -c "import yaml,sys; d=yaml.safe_load(open('$PAKEM_FILE')); print(d.get('pakem',{}).get('version','unpinned'))" 2>/dev/null || echo "unpinned")"
echo "pakem-check: $PROJECT_DIR (pakem $VERSION)"
echo "---"

FAILED=0
PASSED=0
cd "$PROJECT_DIR" || exit 2
while IFS="$(printf '\t')" read -r name cmd; do
  [ -z "$name" ] && continue
  echo "run [$name]: $cmd"
  if bash -c "$cmd"; then
    echo "PASS [$name]"
    PASSED=$((PASSED + 1))
  else
    echo "FAIL [$name]: $cmd"
    FAILED=$((FAILED + 1))
  fi
  echo "---"
done <<EOF
$CHECKS
EOF

echo "pakem-check: $PASSED passed, $FAILED failed"
if [ "$FAILED" -gt 0 ]; then
  exit 1
fi
exit 0
