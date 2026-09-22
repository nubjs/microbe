# Releasing

Two registries, one rule: no credential on a machine or in CI can make a version installable on its own. A human with 2FA stands between CI and the registry.

## Versions

`Cargo.toml`, `napi/Cargo.toml`, `napi/package.json` and every `optionalDependencies` entry in it carry the same version. A release commit changes those files and `CHANGELOG.md`, and nothing else. A lifecycle script or a workflow file changing in the same commit is a red flag, not a release.

## crates.io

Published from the maintainer's own terminal, never from CI: crates.io has no staging step, so a publish from CI would make a stolen push credential enough to ship a crate.

```sh
cargo publish --dry-run --locked
cargo publish --locked          # a short-lived token minted in the browser for this release, revoked after
```

## npm

The `napi` workflow runs on a `v*` tag. It builds the addon for eight targets, smoke-installs a real package with the native ones, and then stages `@nubjs/microbe` and its eight platform packages with trusted publishing. Nothing is installable yet.

```sh
npm stage view <stage-id>       # one id per package, printed by the workflow
npm stage download <stage-id>   # diff against the previous tarball: only what the changelog says
npm stage approve <stage-id>    # 2FA; this is the publish
```

Registry settings that make this hold, set once per package on npmjs.com: the trusted publisher for `nubjs/microbe` and `napi.yml` allows `npm stage publish` only, and the package requires two-factor authentication and disallows tokens. The first version of each package is published by hand from the maintainer's terminal, because a trusted publisher can only be configured on a package that exists.

## Tags

The maintainer pushes the `v*` tag after the crates.io publish. The tag starts the `napi` workflow, which builds and stages; it publishes nothing, so a tag pushed by anyone else stages at most a version nobody approves.
