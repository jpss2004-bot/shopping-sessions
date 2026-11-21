# Shopping Sessions

The default engine reads UTF-8 CSV shards directly into typed Rust records, resolves linked IDs, selects the earliest copy of each event, and writes the two daily Parquet tables. Campaign assists use merged seven-day click intervals. The image also includes a DuckDB reference implementation. Select it with `ENGINE=duckdb` or select Rust with `ENGINE=native`.

To build the native executable locally, run `cargo build --release --locked --manifest-path native/Cargo.toml` with Rust 1.85 or newer. Set `DATA_DIR`, `OUTPUT_DIR` and `NCPU` before running `native/target/release/shopping-sessions`. Use an empty output directory for each run, as the scorer does. `PROFILE=1` prints cumulative elapsed time at each stage. The default deduplicator uses a per-shard bitmap to identify IDs that occur once and an exact ID lookup to resolve every possible collision. Most event copies occupy 24 bytes. Purchases and timestamps outside the compact range use full-width records. The engine groups records by person before ordering each person’s events and clicks. To compare deduplication algorithms, set `EVENT_DEDUP` to `singletons`, `sort`, `hash`, `radix`, or `compact-hash`.

The default CSV scanner (`SHOPPING_SCAN=wide`) classifies 64-byte blocks for commas, quotes and linefeeds on Linux amd64 with OS-enabled AVX-512F/BW and CPU support for BMI1, BMI2 and POPCNT. Other machines use the existing whole-row scanner. Set `SHOPPING_SCAN=whole-intel` to select that scanner explicitly. `SHOPPING_SCAN=whole` selects the portable or AVX2 implementation for comparison. The `scalar` and `avx2` settings select the field-by-field and streaming-mask scanners. Set `PROFILE_MEMORY=1` to print buffer sizes at each repartition. `PARQUET_LAYOUT=compact` enables Parquet delta encodings for comparison with the default plain encoding.

## Local verification

Install the runtime and fixture checker with Python 3.10 or newer.

```sh
python3 -m venv .venv
. .venv/bin/activate
python -m pip install -r requirements.txt polars==2.1.0

SCRATCH_DIR="$PWD/scratch" NCPU=4 MEM_GB=6 \
  python scripts/run_fixtures.py fixtures -- python pipeline/pipeline.py

make run-dev PLATFORM=linux/amd64 CPUS=4 MEM=6g
make check-dev
```

Build the organizer's generator with Rust 1.85 or newer using `cargo build --release --locked`. Generate the public inputs with `generate --seed 1 --events 1000000 --clicks 250000 --output dev`. Extract the official dev answer into `dev-answer/`.

On Apple silicon, build the Rust crate locally for native iteration. Build the submission Dockerfile with `--platform linux/amd64` to use its pinned LLVM profile. The image’s native entry point preloads mimalloc. Set `LD_PRELOAD` to an empty string to compare the system allocator. Keep generated input, answers, and benchmark output outside Git.

To use the Rust scanner in a direct Python run, build it with Rust 1.85 or newer.

```sh
mkdir -p .local
rustc --edition=2024 -C opt-level=3 -o .local/normalize native/normalize.rs
export NORMALIZER="$PWD/.local/normalize"
```
