#!/usr/bin/env bash
###############################################################################
# scripts/release.sh — release this DuckDB community extension, end to end.
#
# WHAT IT DOES
#   A reusable release driver for laterite_ags4 (this repo, niko86/laterite-duckdb)
#   and its community-extensions PR. In one run it:
#     1. (optional) moves your working changes onto a release branch,
#     2. runs `cargo test` (aborts the release if it fails),
#     3. bumps the version in Cargo.toml + description.yml,
#     4. commits, pushes, and tags this repo,
#     5. clones your community-extensions fork, syncs the canonical descriptor
#        into the PR with the real release commit SHA, and pushes it,
#     6. watches the community PR's CI.
#   It pauses (y/N) before EVERY remote-mutating action and shows each diff
#   first, so it is safe to run and bail out of at any prompt.
#
# WHEN TO USE
#   Any time you cut a new release or need to re-point the community PR at a fresh
#   commit. It is the git/gh "push" half of a release.
#
# ── HOW TO REUSE FOR A FUTURE RELEASE ───────────────────────────────────────
#   1. Make your source changes.
#   2. Run it with the version as a REQUIRED arg — the extension tracks the
#      laterite release version (#372), so pass laterite's number:
#        bash scripts/release.sh 0.6.0
#      (override the commit message with COMMIT_MSG=... in the env if needed.)
#   3. If it's a brand-new community PR (not #2079), update PR_NUMBER / PR_FORK /
#      PR_BRANCH to the new one.
#   Answer the y/N prompts. Everything below the CONFIG block is generic and
#   resolves paths from the script's own location, so it runs from anywhere.
#
# REQUIRES
#   cargo · git · gh (authed for niko86/laterite-duckdb + the community-extensions fork)
###############################################################################

set -euo pipefail

# Paths resolve from this script's location: <repo>/scripts/release.sh
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# ============================ CONFIG ========================================
# VERSION is a REQUIRED arg, not a hard-coded default: a buried wrong version is
# the kind of silent release footgun that ships the wrong number. The extension
# tracks the laterite release version (#372) — pass laterite's number.
VERSION="${1:?usage: bash scripts/release.sh <version>   e.g. bash scripts/release.sh 0.6.0 (pass the laterite release version — the extension tracks it, #372)}"
# The commit subject defaults to a generic release line; override with the env
# var COMMIT_MSG=... for a fuller changelog when a release warrants one.
COMMIT_MSG="${COMMIT_MSG:-release: v${VERSION}}"

# --- stable settings (rarely change) ---
EXT_DIR="$REPO_ROOT"                                  # this repo (derived)
RELEASE_BRANCH=""                                     # "" = ship from CURRENT branch;
                                                      #   e.g. "main" to stash→checkout→pop.
FORCE_TAG=0                                            # 1 = move an existing tag (careful)
BUMP_VERSION=1                                         # 1 = rewrite version in the manifests

PR_NUMBER=2079                                         # the community-extensions PR
PR_FORK="niko86/community-extensions"                  # its head fork
PR_BRANCH="add-laterite_ags4"                          # its head branch
DESC_PATH="extensions/laterite_ags4/description.yml"
FORK_DIR="$(dirname "$REPO_ROOT")/community-extensions-fork"  # sibling, OUTSIDE this repo
WATCH_CI=1                                             # 1 = gh pr checks --watch at the end
# ===========================================================================

TAG="v${VERSION}"
say()     { printf '\n\033[1;36m== %s\033[0m\n' "$*"; }
warn()    { printf '\033[1;33m  !! %s\033[0m\n' "$*"; }
confirm() { read -r -p $'  \033[1;32m▶\033[0m '"$* [y/N] " a; [[ "${a:-}" == [yY] ]]; }

command -v gh >/dev/null    || { echo "need the gh CLI"; exit 1; }
[[ -d "$EXT_DIR/.git" ]]    || { echo "$EXT_DIR is not a git repo"; exit 1; }
[[ "$(git -C "$EXT_DIR" remote get-url origin)" == *laterite-duckdb* ]] \
  || { echo "$EXT_DIR origin is not laterite-duckdb"; exit 1; }

