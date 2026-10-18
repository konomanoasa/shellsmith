#!/bin/sh

CDPATH='' cd -P "$(dirname "$0")/.." || exit 1

[ "$#" -le 1 ] || exit 1
case "${1-}" in
  '' | --clean) ;;
  *) exit 1 ;;
esac

rm -f target/lcov.info target/lcov.info.tmp || exit 1
[ "${1-}" != --clean ] || exit 0

cargo llvm-cov report --lcov --output-path target/lcov.info.tmp || exit 1
mv target/lcov.info.tmp target/lcov.info || exit 1
