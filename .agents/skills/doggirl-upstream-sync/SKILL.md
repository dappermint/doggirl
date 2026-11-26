---
name: doggirl-upstream-sync
description: >-
  How to safely fetch upstream LadybirdBrowser/ladybird and rebase doggirl
  branches without ever pushing to origin.
---

# doggirl upstream sync runbook

use this procedure when pulling fresh upstream commits from `origin` (`LadybirdBrowser/ladybird`) into `doggirl` (`dappermint/doggirl`)

## 1. verify remotes before any git network action

```bash
cd /Volumes/dev/ladybird
git remote -v
```

expected configuration:
- `origin` -> `LadybirdBrowser/ladybird` (read-only upstream; never push)
- `doggirl` -> `https://github.com/dappermint/doggirl.git` (fork remote)

## 2. fetch upstream and fast-forward `doggirl/master`

1. back up the current working tree diff to `/Volumes/dev/ladybird-perf-run/backup-$(date +%Y%m%d-%H%M).patch` before moving `HEAD`
2. fetch upstream `master`:
   ```bash
   git fetch origin master
   ```
3. check what changed upstream in files touched by our stack:
   ```bash
   git log --oneline HEAD..origin/master -- $(git diff --name-only HEAD)
   ```
4. apply the working tree cleanly onto `origin/master` (or rebase each carved PR branch onto `origin/master`), resolve any semantic conflicts carefully, and update `doggirl`'s `master` only when intentional:
   ```bash
   git push doggirl origin/master:refs/heads/master
   ```
5. re-run the full `doggirl-build-and-test` and `doggirl-lint-and-pr-hygiene` checklists before force-pushing rebased PR branches to `doggirl` with `git push --force-with-lease doggirl ...`
6. on periodic merge cycles (every 4 to 5 promoted waves), merge verified PRs into `doggirl/master` via `GH_PAGER=cat gh pr ready <n> -R dappermint/doggirl && GH_PAGER=cat gh pr merge <n> -R dappermint/doggirl --rebase` in dependency order, updating `master` after each merge so subsequent PRs stay conflict-free and the final tree matches the verified working tree byte-for-byte
