FROM rust:1.85.1-slim-bookworm@sha256:3490aa77d179a59d67e94239cca96dd84030b564470859200f535b942bdffedf AS normalizer
COPY native/normalize.rs /src/normalize.rs
RUN rustc --edition=2024 -C opt-level=3 -o /normalize /src/normalize.rs

WORKDIR /src/engine
COPY native/Cargo.toml native/Cargo.lock ./
COPY native/src/ src/
# Compare both writer schedules in the same uninstrumented release build.
RUN --mount=type=cache,target=/usr/local/cargo/registry --mount=type=cache,target=/build \
    CARGO_BUILD_JOBS=4 RUSTFLAGS="-Ctarget-cpu=x86-64" cargo build --locked \
      --release --target x86_64-unknown-linux-gnu --target-dir /build \
    && cp /build/x86_64-unknown-linux-gnu/release/shopping-sessions /shopping-sessions

FROM python:3.12-slim-bookworm

RUN apt-get update \
    && apt-get install -y --no-install-recommends libmimalloc2.0=2.0.9+ds-2 \
    && rm -rf /var/lib/apt/lists/*

# The scorer runs the container as uid 1000; give that uid a name and a home.
RUN useradd --uid 1000 --no-create-home --home-dir /scratch app

COPY requirements.txt /app/requirements.txt
RUN pip install --no-cache-dir -r /app/requirements.txt

COPY pipeline/ /app/pipeline/
COPY --from=normalizer /normalize /app/normalize
COPY --from=normalizer /shopping-sessions /app/shopping-sessions
COPY run.sh /app/run.sh
RUN chmod +x /app/run.sh

# At run time the root filesystem is read-only, the user is 1000:1000, there is
# no network, /data is read-only, and /output and /scratch are the only writable
# places. run.sh points every temp and cache directory at /scratch.
ENV PYTHONDONTWRITEBYTECODE=1 \
    PYTHONUNBUFFERED=1 \
    NORMALIZER=/app/normalize

WORKDIR /app
CMD ["/app/run.sh"]
