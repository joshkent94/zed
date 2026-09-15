---
name: publish-app
description: Publish this Zed fork by rebasing personal onto upstream main, pushing personal to the fork, and rebuilding and installing Zed Dev.app on macOS with runtime shaders. Use when the user asks to publish or update their personal Zed app.
---

# Publish App

Complete the Git update, remote push, release build, and installation. Invocation authorizes these actions, including replacing the installed Zed Dev app without a backup. Creating or editing this skill does not invoke it.

## Rebase and push

1. Read the repository instructions and inspect the working tree, active Git operations, branches, worktrees, and remote URLs. This repository uses `origin` for `zed-industries/zed` and `fork` for `joshkent94/zed`; verify those identities live. Use the existing `personal` branch. If it is checked out elsewhere, work there. Preserve uncommitted work: do not automatically commit, stash, or discard it; ask for direction if it prevents publication.
2. Fetch upstream `main` and the fork's `personal` branch. Record the fetched fork tip for the push lease. Confirm that the fork tip is an ancestor of local `personal`; if it contains work absent locally, stop and explain before rewriting it. Create a missing remote branch only when local `personal` already exists and its identity is verified.
3. Switch to `personal` and run `git rebase origin/main`. Resolve straightforward conflicts while preserving both upstream behavior and personal improvements. Use `GIT_EDITOR=true git rebase --continue`. Ask only when resolution requires a product decision or unrelated changes; report any paused rebase explicitly. Do not silently drop personal commits except changes Git proves are already upstream.
4. Run `git diff origin/main --check` and checks appropriate to any conflict resolutions. Verify that the personal app configuration still uses the `dev` release channel, the `Zed Dev` name and identifier, and the stable icon paths. These are personal customizations to preserve during conflict resolution, not permission to introduce unrelated edits.
5. Push only `personal` to the verified fork. For an existing remote branch, use an explicit lease against the recorded tip:

   ```sh
   git push --force-with-lease=refs/heads/personal:<fetched-fork-tip> fork personal:refs/heads/personal
   ```

   For a new remote branch, use `git push -u fork personal`. Verify the remote tip equals local `HEAD` and record that published SHA. A lease rejection means the remote changed: inspect it and report the divergence instead of retrying with a refreshed lease automatically. Complete the push before building, as requested.

## Build with runtime shaders

Read the current `script/bundle-mac`, `.cargo/bundle-config.toml`, and `crates/zed/Cargo.toml` before packaging. Follow their current asset and dependency requirements; the recipe below records the working local route. Adapt it if upstream changes the tooling.

Use an optimized release build and `gpui_platform/runtime_shaders` for **both** Zed and the separately built remote server. This avoids requiring Xcode's build-time Metal compiler. Command Line Tools and the repository's Rust toolchain are still needed. Do not install full Xcode merely because the ordinary packaging script fails to find `metal`.

1. Determine the host target from `rustc -vV` and install that Rust target if needed. Use the `cargo-bundle` version/fork specified by `script/bundle-mac`. Generate licenses once with `script/generate-licenses`; preserve its error checks. Reuse these generated licenses when retrying later packaging steps.
2. From the repository root, set the build environment and compile, replacing `<host-target>` with the discovered target:

   ```sh
   export ZED_RELEASE_CHANNEL=dev
   export ZED_BUNDLE=true
   export CXXFLAGS="-stdlib=libc++"
   cargo --config .cargo/bundle-config.toml build --release --package zed --package cli --target <host-target> --features gpui_platform/runtime_shaders
   cargo --config .cargo/bundle-config.toml build --release --package remote_server --target <host-target> --features gpui_platform/runtime_shaders
   ```

   Keep the remote-server command separate to avoid dependency feature unification. Capture build output in a temporary log, report meaningful progress, and inspect failures before retrying. Do not treat a background process launch as a successful build.
