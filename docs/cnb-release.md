# GitHub → CNB release builds

GitHub `personal-release` pushes start a short Actions dispatcher. It sends the
exact GitHub source SHA to the pinned controller in `jachinshen/github-ci`.
The CNB observer starts native Linux x86_64 and ARM64 builds, verifies their
final results, and updates the GitHub commit status. Actions finishes after
dispatch; `cnb/release/<run-id>/<attempt>` records the compilation result.

Application source and the canonical CI definitions live in this GitHub
repository. The CNB controller contains the deployed CI configuration,
dispatcher, fetch script, and build-image definition.

## Build and artifact contract

Workers fetch the public GitHub repository at the requested commit and check
its identity before executing `.ci/cnb/release.sh`. They use Rust 1.98.1 in
Ubuntu 22.04, matching the upstream release compiler and GLIBC 2.35 baseline.
Cargo downloads, the build image, and each architecture's target directory
are cached. Both architectures use the same pinned catalog as the Nix package.

Existing release scripts split debug information, assemble the executable
with its Python runtime sources, skills and catalog, verify the extracted
executable and hashes, and prove the decoder belongs to that executable.
The runtime version includes the full source commit.

Each architecture uploads a bundle named
`prime-agent-<version>-<source-sha>-linux-<x64|arm64>-artifacts.tar.gz` and its
SHA-256 file to the **controller commit's CNB attachments**. The source SHA
keeps different source builds from overwriting one another. Each bundle has:

- The installable release `.tar.gz` and separate `.debug.gz` decoder.
- Platform-specific `SHA256SUMS` and `manifest.json` files.
- Build provenance: source commit, controller commit, compiler, catalog and log URL.

Archives carry runtime sources. The [Nix package](nix.md) provides Python 3.14
and the kernel dependencies when using the flake.

Only a successful worker result with both named pipelines and every required
fetch, build/verification and upload stage may publish a success status.
Failures, cancellation, incomplete or unknown results, API errors and timeouts
fail explicitly. Each Actions retry has a separate status context. Cancelling
the Actions dispatcher does not cancel builds already accepted by CNB.

## Controller deployment

In a checkout of `jachinshen/github-ci` on `main`, copy these trusted files:

```sh
mkdir -p prime-agent
cp <prime-agent-checkout>/.ci/cnb/{Dockerfile,bridge.py,fetch.sh,pipeline.yml} prime-agent/
```

Add `prime-agent/pipeline.yml` to the controller root `.cnb.yml`'s `include`
list, preserving its existing events. Validate the fragment and root config
against the CNB schema, commit and push. Set the GitHub repository variable
`CNB_CI_CONFIG_SHA` to that full, immutable **controller** commit SHA.
The dispatched GitHub source SHA remains a separate parameter.

## Credentials

Configure these two credentials before enabling the first successful run:

1. GitHub Actions secret `CNB_CI_TOKEN` in `JachinShen/prime-agent`: a CNB token
   scoped to `jachinshen/github-ci` with `repo-cnb-trigger:rw`.
2. CNB KeyStore file `jachinshen/github-ci-secrets/prime-agent-status.yml`:
   use [the scope template](../.ci/cnb/status.example.yml), filling
   `PRIME_AGENT_STATUS_TOKEN` with a GitHub fine-grained PAT that selects
   `JachinShen/prime-agent` and permits **Commit statuses: Read and write**.
   Set an expiration date. This credential is imported only by the observer.

The source repository is public, so fetching it requires no GitHub read token.
The dispatcher uses its temporary GitHub job token only to write the initial
status. The observer's persistent status credential stays in its own pipeline
and is excluded from worker requests. Workers execute trusted personal-branch
source; their CNB upload token is scoped to the CI infrastructure repository.
Trigger tokens authorize the controller repository, including the platform's
custom-config API; they are not restricted to a single event.

After configuring credentials, push `personal-release` or run **Dispatch CNB
release builds** manually on that branch. Follow its CNB link and the separate
commit status for the final result. A successful dispatcher means CNB accepted
the request. A failed credential import or interrupted observer can leave the
status pending; inspect its log before rerunning.

Native macOS release builds use the local flake or a separately configured
macOS runner. Version tags, GitHub Release publication and a shared Nix binary
cache have their own publication configuration.

Local checks:

```sh
python3 -I .ci/cnb/test_bridge.py
shellcheck .ci/cnb/{fetch,release}.sh
```
