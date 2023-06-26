# syntax=docker/dockerfile:1
FROM --platform=linux/amd64 rust:bookworm AS builder
ARG LLVM_VERSION=16
ENV CC=clang-${LLVM_VERSION} CXX=clang-${LLVM_VERSION} CFLAGS="-flto -fuse-ld=lld-${LLVM_VERSION}" CXXFLAGS="-flto -fuse-ld=lld-${LLVM_VERSION}" RUSTFLAGS="-Clinker-plugin-lto -Clinker=clang-${LLVM_VERSION} -Clink-arg=-fuse-ld=lld-${LLVM_VERSION}"
WORKDIR /usr/src/myapp
RUN echo "deb http://apt.llvm.org/bookworm/ llvm-toolchain-bookworm-${LLVM_VERSION} main" > /etc/apt/sources.list.d/llvm.list && \
    wget -qO- https://apt.llvm.org/llvm-snapshot.gpg.key | tee /etc/apt/trusted.gpg.d/apt.llvm.org.asc && apt-get update && \
    apt-get install -y eatmydata && eatmydata apt-get install -y crossbuild-essential-arm64 clang-$LLVM_VERSION lldb-$LLVM_VERSION lld-$LLVM_VERSION clangd-$LLVM_VERSION && \
    rm -rf /var/lib/apt/lists/*
RUN rustup target add aarch64-unknown-linux-gnu
COPY . .
RUN --mount=type=cache,target=/root/.cargo cargo fetch
ARG TARGETARCH
RUN --mount=type=cache,target=/root/.cargo \
    if [ "$TARGETARCH" = "arm64" ] ; then \
        RUSTFLAGS="$RUSTFLAGS -Clink-arg=--target=aarch64-unknown-linux-gnu" cargo install --target=aarch64-unknown-linux-gnu --path . ; \
    else \
        cargo install --path . ; \
    fi

FROM debian:bookworm-slim
COPY --from=builder /usr/local/cargo/bin/hath-rust /usr/local/bin/hath-rust
CMD ["hath-rust"]