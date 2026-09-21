#!/usr/bin/env bash
set -euo pipefail

MONOLITH_REMOTE='origin'
MONOLITH_BRANCH='dev'
BOOK_REMOTE='crosslink_book-upstream'
BOOK_BRANCH='code'
BOOK_PREFIX='crosslink_book'
DEFAULT_COMMIT_MESSAGE='Sync crosslink book changes from monolith'

function die
{
  echo "error: $*" >&2
  exit 1
}

repo_root="$(git rev-parse --show-toplevel 2> /dev/null)" \
  || die 'run this script from inside the crosslink_monolith repository'
cd "$repo_root"

current_branch="$(git branch --show-current)"
[[ "$current_branch" == "$MONOLITH_BRANCH" ]] \
  || die "expected branch '$MONOLITH_BRANCH', found '${current_branch:-detached HEAD}'"

git remote get-url "$MONOLITH_REMOTE" > /dev/null \
  || die "missing Git remote '$MONOLITH_REMOTE'"
git remote get-url "$BOOK_REMOTE" > /dev/null \
  || die "missing Git remote '$BOOK_REMOTE'"
[[ -d "$BOOK_PREFIX" ]] \
  || die "missing subtree directory '$BOOK_PREFIX'"
[[ -f "$repo_root/subtree" ]] \
  || die "missing shared subtree tool at '$repo_root/subtree'"

# Subtree commands require a clean tracked worktree. Refuse to accidentally
# include or hide unrelated monolith edits; untracked files outside the subtree
# are left alone.
git diff --quiet -- . ":(exclude)$BOOK_PREFIX" \
  || die "commit or stash tracked changes outside '$BOOK_PREFIX' first"
git diff --cached --quiet -- . ":(exclude)$BOOK_PREFIX" \
  || die "commit or unstage staged changes outside '$BOOK_PREFIX' first"

commit_message="${*:-$DEFAULT_COMMIT_MESSAGE}"

echo "Synchronizing $MONOLITH_REMOTE/$MONOLITH_BRANCH with merge semantics..."
git pull --no-rebase --autostash "$MONOLITH_REMOTE" "$MONOLITH_BRANCH"

if [[ -n "$(git status --porcelain=v1 --untracked-files=all -- "$BOOK_PREFIX")" ]]
then
  echo "Committing local changes beneath $BOOK_PREFIX/..."
  git add -- "$BOOK_PREFIX"
  git commit -m "$commit_message" -- "$BOOK_PREFIX"
else
  echo "No uncommitted changes beneath $BOOK_PREFIX; skipping local commit."
fi

git diff --quiet \
  || die 'tracked worktree changes remain after the book commit'
git diff --cached --quiet \
  || die 'staged changes remain after the book commit'

echo "Pulling $BOOK_REMOTE/$BOOK_BRANCH into $BOOK_PREFIX/..."
bash "$repo_root/subtree" pull "$BOOK_PREFIX" "$BOOK_BRANCH"

echo "Pushing $BOOK_PREFIX/... to $BOOK_REMOTE/$BOOK_BRANCH..."
bash "$repo_root/subtree" push "$BOOK_PREFIX" "$BOOK_BRANCH"

echo "Pushing monolith history to $MONOLITH_REMOTE/$MONOLITH_BRANCH..."
if ! git push "$MONOLITH_REMOTE" "$MONOLITH_BRANCH"
then
  echo 'The monolith remote advanced during synchronization; merging it once and retrying...'
  git pull --no-rebase --autostash "$MONOLITH_REMOTE" "$MONOLITH_BRANCH"
  git push "$MONOLITH_REMOTE" "$MONOLITH_BRANCH"
fi

echo 'Crosslink book and monolith synchronization complete.'
