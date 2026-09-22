# Changelog

All notable changes to `rip` are documented here.

The format follows [Keep a Changelog 1.1.0](https://keepachangelog.com/en/1.1.0/),
and `rip` uses [Semantic Versioning](https://semver.org/).

## [Unreleased]

## [1.0.0] - 2026-09-22

### Added

- Subcommands `undo`, `list`, `restore`, `empty` and `purge`, alongside
  `rip FILE...` to trash files and rm-style `-r`/`-R`/`-d`/`-f`/`-i`/`-I`/`-v`
  at the root.
- A subcommand name is recognized only as the very first word; an rm-style
  flag before it (e.g. `rip -f empty`) is refused with a hint to write
  `rip -- empty` instead.
- Correct placement across bind mounts and btrfs subvolumes: items are
  renamed into the home trash through whatever mount shows both the file
  and the trash, never assumed to share a mount just because the paths look
  like they should.
- A copy fallback (`fallback = "copy"` or `"refuse"`, `copy_threshold`) for
  files with no trash reachable by rename, configured by
  `$XDG_CONFIG_HOME/ripper/config.toml` or `--config PATH`.
- Confirmation prompts before `empty`, `purge` and a large copy fallback,
  skippable with `-y` (subcommands) or `-f` (root); without a terminal, a
  needed prompt fails instead of proceeding.
- `restore`/`purge` by original path, by a trash `files/` entry, or with no
  arguments through an fzf multi-select picker (`--all`, `--rename`).
- `empty --older-than DUR` and `--max-size SIZE` filters; every `empty` also
  removes dangling `.trashinfo` files that have no trashed item.
- Hand-written completions for fish, bash and zsh
  (`rip --completions bash|zsh|fish`), including trashed-path completion for
  `restore`/`purge`.
- A statically linked `x86_64-linux` release binary, packaged alongside the
  Nix flake package.
