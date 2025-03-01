#!/bin/bash

# Exits as soon as any line fails.
set -euo pipefail

source ci/scripts/common.env.sh

while getopts 'p:' opt; do
    case ${opt} in
        p )
            profile=$OPTARG
            ;;
        \? )
            echo "Invalid Option: -$OPTARG" 1>&2
            exit 1
            ;;
        : )
            echo "Invalid option: $OPTARG requires an arguemnt" 1>&2
            ;;
    esac
done
shift $((OPTIND -1))

echo "--- Download artifacts"
mkdir -p target/debug
buildkite-agent artifact download risingwave-"$profile" target/debug/
buildkite-agent artifact download risedev-playground-"$profile" target/debug/
mv target/debug/risingwave-"$profile" target/debug/risingwave
mv target/debug/risedev-playground-"$profile" target/debug/risedev-playground

echo "--- Adjust permission"
chmod +x ./target/debug/risingwave
chmod +x ./target/debug/risedev-playground

echo "--- Generate RiseDev CI config"
cp risedev-components.ci.env risedev-components.user.env

echo "--- Prepare RiseDev playground"
~/cargo-make/makers pre-start-playground
~/cargo-make/makers link-all-in-one-binaries

echo "--- e2e, ci-3cn-1fe, streaming"
~/cargo-make/makers ci-start default
timeout 5m sqllogictest -p 4566 -d dev './e2e_test/streaming/**/*.slt' -j 8

echo "--- Kill cluster"
~/cargo-make/makers ci-kill

echo "--- e2e, ci-3cn-1fe, delta join"
~/cargo-make/makers ci-start default
timeout 3m sqllogictest -p 4566 -d dev './e2e_test/streaming_delta_join/**/*.slt'

echo "--- Kill cluster"
~/cargo-make/makers ci-kill

echo "--- e2e, ci-3cn-1fe, batch distributed"
~/cargo-make/makers ci-start default
timeout 2m sqllogictest -p 4566 -d dev './e2e_test/ddl/**/*.slt'
timeout 2m sqllogictest -p 4566 -d dev './e2e_test/batch/**/*.slt' -j 8

echo "--- Kill cluster"
~/cargo-make/makers ci-kill
