# Releasing Mosaica

One version covers everything: the `mosaica` binary, the Rust crates, the `mosaica` and
`mosaica-native` Python packages, the four `@mosaicajs` npm packages and the container image. Its
one source is `[workspace.package] version` in the root `Cargo.toml`. A release is a tag `vX.Y.Z`
(or `vX.Y.Z-rc.N` for a pre-release) on a commit of `main`, and `.github/workflows/release.yml`
does the rest after one approval.

## One-time set-up

These are done once, by the owner of the repository and the registry accounts.

1. **The `release` environment.** In the repository's Settings, Environments, create `release`.
   Add yourself as a required reviewer, and under deployment branches and tags allow only tags
   matching `v*`. The publish jobs run in this environment, and its secrets are visible only to
   them.
2. **PyPI trusted publishers.** For each of the projects `mosaica-native` and `mosaica`, open the
   project on PyPI, then Settings, Publishing, and add a GitHub publisher with owner `jennis0`,
   repository `mosaica`, workflow `release.yml` and environment `release`. No token is stored.
3. **npm.** Create the organisation `mosaicajs` on npmjs.com, which owns the `@mosaicajs` scope.
   A trusted publisher can only be added to a package that exists, so the first release publishes
   with a token:
   - Create a granular access token with read and write access to the `@mosaicajs` scope, and add
     it to the `release` environment as the secret `NPM_TOKEN`.
   - After the first release, add a trusted publisher to each of `@mosaicajs/client`,
     `@mosaicajs/deck`, `@mosaicajs/components` and `@mosaicajs/react` (package Settings, Trusted
     publishing: GitHub Actions, repository `jennis0/mosaica`, workflow `release.yml`,
     environment `release`), then delete the `NPM_TOKEN` secret and revoke the token. The workflow
     publishes through the trusted publisher when the secret is absent.
4. **crates.io.** Create an API token at crates.io (Account Settings, API Tokens) with the
   `publish-update` scope, limited to crates matching `mosaica*`, and add it to the `release`
   environment as the secret `CARGO_REGISTRY_TOKEN`. The account must own all twenty published
   crate names.
5. **GHCR.** Under Settings, Actions, General, set workflow permissions so that a workflow can be
   granted `packages: write` (the release workflow asks for it per job). After the first release,
   open the `mosaica` package under your GitHub profile's Packages, check that it is linked to
   `jennis0/mosaica`, and set its visibility to public: a new package is private.

## Cutting a release

1. **Choose the version.** While the version is below 1.0, a release that changes a format, the
   HTTP API or a client's API raises the minor version (0.1.0 to 0.2.0), and any other release
   raises the patch version.
2. **Set it everywhere.** On a branch:

   ```bash
   python3 scripts/version.py set 0.2.0
   python3 scripts/version.py check
   ```

   `set` writes the version into `Cargo.toml`, `Cargo.lock`, both `pyproject.toml` files, the
   `mosaica-native==` pin, `mosaica.__version__`, the four npm manifests and
   `package-lock.json`.
3. **Write the changelog.** Add a section `## [0.2.0] - YYYY-MM-DD` to `CHANGELOG.md` with
   today's date, describing what changed for a user, and a link line
   `[0.2.0]: https://github.com/jennis0/mosaica/releases/tag/v0.2.0` at the bottom. The section's
   body becomes the release notes. `python3 scripts/version.py notes` prints it.
4. **Refresh the licence notices** if `Cargo.lock` changed since the last release:

   ```bash
   cargo install --locked --features cli cargo-about
   python3 scripts/third-party-licenses.py
   ```

5. **Merge to `main`** and wait for the gate (`ci.yml`) to pass on the merge commit.
6. **Tag and push the tag:**

   ```bash
   git tag -a v0.2.0 -m "Mosaica 0.2.0"
   git push origin v0.2.0
   ```

7. **Approve.** The workflow verifies the tag, builds everything, then waits for approval in the
   `release` environment. Open the run in the Actions tab, check that the build jobs passed, and
   approve the waiting deployments. One approval releases them all.
8. **Check the result:** the GitHub release with its archives, `SHA256SUMS` and wheels;
   `pip install mosaica==0.2.0`; `npm view @mosaicajs/client version`;
   `cargo install mosaica --version 0.2.0`; `docker pull ghcr.io/jennis0/mosaica:0.2.0`.

## What the workflow does

| Job | What it does |
|---|---|
| `verify` | Refuses the tag unless it is `v` and the version in `Cargo.toml`, every copy of the version agrees, `CHANGELOG.md` has its section, the gate passed on the commit, `cargo deny check` passes including advisories, and `THIRD_PARTY_LICENSES` is current. |
| `wheels` | Calls `wheels.yml`: seven `mosaica-native` platform wheels, the pure `mosaica` wheel, the bare binaries, and a smoke test that installs both wheels in a clean virtualenv. |
| `archives` | One `.tar.gz` (`.zip` for Windows) per target with the binary, `LICENSE`, `THIRD_PARTY_LICENSES`, `README.md` and `CHANGELOG.md`, and `SHA256SUMS`. |
| `npm-pack` | Builds the four npm packages and packs them. |
| `pypi` | Publishes `mosaica-native`, waits until PyPI serves it, then publishes `mosaica`, through trusted publishing. |
| `npm` | Publishes the four tarballs in dependency order with provenance; a pre-release goes to the `next` dist-tag. |
| `crates` | `cargo publish --workspace`, which publishes the twenty crates in dependency order. |
| `image-build`, `image` | Builds the image natively on amd64 and arm64, pushes both by digest, then tags the pair as the version, the minor version and `latest` (not for a pre-release). |
| `github-release` | Creates the release with the changelog section as its notes, marked pre-release for an `-rc` tag. |

## When a publish fails

Fix the cause, then re-run the failed jobs from the Actions tab. `pypi` and `npm` skip what an
earlier attempt published, and pushing the image again is harmless. `crates` stops at the first
crate crates.io already has, so publish the rest by hand from the tagged commit:

```bash
cargo publish --workspace --locked --exclude mosaica-bench --exclude mosaica-docgen \
  --exclude <each crate already published>
```

A version cannot be reused once any registry has it. If a release is broken, fix it in a new patch
release; yank the broken version on crates.io (`cargo yank`) and PyPI, and deprecate it on npm
(`npm deprecate`).
