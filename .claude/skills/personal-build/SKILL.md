---
name: personal-build
description: Fold the working tree into the single commit on the `personal` branch, then rebase, rebuild, install Zed Dev and force-push to the fork. Use when the user asks to fold in, amend, ship, publish, or "run the full build and push" on the personal branch.
---

# Personal Build

Invoking this skill authorizes the amend and the lease-protected force-push to `fork/personal`. It authorizes nothing else.

## Steps

### 1. Check the branch

Run `git branch --show-current`, `git status --short` and `git rev-list --count origin/main..HEAD`.

Stop and report when the branch is not `personal`, when it is more than one commit ahead of `origin/main`, or when a rebase or another `zed-personal-build` run is already in progress.

**Complete when:** the branch is `personal` with at most one commit above `origin/main`.

### 2. Fold in the working tree

Skip this step when the tree is clean.

Stage every modified tracked file. Stage an untracked file only when this session created it as part of the work; name any other untracked file to the user and leave it out. Never stage secrets or `.claude/settings.local.json`.

Run `git commit --amend --no-edit`. When the branch has no commit of its own yet, create one instead of amending an upstream commit.

**Complete when:** `git status --short` shows nothing left that belongs to the work, and the branch is exactly one commit ahead of `origin/main`.

### 3. Run the script

Run in the background with a one-hour limit:

```sh
ZED_PERSONAL_BUILD_CODEX=1 ~/.local/bin/zed-personal-build > /tmp/zed-personal-build-run.log 2>&1
```

The script fetches `origin/main`, rebases with autostash, runs `script/bundle-mac -i` when HEAD changed since the last build, then pushes `HEAD` to `fork/personal` with `--force-with-lease`. `ZED_PERSONAL_BUILD_CODEX=1` leaves a conflicted rebase in place for this session instead of opening another terminal.

Leave the working tree and HEAD alone until it exits: the script pushes whatever HEAD is at the end.

**Complete when:** the script has exited.

### 4. Verify and report

On exit 0, confirm each of these and report the values:

- `git ls-remote fork refs/heads/personal` equals `git rev-parse HEAD`
- `git status --short` is empty
- `stat -f "%Sm" "/Applications/Zed Dev.app/Contents/MacOS/zed"` is from this run, unless the script skipped the build because HEAD was already built

Tell the user to restart Zed Dev.

On a non-zero exit, read `/tmp/zed-personal-build-run.log` and report the failing stage with its output:

- **Rebase conflict:** resolve it with the user, asking before any non-obvious choice, then repeat from step 3.
- **Build failure:** report the first compiler error. Fix it only when the user asks.
- **Push failure:** report it. The bundle is already installed. Do not retry with a plain `--force`.

**Complete when:** the user has the commit SHA, the fork ref, and the install time, or the exact failure.
