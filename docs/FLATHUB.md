# Submitting to Flathub

Everything needed is already in the repo. This is the sequence, and the parts
that only the maintainer can do.

Check <https://docs.flathub.org/docs/for-app-authors/submission> before
starting - the requirements do move, and this file is a snapshot.

## What is already here

| Path | |
| --- | --- |
| `packaging/flatpak/org.nanotorrent.NanoTorrent.yml` | the manifest |
| `packaging/flatpak/build.sh` | builds and installs it locally |
| `packaging/linux/org.nanotorrent.NanoTorrent.metainfo.xml` | AppStream data for the store page |
| `packaging/linux/org.nanotorrent.NanoTorrent.desktop` | desktop entry |
| `docs/screenshots/*.png` | the images the metainfo points at |

The app id is `org.nanotorrent.NanoTorrent`, matching nanotorrent.org. Flathub
requires the id to be a domain the submitter controls, and may ask for proof -
usually a DNS TXT record or a file under `/.well-known/` on that domain.

## 1. Tag and push the release

The screenshot URLs in the metainfo point at `master` on GitHub, and Flathub
fetches them during review, so they have to resolve before the PR is opened.

```sh
git tag -a v0.2.2 -m "NanoTorrent 0.2.2"
git push origin master --tags
git rev-parse v0.2.2^{commit}      # the sha the manifest needs
```

## 2. Point the manifest at the tag

Flathub builds from a tagged revision, never from a working directory. In the
manifest, replace the `type: dir` source with:

```yaml
      - type: git
        url: https://github.com/Power2All/nanotorrent.git
        tag: v0.2.2
        commit: <sha from step 1>
```

The `dir` source is marked with a comment saying exactly this.

## 3. Regenerate the vendored crate list

```sh
packaging/flatpak/build.sh      # rewrites cargo-sources.json when Cargo.lock is newer
```

`cargo-sources.json` is gitignored here because it is generated, but the
**Flathub repo needs a committed copy** - it has no `Cargo.lock` to generate one
from. Copy it across in the next step.

## 4. Open the pull request

1. Fork <https://github.com/flathub/flathub>.
2. Branch from **`new-pr`** - not `master`. This is the one step people get
   wrong; a PR against `master` is closed unread.
3. Add two files at the repository root:
   - `org.nanotorrent.NanoTorrent.yml` (the manifest, with the git source)
   - `cargo-sources.json`
4. Open the PR against the `new-pr` branch.

A bot builds the submission and reports back. Reviewers then look at it by
hand; expect questions about permissions.

## 5. Expect to justify the permissions

The `finish-args` are already as narrow as the app can work with, and the
manifest explains each one. The two that draw questions:

- `--filesystem=xdg-download` - a torrent client has to write somewhere.
  Anything outside `~/Downloads` goes through the file portal instead. Do not
  widen this to `--filesystem=home` to make something convenient work; that is
  what gets a submission held up.
- `--share=network` - self-evident for a BitTorrent client.

After the PR is merged Flathub creates `flathub/org.nanotorrent.NanoTorrent`,
and that repo - not this one - is where future updates are published. Each new
release is a PR there bumping `tag`, `commit` and `cargo-sources.json`.

## Runtime version

The manifest uses freedesktop **25.08**, because the `rust-stable` extension
for 24.08 ships rustc 1.89 and Slint 1.17 needs 1.92. 25.08 carries 1.98.

If Flathub ever asks for an older runtime, the options are poor: pinning Slint
back far enough to build on 1.89 would undo UI work, and shipping a rustup
toolchain in the manifest is discouraged because it fetches a compiler at build
time. Raise it in the PR rather than working around it.
