#!/usr/bin/env bash
# Exercises the documented Linux x86_64 released-binary first-success path.
set -euo pipefail

: "${TOGI_VERSION:?TOGI_VERSION is required}"
: "${GITHUB_WORKSPACE:?GITHUB_WORKSPACE is required}"
: "${GITHUB_PATH:?GITHUB_PATH is required}"
: "${RUNNER_TEMP:?RUNNER_TEMP is required}"

case "$(uname -s)" in
  Linux) ;;
  *)
    echo "Released-binary smoke requires Linux, got $(uname -s)" >&2
    exit 1
    ;;
esac

if command -v togi >/dev/null 2>&1; then
  echo "Released-binary smoke requires no preinstalled togi: $(command -v togi)" >&2
  exit 1
fi

binding=$(bash "${GITHUB_WORKSPACE}/.github/scripts/assert-release-target.sh")
eval "$binding"

fetched=$(TOGI_VERSION="$TOGI_VERSION" TOGI_ARCHIVE="$TOGI_ARCHIVE" \
  bash "${GITHUB_WORKSPACE}/.github/scripts/fetch-togi-release-asset.sh")
eval "$fetched"

TOGI_ARCHIVE="$TOGI_ARCHIVE" TOGI_BINARY="$TOGI_BINARY" TOGI_ARCHIVE_PATH="$TOGI_ARCHIVE_PATH" \
  bash "${GITHUB_WORKSPACE}/.github/scripts/install-togi-archive.sh"
install_dir="${RUNNER_TEMP}/togi-bin"
export PATH="${install_dir}:${PATH}"
if [ "$(command -v togi)" != "${install_dir}/togi" ]; then
  echo "Released-binary smoke did not select the installed togi" >&2
  exit 1
fi
version_output=$(togi --version)
printf '%s\n' "$version_output"
if [ "$version_output" != "togi ${TOGI_VERSION#v}" ]; then
  echo "Unexpected togi version: ${version_output}" >&2
  exit 1
fi

fixture_dir="${GITHUB_WORKSPACE}/tests/fixtures/go"
project_root=$(mktemp -d "${RUNNER_TEMP}/togi-released-binary-fixture.XXXXXX")
trap 'rm -rf "$project_root"' EXIT
cp -R "$fixture_dir/." "$project_root/"
git -C "$project_root" init -q
git -C "$project_root" config user.name "Togi Released Binary Smoke"
git -C "$project_root" config user.email "togi-smoke@example.invalid"
sed -i 's/if n > 0/if n < 0/' "$project_root/calc.go"
git -C "$project_root" add -- .
git -C "$project_root" commit -qm "Seed fixture"
sed -i 's/if n < 0/if n > 0/' "$project_root/calc.go"
git -C "$project_root" add calc.go
git -C "$project_root" commit -qm "Restore positive comparison"
git -C "$project_root" rev-parse --verify HEAD~1 >/dev/null

export GOFLAGS=-count=1
export GOCACHE="${RUNNER_TEMP}/togi-released-binary-go-cache"
rm -rf "$GOCACHE"
go -C "$project_root" test ./...

run_output="${RUNNER_TEMP}/togi-released-binary-smoke-output.txt"
set +e
(
  cd "$project_root"
  togi check --base HEAD~1
) >"$run_output" 2>&1
status=$?
set -e

case "$status" in
  1)
    ;;
  0)
    echo "Expected the weak fixture to produce a surviving mutant" >&2
    cat "$run_output" >&2
    exit 1
    ;;
  *)
    echo "Released-binary mutation run failed with exit code ${status}" >&2
    cat "$run_output" >&2
    exit "$status"
    ;;
esac

if ! grep -Eq '^Results: [0-9]+ killed, [1-9][0-9]* survived, 0 timeout, 0 build errors$' "$run_output"; then
  echo "Released-binary mutation run did not prove healthy survivor results" >&2
  cat "$run_output" >&2
  exit 1
fi
if ! grep -Eq '^Execution: [1-9][0-9]* freshly tested$' "$run_output"; then
  echo "Released-binary mutation run did not prove fresh execution" >&2
  cat "$run_output" >&2
  exit 1
fi
if grep -q '^Partial:' "$run_output"; then
  echo "Released-binary mutation run was partial" >&2
  cat "$run_output" >&2
  exit 1
fi
git -C "$project_root" diff --exit-code

# Prove the full repair loop with the installed archive, without a source build.
TOGI_BIN="${install_dir}/togi" bash "${GITHUB_WORKSPACE}/examples/demo.sh"
