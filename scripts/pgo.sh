#!/usr/bin/env bash
# Run inside the pinned amd64 Rust builder. See scripts/pgo.md for Docker commands.
set -euo pipefail
export LC_ALL=C

usage() {
    echo "pgo.sh train GENERATOR_SOURCE NEW_PROFILE_DIR"
    echo "pgo.sh build PROFILE_DIR OUTPUT_BINARY"
}

if [[ ${1:-} == --help ]]; then usage; exit 0; fi
if [[ $# != 3 || ( $1 != train && $1 != build ) ]]; then usage >&2; exit 2; fi
mode=$1
engine=/src/engine
target=x86_64-unknown-linux-gnu
toolchain=1.85.1
common_flags="-Ctarget-cpu=x86-64"
generator_hash=8fafe63a66ac2db2094d5caa3c0d8b0dafa21162c6b9ebbff89e53d25371e860

die() { echo "pgo: $*" >&2; exit 1; }
[[ $(uname -s) == Linux && $(uname -m) == x86_64 ]] || die "requires linux/amd64"
export RUSTUP_TOOLCHAIN=$toolchain
version=$(rustc -vV)
for requirement in "release: $toolchain" "host: $target" "LLVM version: 19.1.7" \
    "commit-hash: 4eb161250e340c8f48f66e2b929ef4a5bed7c181"; do
    grep -Fxq "$requirement" <<<"$version" || die "expected $requirement"
done
[[ $(cargo -V) == 'cargo 1.85.1 (d73d2caf9 2024-12-31)' ]] || die "unexpected Cargo version"
[[ -f $engine/Cargo.lock ]] || die "mount native at $engine"
unset CARGO_ENCODED_RUSTFLAGS RUSTFLAGS
export CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2 RAYON_NUM_THREADS=4

source_manifest() (
    cd "$1"
    find Cargo.toml Cargo.lock src -type f -print0 | sort -z | xargs -0 sha256sum
)

# Both phases compile from the same source path using the same target, lockfile and release profile.
build_engine() {
    RUSTFLAGS="$common_flags $1" cargo build --locked --release --target "$target" \
        --manifest-path "$engine/Cargo.toml" --target-dir "$work/engine-target"
}

profile_format() {
    printf '%s\n' "$version" "target: $target" "flags: $common_flags" "format: llvm-instr-pgo-indexed-v12"
    printf 'generator: %s\n' "$generator_hash"
    printf 'pgo-script: %s\n' "$(sha256sum "$0" | cut -d' ' -f1)"
}

work=$(mktemp -d)
trap 'rm -rf -- "$work"' EXIT
binary="$work/engine-target/$target/release/shopping-sessions"

if [[ $mode == train ]]; then
    generator=$(realpath "$2")
    bundle=$(realpath -m "$3")
    [[ ! -e $bundle ]] || die "profile output must be a new directory"
    digest=$(source_manifest "$generator" | sha256sum | cut -d' ' -f1)
    [[ $digest == "$generator_hash" ]] || die "generator differs from the supplied public archive"
    rustup component add llvm-tools-preview
    profdata="$(rustc --print sysroot)/lib/rustlib/$target/bin/llvm-profdata"
    "$profdata" --version | grep -F '19.1.7' >/dev/null || die "unexpected llvm-profdata version"

    # Generate only from public seeds. Instrument the submission binary, not the generator.
    cargo build --manifest-path "$generator/Cargo.toml" --locked --release --target-dir "$work/generator-target"
    mkdir "$work/raw" "$work/bundle"
    profile_format > "$work/bundle/toolchain.txt"
    source_manifest "$engine" > "$work/bundle/source.sha256"
    build_engine "-Cprofile-generate=$work/raw"
    # Exercise the compiled defaults on all three training seeds.
    for seed in 11 37 109; do
        root="$work/seed-$seed"
        mkdir -p "$root/output" "$root/scratch"
        "$work/generator-target/release/generate" --seed "$seed" --events 120000 --clicks 30000 --output "$root/data"
        DATA_DIR="$root/data" OUTPUT_DIR="$root/output" SCRATCH_DIR="$root/scratch" NCPU=4 MEM_GB=4 \
            LLVM_PROFILE_FILE="$work/raw/seed-$seed-%m-%p.profraw" "$binary"
        compgen -G "$root/output/user_daily/*.parquet" >/dev/null || die "missing user output for $seed"
        compgen -G "$root/output/campaign_daily/*.parquet" >/dev/null || die "missing campaign output for $seed"
        compgen -G "$work/raw/seed-$seed-*.profraw" >/dev/null || die "missing counts for seed $seed"
    done
    # An optional second runner can collect counts for another runtime-selected CPU path.
    # It receives the exact instrumented binary, raw output directory and public datasets.
    if [[ -n ${PGO_EXTRA_RUNNER:-} ]]; then
        "$PGO_EXTRA_RUNNER" "$binary" "$work/raw" "$work/seed-11/data" "$work/seed-37/data" "$work/seed-109/data"
    fi
    "$profdata" merge -o "$work/merged.profdata" "$work/raw/"*.profraw
    "$profdata" show --profile-version "$work/merged.profdata" | tee "$work/summary"
    grep -Fxq 'Profile version: 12' "$work/summary" || die "expected indexed profile v12"
    grep -Eq '^Maximum function count: [1-9][0-9]*$' "$work/summary" || die "empty function counters"
    grep -Eq '^Maximum internal block count: [1-9][0-9]*$' "$work/summary" || die "empty block counters"
    cp "$work/summary" "$work/bundle/summary.txt"
    "$profdata" show --function=intel_rows_tuned --counts "$work/merged.profdata" > "$work/bundle/scanner-counts.txt"
    "$profdata" show --function=register_rows_tuned --counts "$work/merged.profdata" > "$work/bundle/wide-counts.txt"
    # Check the checkout stayed unchanged throughout training, including new Rust modules.
    source_manifest "$engine" | cmp - "$work/bundle/source.sha256"
    gzip -nc "$work/merged.profdata" > "$work/bundle/merged.profdata.gz"
    (cd "$work/bundle" && sha256sum merged.profdata.gz > profile.sha256)
    mkdir "$bundle"
    cp "$work/bundle/"* "$bundle/"
    du -h "$bundle/merged.profdata.gz"
else
    bundle=$(realpath "$2")
    output=$(realpath -m "$3")
    profile_format | cmp - "$bundle/toolchain.txt" || die "profile/toolchain settings have changed"
    source_manifest "$engine" | cmp - "$bundle/source.sha256" || die "retrain for this source and lockfile"
    (cd "$bundle" && sha256sum -c profile.sha256)
    gzip -dc "$bundle/merged.profdata.gz" > "$work/merged.profdata"
    build_engine "-Cprofile-use=$work/merged.profdata -Cllvm-args=-pgo-warn-missing-function"
    install -m 755 "$binary" "$output"
fi
