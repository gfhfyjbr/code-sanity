# Releasing code-sanity

Releases are tag-driven. GitHub Actions refuses a tag whose version differs
from `Cargo.toml` or whose commit is not reachable from `origin/main`.

## Prepare

1. Move the relevant entries from `CHANGELOG.md`'s `Unreleased` section into a
   dated `## [X.Y.Z]` section.
2. Set the same `version = "X.Y.Z"` in `Cargo.toml` and refresh `Cargo.lock`.
3. Merge the release commit into `main` and confirm CI is green.

## Publish

```bash
git switch main
git pull --ff-only origin main
git tag -a vX.Y.Z -m "Release vX.Y.Z"
git push origin vX.Y.Z
```

The `Release` workflow then:

1. validates tag/version parity and main-branch ancestry;
2. validates the installer and MSRV, then runs format, clippy, tests, and
   dependency policy checks;
3. builds Linux and macOS binaries for x86_64 and aarch64;
4. smoke-tests native binaries and produces `.tar.gz` plus `.sha256` assets;
5. creates the GitHub Release from the matching changelog section;
6. downloads that release through `install.sh` on fresh Ubuntu and macOS
   runners and executes the installed binary.

Do not upload replacement binaries manually. If a workflow fails before the
release is created, fix the problem and create a new tag after the fix. If it
fails after publication, leave the assets intact, document the issue, and ship
a patch release.
