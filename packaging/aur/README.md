# AUR: `oomtop-bin`

Arch Linux package that installs the glibc release binary (x86_64, aarch64) from the GitHub release.

```console
$ yay -S oomtop-bin          # or: paru -S oomtop-bin
```

## Maintaining

`PKGBUILD.in` is the template; `render.py` fills in the version and the checksums from the release's
`SHA256SUMS` and writes `PKGBUILD` + `.SRCINFO`:

```console
$ gh release download v0.1.0 -R iprajax/oomtop -p SHA256SUMS -D dist
$ uv run packaging/aur/render.py --version 0.1.0 --sums dist/SHA256SUMS --out dist/aur/oomtop-bin
```

`.github/workflows/publish.yml` does this on every published release and pushes to
`ssh://aur@aur.archlinux.org/oomtop-bin.git` when the `AUR_SSH_PRIVATE_KEY` secret is set.

First-time setup (once, by the maintainer):

1. Create an account at <https://aur.archlinux.org/register> and add an SSH public key under *My Account*.
2. Create the package by pushing the first rendered commit:
   ```console
   $ git clone ssh://aur@aur.archlinux.org/oomtop-bin.git && cd oomtop-bin
   $ cp ../dist/aur/oomtop-bin/{PKGBUILD,.SRCINFO} . && git add PKGBUILD .SRCINFO
   $ git commit -m "oomtop-bin 0.1.0" && git push
   ```
3. Store the private key as the repository secret `AUR_SSH_PRIVATE_KEY` for later releases.

On an Arch machine, `makepkg --printsrcinfo` and `namcap PKGBUILD` double-check the rendered files.
