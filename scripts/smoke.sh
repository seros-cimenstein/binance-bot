#!/usr/bin/env bash
set -euo pipefail

for script in scripts/*.sh; do
    bash -n "$script"
done

echo "Shell smoke checks passed."
