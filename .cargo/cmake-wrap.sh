#!/usr/bin/env bash
# cmake wrapper: strips --config <value> (unsupported on Linux Makefile generators)
# and injects the minimum policy version for configure invocations.
cmake_args=()
prev_config=false
for arg in "$@"; do
    if [ "$prev_config" = "true" ]; then
        prev_config=false
        continue
    fi
    if [ "$arg" = "--config" ]; then
        prev_config=true
        continue
    fi
    cmake_args+=("$arg")
done

# First positional arg is --build for the build phase; absence means configure phase.
if [ "${cmake_args[0]}" != "--build" ]; then
    cmake_args+=("-DCMAKE_POLICY_VERSION_MINIMUM=3.5")
fi

exec cmake "${cmake_args[@]}"
