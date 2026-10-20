#!/bin/sh
# vendor/<name> is a squashed git subtree of a fork; this moves code between the two.
#
#   vendor/subtree.sh pull rust-analyzer [ref]   take the fork's branch (or ref) into vendor/
#   vendor/subtree.sh push rust-analyzer         send ra-jit's commits under vendor/ to the fork
#
# Run from anywhere in the repository, with a clean tree.
set -eu

usage() {
	echo "usage: vendor/subtree.sh pull|push rust-analyzer|rustc_abi [ref]" >&2
	exit 2
}

[ $# -ge 2 ] || usage
op=$1
name=$2

case $name in
rust-analyzer) url=https://github.com/indexable-inc/rust-analyzer branch=ra-jit ;;
rustc_abi) url=https://github.com/indexable-inc/rustc_abi branch=main ;;
*) usage ;;
esac
ref=${3:-$branch}

cd "$(git rev-parse --show-toplevel)"
case $op in
pull) exec git subtree pull --prefix "vendor/$name" "$url" "$ref" --squash ;;
push) exec git subtree push --prefix "vendor/$name" "$url" "$ref" ;;
*) usage ;;
esac