3. Package the freshly built executable using the dev metadata. The current bundler requires temporarily renaming `[package.metadata.bundle-dev]` to `[package.metadata.bundle]` in `crates/zed/Cargo.toml`, as the upstream script does. Preserve the exact original file in a temporary directory, arrange restoration on success and failure, and restore it immediately after bundling. Never commit this temporary rename or overwrite user edits made during the build.

   From `crates/zed`, run:

   ```sh
   TERM=xterm-256color CARGO_BUNDLE_SKIP_BUILD=true cargo bundle --release --target <host-target> --select-workspace-root
   ```

   `TERM=dumb` can make this bundler panic while printing diagnostics. `CARGO_BUNDLE_SKIP_BUILD=true` prevents a second build that omits runtime shaders; use it only after the two build commands succeed for the published source. Use the actual bundle path printed by the tool.
4. Complete the bundle using the current `script/bundle-mac` recipe: include the freshly built `cli`, `Document.icns`, the dev provisioning profile when required, and the target-specific bundled Git. Use the Git version and download URL from that script rather than a separately pinned version. Download and unpack into a temporary directory.
5. Apply the script's local/ad-hoc signing procedure, with its local entitlements (exclude associated-domain entitlements if present). Sign after all bundle contents are final. A paid signing identity, notarization, DMG, and remote-server archive are unnecessary for this local app installation. Verify the staged `.app` with `codesign --verify --deep --strict` before touching the installed app.

### Known packaging traps

- The current `script/bundle-mac` does not pass through Cargo feature flags. Prefer the explicit commands above. If adapting it with a temporary Cargo wrapper, use an executable in a temporary `PATH` directory, not an exported shell function: shell tracing inside `generate-licenses` otherwise writes to stderr and trips its strict license check.
- The script's release-mode `-i` path moves the app into Applications before later DMG steps try to move it again. Assemble the bundle and install directly instead.
- Resume a failed packaging/signing step from verified build artifacts when the published source and build inputs are unchanged; avoid restarting compilation and license generation unnecessarily.

## Install and verify

1. Confirm the staged app identifies as `dev.zed.Zed-Dev`, uses the standard Zed icon, and corresponds to the published SHA. Check that `HEAD` has not changed during the build and account for any working-tree changes before installing.
2. Check whether `/Applications/Zed Dev.app` is running. Ask the user to quit it if necessary so unsaved work is preserved; do not force-quit it. Then replace **only** `/Applications/Zed Dev.app` with the verified staged bundle. If a destination exists, validate that it is the expected app and not a symlink before removing it. Do not create a backup of the old installation. Leave `/Applications/Zed.app` untouched.
3. Verify the installed signature, bundle identifier/version, and CLI:

   ```sh
   codesign --verify --deep --strict '/Applications/Zed Dev.app'
   '/Applications/Zed Dev.app/Contents/MacOS/cli' --version
   ```

   The main `Contents/MacOS/zed` executable does not accept `--version`. Inspect the installed icon resource against the newly packaged icon. Manual UI testing and launching the app are opt-in.
4. Restore temporary manifest changes and remove temporary wrappers, downloads, logs, and staging directories created by this invocation. If installation fails after replacement, report that the old app was removed without a backup and retain the verified new bundle until installation is repaired.
5. After successful installation, remove this checkout's debug build outputs (`target/debug` and `target/<host-target>/debug`, where present). Resolve the actual Cargo target directory first, including any configuration override; verify exact paths are generated directories, not symlinks, and that no build is using them. Skip shared or ambiguous targets and report why. Preserve both `target/<host-target>/release` and `target/release`: the latter contains host-side build scripts and procedural macros used by explicit-target release builds. Keep shared Cargo download caches and Rust toolchains, and do not run `cargo clean`. Report the size of the removed outputs and that future debug builds will recreate them.

Finish with the published commit, a clickable `/Applications/Zed Dev.app` link, and verification results. If any phase fails, distinguish what was already pushed or installed from what remains incomplete. Mention that the previous installed copy was replaced without a backup when applicable.
