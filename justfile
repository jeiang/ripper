# List available targets
list:
	@just --list --unsorted

build:
	cargo build

# Unit tests and the bwrap sandbox tests (Linux; needs unprivileged user namespaces and btrfs)
test:
	cargo test

check:
	cargo clippy --all-targets -- -D warnings
	cargo fmt --check
	# Explicit files, not bare `actionlint`: its default project-root
	# detection walks up looking for `.git`, which the `artemis` recipe
	# below deliberately excludes from the synced copy it runs this in.
	actionlint .github/workflows/*.yml

fmt:
	cargo fmt

# Bump Cargo.toml/Cargo.lock, move CHANGELOG.md's Unreleased section into
# vVERSION, commit and tag locally. Never pushes -- see AGENTS.md.
release VERSION:
	./scripts/release.sh {{VERSION}}

# Run `just <target>` on artemis in a synced copy of this worktree. The directory is per
# worktree, so parallel agents do not overwrite each other. .git is excluded: in an agent
# worktree it is a file pointing at a path on the Mac, which does not exist on artemis and
# would break flake evaluation there. `path:.` makes Nix read the synced copy as a plain
# directory instead of a git repository, so it works without .git.
artemis target="test":
	rsync -a --delete --exclude /.git --exclude /target --exclude '/result*' --exclude /.claude ./ artemis.jeiang.vpn:/tmp/ripper-{{file_name(justfile_directory())}}/
	ssh artemis.jeiang.vpn "bash -c 'cd /tmp/ripper-{{file_name(justfile_directory())}} && nix develop path:. --command just {{target}}'"
