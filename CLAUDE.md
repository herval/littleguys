# littleguys

## Git

- Work directly on `main`. Don't create feature branches; commit to `main` when asked to commit.

## Releasing

A `v*` tag runs `.github/workflows/release.yml`. It builds the universal app, signs it with the Developer ID, notarizes it, and publishes the GitHub release: the `.dmg`, the update bundle and `latest.json`, which the in-app updater reads. It then pushes the cask to herval/homebrew-tap, which is what `brew install herval/tap/littleguys` installs.

To cut one (vX.Y.Z):
1. Set the version in `src-tauri/tauri.conf.json`, `package.json` and `src-tauri/Cargo.toml`. The workflow fails if the tag doesn't match `tauri.conf.json`. Run `cd src-tauri && cargo check` so `Cargo.lock` picks up the new version.
2. Commit just those files ("Release vX.Y.Z"), then `git tag vX.Y.Z && git push origin main vX.Y.Z`.
3. Watch the run (`gh run list --workflow release.yml`, then `gh run watch <id>`). It takes about 25 minutes. Credentials are checked in the first minute, and most of the rest is the Rust build and Apple's notarization queue.
4. Check it shipped:
   - The release has `littleguys_X.Y.Z_universal.dmg`, `littleguys_universal.app.tar.gz` (+ `.sig`) and `latest.json`.
   - `Casks/littleguys.rb` in herval/homebrew-tap has the new version, and its sha256 matches the `.dmg`'s.

If it fails:
- **"Signing and notarization credentials work" fails:** a secret is missing or stale. Ask herval to run `make release-secrets` and replace the one the error names. Claude can't set GitHub secrets, because auto mode blocks it.
- **It fails later:** the release stays a draft, which the updater and brew never see. Fix the cause, `gh release delete vX.Y.Z`, move the tag to the fix (`git push origin :refs/tags/vX.Y.Z`, then tag and push again).
- **Never move the tag of a published release.** Ship the next patch version instead.

Credentials live in the repo's GitHub secrets, and `make release-secrets` sets them all. They only need redoing to rotate:
- the app-specific password, used for notarization;
- the Developer ID Application certificate (team N57JK357GW, from the G2 Sub-CA, expires 2031). Never use a Previous Sub-CA certificate; that CA expires in February 2027.
