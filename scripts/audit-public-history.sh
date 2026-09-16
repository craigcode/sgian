#!/usr/bin/env bash
# Public-history audit (docs/public-readiness.md). Run before the repository
# is made public and before every public release, from a clone that has every
# branch and tag: public visibility exposes old blobs, tags and commit
# messages, not just the current tree.
#
# Fails closed. Prints counts and commit ids, never the matched content.
set -euo pipefail

cd "$(dirname "$0")/.."

if git rev-parse --is-shallow-repository | grep -q true; then
  echo 'public-history audit: refusing a shallow clone; fetch full history first' >&2
  exit 1
fi

# Secrets: every reachable ref, redacted output.
if [ "${SGIAN_SKIP_GITLEAKS:-0}" != 1 ]; then
  command -v gitleaks >/dev/null 2>&1 || {
    echo 'public-history audit: gitleaks is required (brew install gitleaks), or set SGIAN_SKIP_GITLEAKS=1 for a local dry run' >&2
    exit 1
  }
  gitleaks git --no-banner --redact --log-opts='--all' .
fi

# Personal markers that must not appear in any reachable patch, message or
# identity header. Built from fragments so this script does not itself carry
# the literal strings.
markers=(
  '/Users/'craig'martin'
  'craig''@''craigmartin.com'
  'craigmartin8008''@'
  'Craigs-'Mac'-mini'
)
if [ -n "${SGIAN_PUBLIC_AUDIT_MARKERS_FILE:-}" ]; then
  while IFS= read -r line; do
    case "$line" in ''|'#'*) continue ;; esac
    markers+=("$line")
  done < "$SGIAN_PUBLIC_AUDIT_MARKERS_FILE"
fi

failed=0
for marker in "${markers[@]}"; do
  patches="$(git log --all --format='%H %cs' -G"$(printf '%s' "$marker" | sed 's/[][\.*^$/]/\\&/g')" -- . || true)"
  if [ -n "$patches" ]; then
    printf 'public-history audit: marker %d remains in reachable patches:\n%s\n' "${#marker}" "$patches" >&2
    failed=1
  fi
  messages="$(git log --all --format='%H %cs%x09%B' | grep -F "$marker" | cut -f1 || true)"
  if [ -n "$messages" ]; then
    printf 'public-history audit: marker %d remains in commit messages:\n%s\n' "${#marker}" "$messages" >&2
    failed=1
  fi
  identities="$(git log --all --format='%H %an <%ae> %cn <%ce>' | grep -F "$marker" | cut -d' ' -f1 || true)"
  if [ -n "$identities" ]; then
    printf 'public-history audit: marker %d remains in author/committer headers:\n%s\n' "${#marker}" "$identities" >&2
    failed=1
  fi
done

# Every human commit must use a noreply identity.
# (GitHub's own `noreply@github.com` is the web-flow committer on squash merges.)
humans="$(git log --all --format='%ae%n%ce' | grep -v -e 'users.noreply.github.com' -e '^noreply@github.com$' -e '^$' | sort -u || true)"
if [ -n "$humans" ]; then
  printf 'public-history audit: %d non-noreply identities in history\n' "$(printf '%s\n' "$humans" | wc -l | tr -d ' ')" >&2
  failed=1
fi

# No large binaries hiding in history (the app icon is the known exception).
big="$(git rev-list --objects --all | git cat-file --batch-check='%(objecttype) %(objectsize) %(rest)' | awk '$1=="blob" && $2>5000000 {print $2, $3}' || true)"
if [ -n "$big" ]; then
  printf 'public-history audit: blobs over 5 MiB in history:\n%s\n' "$big" >&2
  failed=1
fi

if [ "$failed" -ne 0 ]; then
  echo 'public-history audit: FAILED' >&2
  exit 1
fi
echo 'public-history audit: ok'