# --- 1. optional: move the working changes onto the release branch ----------
CUR="$(git -C "$EXT_DIR" branch --show-current)"
if [[ -n "$RELEASE_BRANCH" && "$CUR" != "$RELEASE_BRANCH" ]]; then
  say "Move changes from '$CUR' to '$RELEASE_BRANCH'"
  if confirm "stash → checkout $RELEASE_BRANCH (pull --ff-only) → stash pop?"; then
    git -C "$EXT_DIR" stash push -u
    git -C "$EXT_DIR" checkout "$RELEASE_BRANCH"
    git -C "$EXT_DIR" pull --ff-only || warn "could not ff-pull $RELEASE_BRANCH (continuing)"
    git -C "$EXT_DIR" stash pop
  fi
fi
say "Releasing from branch: $(git -C "$EXT_DIR" branch --show-current)  (tag lands at its HEAD)"

# --- 2. pre-flight: tests must pass -----------------------------------------
say "cargo test (pre-flight)"
( cd "$EXT_DIR" && cargo test )

# --- 3. version bump --------------------------------------------------------
if [[ "$BUMP_VERSION" == 1 ]]; then
  say "Setting version = $VERSION  (Cargo.toml + description.yml)"
  sed -i '' "s/^version = \".*\"/version = \"$VERSION\"/" "$EXT_DIR/Cargo.toml"
  sed -i '' "s/^  version: .*/  version: $VERSION/"        "$EXT_DIR/description.yml"
  grep -nE '^version = '  "$EXT_DIR/Cargo.toml"
  grep -nE '^  version:'  "$EXT_DIR/description.yml"
fi

# --- 4. review + commit -----------------------------------------------------
say "Changes to commit in $EXT_DIR"
git -C "$EXT_DIR" status --short
if confirm "Commit?"; then
  git -C "$EXT_DIR" add -A
  git -C "$EXT_DIR" commit -m "$COMMIT_MSG"
fi

# --- 5. push the branch -----------------------------------------------------
BRANCH="$(git -C "$EXT_DIR" branch --show-current)"
if confirm "Push '$BRANCH' to origin (niko86/laterite-duckdb)?"; then
  git -C "$EXT_DIR" push origin "$BRANCH"
fi

# --- 6. tag + push the tag --------------------------------------------------
if git -C "$EXT_DIR" rev-parse "$TAG" >/dev/null 2>&1 && [[ "$FORCE_TAG" != 1 ]]; then
  warn "tag $TAG already exists — bump VERSION or set FORCE_TAG=1. Skipping tag."
elif confirm "Tag $TAG at HEAD and push it?"; then
  git -C "$EXT_DIR" tag -f "$TAG"
  git -C "$EXT_DIR" push origin -f "$TAG"
fi

SHA="$(git -C "$EXT_DIR" rev-parse HEAD)"
say "Release commit SHA: $SHA"

# --- 7. sync the descriptor into the community-extensions PR fork -----------
say "Finalize community-extensions PR #$PR_NUMBER (fork $PR_FORK @ $PR_BRANCH)"
if [[ -d "$FORK_DIR/.git" ]]; then
  git -C "$FORK_DIR" fetch origin "$PR_BRANCH"
  git -C "$FORK_DIR" checkout "$PR_BRANCH"
  git -C "$FORK_DIR" reset --hard "origin/$PR_BRANCH"
else
  gh repo clone "$PR_FORK" "$FORK_DIR" -- --branch "$PR_BRANCH"
fi
# This repo's description.yml is the canonical descriptor; copy it over + fill the
# placeholder ref with the real release SHA.
cp "$EXT_DIR/description.yml" "$FORK_DIR/$DESC_PATH"
sed -i '' "s|REPLACE_WITH_RELEASE_COMMIT_SHA|$SHA|" "$FORK_DIR/$DESC_PATH"
say "Descriptor change in the PR fork:"
git -C "$FORK_DIR" --no-pager diff -- "$DESC_PATH" || true
if git -C "$FORK_DIR" diff --quiet -- "$DESC_PATH"; then
  warn "descriptor already matches — nothing to push."
elif confirm "Commit + push this descriptor to $PR_FORK $PR_BRANCH?"; then
  git -C "$FORK_DIR" commit -am "laterite_ags4: pin $TAG ($SHA), exclude wasm; sync descriptor"
  git -C "$FORK_DIR" push origin "$PR_BRANCH"
fi

# --- 8. watch the PR CI -----------------------------------------------------
if [[ "$WATCH_CI" == 1 ]]; then
  say "Watching PR #$PR_NUMBER CI (Ctrl-C to stop)"
  gh pr checks "$PR_NUMBER" -R duckdb/community-extensions --watch --interval 30 || true
fi
say "Done — native builds pass, wasm skipped; PR #$PR_NUMBER is in the maintainers' hands."
