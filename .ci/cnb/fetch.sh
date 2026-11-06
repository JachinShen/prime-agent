#!/usr/bin/env bash
set -euo pipefail
set +x

[[ "${GITHUB_REPOSITORY:-}" == JachinShen/prime-agent &&
   "${GITHUB_REF:-}" == refs/heads/personal-release ]] || exit 2
source_sha=${GITHUB_SOURCE_SHA:?exact GitHub source commit required}
[[ "$source_sha" =~ ^[0-9a-f]{40}$ && "$source_sha" != 0000000000000000000000000000000000000000 ]] || exit 2
[[ "${CNB_COMMIT:?controller commit required}" == "${CI_CONTROLLER_SHA:?pinned controller required}" ]] || exit 2

export GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null GIT_TERMINAL_PROMPT=0
git -c init.templateDir= init -q source
git -C source -c credential.helper= -c core.hooksPath=/dev/null \
    fetch --depth=1 https://github.com/JachinShen/prime-agent.git "$source_sha"
[[ "$(git -C source rev-parse FETCH_HEAD)" == "$source_sha" ]] || exit 3
git -C source -c core.hooksPath=/dev/null checkout -q --detach "$source_sha"
printf 'Fetched GitHub source %s\n' "$source_sha"
