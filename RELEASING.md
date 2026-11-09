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

Registry settings that make this hold, set once per package on npmjs.com: the trusted publisher for `nubjs/microbe` and `napi.yml` allows `npm stage publish` only, and the package requires two-factor authentication and disallows tokens. The first version of each package is published by hand from the maintainer's terminal, because a trusted publisher can only be configured on a package that exists. See [First publish](#first-publish).

## Tags

The maintainer pushes the `v*` tag after the crates.io publish. The tag starts the `napi` workflow, which builds and stages; it publishes nothing, so a tag pushed by anyone else stages at most a version nobody approves.

## First publish

A trusted publisher can only be configured on a package that exists, so the first version of each name is published with an owner's credential.

1. Set the date in `CHANGELOG.md`, push it, and dispatch the `napi` workflow on that commit.
2. Mint two tokens that expire the next day. Keep each one in the environment of the commands that use it, never in a file.

   | Registry | Page | Settings |
   | --- | --- | --- |
   | crates.io | Account settings, API Tokens | `publish-new` only, crate scope `microbe` |
   | npm | Access Tokens, granular token | Read and write on the `@nubjs` scope, no organization access, bypass 2FA on |

3. Publish the crate, then the eight platform packages, then the main package. npm expands `${NPM_TOKEN}` when it reads the file, so the token itself is never written.

   ```sh
   cargo publish --locked                                     # CARGO_REGISTRY_TOKEN in the environment

   gh run download <napi-run-id> -R nubjs/microbe -D napi/dist
   cd napi
   printf '//registry.npmjs.org/:_authToken=${NPM_TOKEN}\n' > .npmrc
   export NPM_CONFIG_USERCONFIG="$PWD/.npmrc"                  # NPM_TOKEN in the environment
   for d in dist/microbe-*/; do (cd "$d" && npm publish --access public); done
   cp ../LICENSE .
   npm publish --access public
   rm .npmrc LICENSE
   ```

4. Apply the registry settings from the npm section to all nine packages.
5. Push the `v*` tag at the published commit. Its staging job fails because the version already exists, and it publishes nothing.
6. Revoke both tokens.
