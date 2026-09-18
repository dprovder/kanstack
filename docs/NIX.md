# Installing and developing with Nix

This flake builds `kanstack` with [crane](https://github.com/ipetkov/crane) and
gives a `but`/`tmux`/`cmux`-equipped dev shell for working on it. It is one of
several install channels; the others (`brew`, prebuilt binaries, `cargo
install`) are described in the [README](#install).

## What you get

| command | gives you |
|---|---|
| `nix run .#` | `kanstack` with `but`, `tmux`, `git` (+ `cmux` on Apple Silicon) — the default package, deps bundled |
| `nix build .#` | the same bundled package |
| `nix profile install .#` | kanstack + its runtime deps, installed together |
| `nix build .#kanstack-bin` | the raw crane binary alone, no deps |
| `nix run .#but -- --version` | the `but` CLI it drives, standalone |
| `nix develop` | rust toolchain + `rust-analyzer` + `but` + `tmux` + `git` (+ `cmux`, Apple Silicon) |

`packages.default` is a wrapper that prepends the bundled dependency `bin` dirs
to kanstack's PATH, so the pinned `but` arrives with the package instead of
being left to the user. `packages.kanstack-bin` is that wrapper's unwrapped
parent if you ever want the bare binary.

### Using it as a flake input

Install the bundled package declaratively on NixOS / home-manager:

```nix
kanstack.url = "github:dprovder/kanstack";

environment.systemPackages = [ kanstack.packages.${system}.default ];
```

No separate `but`/`tmux`/`git` installs — Nix resolves the pinned versions.
(They do land on the system PATH, like any `systemPackages` entry.)

## Provisioning `but`, per device

kanstack refuses to run below `but` 0.22 (and notes versions newer than it was
verified against). nixpkgs' own `gitbutler` package is unusable for this: it is
0.19.x and its build strips the `but` binary entirely. This flake therefore
gets a 0.22+ `but` two ways, all in `flake.nix`, no global installs:

| system | `but` source | why |
|---|---|---|
| `x86_64-linux` | prebuilt, from GitButler's official CDN | raw `but` binaries are published for Linux |
| `aarch64-linux` | prebuilt, from the same CDN | same |
| `aarch64-darwin` | built from source (`release/0.22.3`) | macOS only ships a self-extracting `but-installer`, not a fetchable binary |
| `x86_64-darwin` | unsupported on nixpkgs-unstable | 26.11 dropped Intel macs; see below |

The Linux binaries are pinned (`butRelease = "0.22.3-3234"` in `flake.nix`,
with fixed SRI hashes). `cmux` is macOS-only software and kanstack falls back to
`tmux` without it, so the Linux dev shell simply does not include it.

The Apple Silicon build is slow the first time — it compiles `but` from
source (~800 crates plus OpenSSL and libcurl) and there is no
cache.nixos.org prebuild for this path, only for the Linux binaries.
Subsequent builds reuse the Nix store, so only the first `nix run` pays
for it; expect several minutes.

### Updating `but`

kanstack is verified through 0.22.3 (`src/but/mod.rs`), so this is currently
the exact pinned release. To bump it later:

1. Find the newest release's CDN path: `curl -s https://gitbutler.com/install.sh | grep installers/info`, or check
   [GitButler releases](https://github.com/gitbutlerapp/gitbutler/releases) for the `N-NNNN` build number.
2. **Linux:** update `butRelease` (e.g. `0.23.0-3300`) and refresh each hash:
   ```sh
   nix-prefetch-url https://releases.gitbutler.com/releases/release/<rel>/linux/x86_64/but
   nix-prefetch-url https://releases.gitbutler.com/releases/release/<rel>/linux/aarch64/but
   ```
   Paste the SRI output into `butSources`. The check stays: don't ship a `but`
   below 0.22.
3. **Apple Silicon:** update the `gitbutler-src` input ref to the matching tag:
   ```sh
   nix flake lock --update-input gitbutler-src
   ```

## Intel Macs

nixpkgs-unstable dropped `x86_64-darwin` entirely, so this flake cannot
evaluate there. Options:

- pin the flake's nixpkgs to the `nixpkgs-26.05-darwin` branch,
- or install `but` via GitButler's own installer and use a non-Nix
  `kanstack` (brew / prebuilt binaries) instead.

## Reference

The Linux `but` fetch-and-patchelf derivation in `flake.nix` is modelled on
[kmdtaufik/nix4gitbutler](https://github.com/kmdtaufik/nix4gitbutler), which
serves the same prebuilt Linux binaries. It is kept in-tree (and in one file)
because that flake only supports `x86_64-linux`; this one also needs
`aarch64-linux` and a source-built macOS path.