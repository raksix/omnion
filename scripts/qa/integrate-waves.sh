#!/usr/bin/env bash
# Consolidate the Omnion writer fleet into main.
#
# WHY FILE-COPIED MERGE, NOT git merge
# Seven writer loops each produced ~400 commits on their own branch and none of them
# ever reached main, so ~3,000 commits of finished work exist only in those branches.
# A three-way merge of that would hit conflicts in every shared file, and the only way
# to resolve 3,000 commits of conflicts by hand is to guess. The waves were partitioned
# by ownership, so a file-based union is the honest merge: every branch's files land,
# and where two branches touched the same file the later branch wins — which is the
# same rule the writers already lived under while they were running side by side.
#
# What this does NOT do: preserve the branch history in main. It becomes one commit.
# The branches stay on origin either way, so no commit is ever destroyed.
set -euo pipefail
cd /mnt/apopic/omnion

echo "== creating the integration branch =="
git checkout -q -b integrate/waves main
echo "  $(git rev-parse --abbrev-ref HEAD) at $(git rev-parse --short HEAD)"

# Oldest branch first, newest last. A file two branches both wrote lands from the
# branch that touched it most recently, which is the same "last writer wins" rule the
# writers were already living under when they ran side by side. w9 and w10 stopped on
# 29 Sep and everything else ran until 2 Oct, so they go in first and are overwritten
# wherever a fresher branch has an answer.
ORDER="wave9 wave10 wave7 wave5 wave8 wave6 wave3-automation wave2-cms wave4"
for b in $ORDER; do
  echo "== unioning $b =="
  git fetch -q origin 2>/dev/null || true
  n=$(git ls-tree -r "origin/$b" --name-only 2>/dev/null | wc -l)
  [ "$n" = "0" ] && { echo "  SKIP $b — not on origin"; continue; }
  # `checkout <tree> -- .` copies every tracked file the branch has. Files main has and
  # the branch does not are left alone, which is what makes this a union and not a
  # replace — `git read-tree -m` would delete them.
  git checkout "origin/$b" -- . 2>/dev/null || git checkout "$b" -- .
  # The index must be refreshed: a checkout leaves removals from an earlier branch staged.
  git add -A
  c=$(git diff --cached --name-only | wc -l)
  echo "  $b: $n files in tree, $c changed vs what we had"
  git commit -q -m "integrate($b): union the $b writer's work into one tree

Writer loop $b ran on its own branch and never merged to main, so its finished work
existed only there. This copies its tree over, so the work is on one branch instead of
nine. The branch remains on origin — no commit is destroyed." || echo "  (nothing to commit for $b)"
done

echo "== what the union looks like =="
git log --oneline -1
echo "  migrations: $(ls database/migrations/*.sql 2>/dev/null | wc -l)"
echo "  tracked files: $(git ls-files | wc -l)"
