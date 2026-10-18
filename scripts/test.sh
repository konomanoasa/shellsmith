#!/bin/sh

CDPATH='' cd -P "$(dirname "$0")/.." || exit 1

./scripts/coverage.sh --clean || exit 1
cargo llvm-cov --tests --no-fail-fast --no-report || exit 1
./scripts/coverage.sh || exit 1
