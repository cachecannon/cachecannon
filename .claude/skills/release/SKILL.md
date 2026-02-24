---
name: release
description: Create a release PR with version bump, then tag after merge
---

Create a release PR that bumps the version. After the PR is merged, tag the release to trigger the release workflow which builds and publishes packages.

## Arguments

The skill accepts a version level argument:
- `patch` - 0.0.1 -> 0.0.2
- `minor` - 0.0.1 -> 0.1.0
- `major` - 0.0.1 -> 1.0.0
- Or an explicit version like `0.1.0`

Example: `/release minor`

## Steps

1. **Verify prerequisites**:
   - Must be on `main` branch
   - Working directory must be clean
   - Must be up to date with origin/main

   ```bash
   git fetch origin
   if [ "$(git branch --show-current)" != "main" ]; then
     echo "Error: Must be on main branch"
     exit 1
   fi
   if [ -n "$(git status --porcelain)" ]; then
     echo "Error: Working directory not clean"
     exit 1
   fi
   if [ "$(git rev-parse HEAD)" != "$(git rev-parse origin/main)" ]; then
     echo "Error: Not up to date with origin/main"
     exit 1
   fi
   ```

2. **Run local checks**:
   ```bash
   cargo clippy --all-targets --all-features -- -D warnings
   cargo test --all
   ```
   If checks fail, stop and report the errors.

3. **Determine the new version**:
   - Read the current version from `Cargo.toml` (root, single crate)
   - Calculate the new version based on the level argument (patch/minor/major) or use the explicit version provided

4. **Create release branch**:
   ```bash
   NEW_VERSION="X.Y.Z"  # from step 3
   git checkout -b release/v${NEW_VERSION}
   ```

5. **Bump version in Cargo.toml**:
   - Update the `version = "..."` field in the root `Cargo.toml`
   - Run `cargo check` to regenerate `Cargo.lock`

6. **Update CHANGELOG.md**:
   - Move items from the "Unreleased" section to a new version section with the release date
   - Create a new empty "Unreleased" section
   - The changelog follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) format
   - Ask the user if they want to review/edit the changelog before proceeding

7. **Commit changes**:

   **CRITICAL**: The commit message MUST start with `release: v` for consistency.

   ```bash
   git add Cargo.toml Cargo.lock CHANGELOG.md
   git commit -m "release: v${NEW_VERSION}"
   ```

8. **Push and create PR**:
   ```bash
   git push -u origin release/v${NEW_VERSION}

   gh pr create \
     --title "release: v${NEW_VERSION}" \
     --body "$(cat <<EOF
   ## Release v${NEW_VERSION}

   This PR prepares the release of v${NEW_VERSION}.

   ### Changes
   - Version bump to ${NEW_VERSION}
   - Changelog update

   ### After Merge
   After this PR is merged, run \`/release-tag\` or manually create and push the tag:
   \`\`\`
   git tag v${NEW_VERSION} <merge-commit-sha>
   git push origin v${NEW_VERSION}
   \`\`\`

   This will trigger the release workflow to build and publish packages.
   EOF
   )"
   ```

9. **Report the PR URL** to the user.

## After PR Merge

After the PR is merged to main, a git tag must be created and pushed to trigger the `release.yml` workflow:
```bash
git checkout main
git pull origin main
git tag v${NEW_VERSION}
git push origin v${NEW_VERSION}
```

The release workflow will then:
1. Build .deb and .rpm packages (amd64 + arm64)
2. Sign packages with GPG
3. Publish to APT and YUM S3 repositories
4. Create a GitHub Release with artifacts

## Troubleshooting

- **gh CLI not installed**: `brew install gh` or see https://cli.github.com/
- **Not authenticated with gh**: `gh auth login`
