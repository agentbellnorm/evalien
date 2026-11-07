# Build: Rust, plus wasi-sdk to compile QuickJS for the sandbox guest.
FROM docker.io/library/rust:1.99-bookworm AS build
ARG WASI_SDK_VERSION=34
RUN arch="$(uname -m | sed 's/aarch64/arm64/')" \
 && curl -fsSL "https://github.com/WebAssembly/wasi-sdk/releases/download/wasi-sdk-${WASI_SDK_VERSION}/wasi-sdk-${WASI_SDK_VERSION}.0-${arch}-linux.tar.gz" \
    | tar xz -C /opt \
 && mv /opt/wasi-sdk-* /opt/wasi-sdk
ENV WASI_SDK=/opt/wasi-sdk
WORKDIR /src
COPY . .
RUN cargo build --release --locked --bins && mkdir /data

# Run: no shell, no package manager, not root.
FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /src/target/release/evalien /src/target/release/report /usr/local/bin/
COPY --from=build --chown=nonroot:nonroot /data /data
ENV AGENT_DB_PATH=/data/agent.db
ENTRYPOINT ["evalien"]
