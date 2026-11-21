# Linux amd64 PGO

Train once for a fixed source revision using the supplied public dataset generator. The three training seeds are 11, 37 and 109, each with 120,000 events and 30,000 clicks. Generated CSVs, output tables and raw profiles live in the temporary training container. The retained bundle holds LLVM instrumentation counts, the source and profile checksums, and the compiler settings.

From the repository root, unpack the supplied generator into `.local/generator` and run the following on a scheduled build machine. The generator directory must contain its Cargo manifest, lockfile and source. The script checks their digest against the organizer archive. On a Linux amd64 machine with OS-enabled AVX-512F/BW and BMI1/BMI2/POPCNT, training exercises the wide scanner. An AVX2 machine exercises the whole-row fallback.

```sh
mkdir -p .local/pgo
builder='rust:1.85.1-slim-bookworm@sha256:3490aa77d179a59d67e94239cca96dd84030b564470859200f535b942bdffedf'
docker run --rm --platform linux/amd64 --cpus 4 --memory 4g \
  -v "$PWD/native:/src/engine:ro" -v "$PWD/scripts/pgo.sh:/pgo.sh:ro" \
  -v "$PWD/.local/generator:/generator:ro" -v "$PWD/.local/pgo:/profiles" \
  "$builder" bash /pgo.sh train /generator /profiles/trial
```

The `trial` directory must be new. To compile a candidate binary for an amd64 container test, mount the same source and completed bundle.

```sh
docker run --rm --platform linux/amd64 --cpus 4 --memory 4g \
  -v "$PWD/native:/src/engine:ro" -v "$PWD/scripts/pgo.sh:/pgo.sh:ro" \
  -v "$PWD/.local/pgo:/profiles" "$builder" \
  bash /pgo.sh build /profiles/trial /profiles/shopping-sessions
```

The builder pins rustc 1.85.1, LLVM 19.1.7 (raw profile v10, indexed v12), the amd64 bookworm image manifest, Cargo.lock and the `x86_64-unknown-linux-gnu` target. Both phases use release mode and `-Ctarget-cpu=x86-64`. Generation and profile use are the two phases of instrumentation PGO. Distinct seeds get separate raw profiles, which the matching Rust toolchain’s llvm-profdata merges. Runtime thread scheduling can change the exact counts across training runs. [Rust PGO workflow](https://doc.rust-lang.org/rustc/profile-guided-optimization.html)

The compiler runs two build jobs. Training uses the three generated datasets on the training CPU. The optional `PGO_EXTRA_RUNNER` environment variable names an executable that also trains on another CPU. The script passes it the identical instrumented engine, a writable raw-profile directory and the three dataset directories in seed order. The runner executes the engine with the requested dataset and a distinct `LLVM_PROFILE_FILE` in the raw directory for each seed. The script merges counts from both environments.

The included profile covers the AVX2 whole-row fallback and the AVX-512F/BW scanner with BMI1/BMI2/POPCNT row decoding. Each path processed seeds 11, 37 and 109. A Bochs Skylake-X Linux guest ran the wide path, and the AVX2 runs used the Docker Desktop amd64 environment. The local training bundle’s `summary.txt`, `scanner-counts.txt` and `wide-counts.txt` record the merged counters.

## Scorer build

The root Dockerfile compiles with the trained profile during each image build. The build checks the compiler version, trainer digest, manifest of every Rust source file, and compressed profile checksum before compilation. The final image copies the resulting binary from the builder.

The tracked bundle under `profiles/amd64/` contains the four files used by the build. The source manifest includes the wide scanner assembly and its Rust caller. Retrain after changing the engine source, dependency lockfile or PGO script.
