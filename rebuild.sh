#!/bin/bash

if [[ -x bin/depree ]] ; then
    if [[ "$(bin/depree version)" == "$(git rev-parse HEAD)" ]] ; then
	echo "No build needed"
	exit 0
    fi
fi

cargo build --release
mkdir -p bin/
mv target/release/depree bin
git clean -xdf target/
