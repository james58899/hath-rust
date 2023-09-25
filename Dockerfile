# syntax=docker/dockerfile:1
FROM --platform=linux/amd64 rust:bookworm AS builder

ARG LLVM_VERSION=17
ENV CC=clang-${LLVM_VERSION} CXX=clang-${LLVM_VERSION} CFLAGS="-flto -fuse-ld=lld-${LLVM_VERSION}" CXXFLAGS="-flto -fuse-ld=lld-${LLVM_VERSION}"

WORKDIR /usr/src/myapp
RUN echo "deb http://apt.llvm.org/bookworm/ llvm-toolchain-bookworm-${LLVM_VERSION} main" > /etc/apt/sources.list.d/llvm.list && \
    wget -qO- https://apt.llvm.org/llvm-snapshot.gpg.key | tee /etc/apt/trusted.gpg.d/apt.llvm.org.asc && apt-get update && \
    apt-get install -y eatmydata && eatmydata apt-get install -y crossbuild-essential-arm64 clang-$LLVM_VERSION lldb-$LLVM_VERSION lld-$LLVM_VERSION clangd-$LLVM_VERSION && \
    rm -rf /var/lib/apt/lists/*
RUN rustup toolchain install nightly && rustup target add --toolchain nightly aarch64-unknown-linux-gnu
RUN --mount=type=bind,target=. --mount=type=cache,target=/root/.cargo cargo fetch
ARG TARGETARCH
RUN --mount=type=bind,rw,target=. --mount=type=cache,target=/root/.cargo --mount=type=cache,target=target,id=target-$TARGETARCH \
    if [ "$TARGETARCH" = "arm64" ] ; then \
        CARGO_HOST_LINKER=clang-${LLVM_VERSION} CARGO_HOST_RUSTFLAGS="-Clink-arg=-fuse-ld=lld-${LLVM_VERSION}" \
        CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUSTFLAGS="-Clinker-plugin-lto -Clinker=clang-${LLVM_VERSION} -Clink-arg=-fuse-ld=lld-${LLVM_VERSION} -Clink-arg=--target=aarch64-unknown-linux-gnu" \
        cargo +nightly -Ztarget-applies-to-host -Zhost-config install --target=aarch64-unknown-linux-gnu --path . ; \
    else \
        RUSTFLAGS="-Clinker-plugin-lto -Clinker=clang-${LLVM_VERSION} -Clink-arg=-fuse-ld=lld-${LLVM_VERSION}" cargo install --path . ; \
    fi

FROM debian:bookworm-slim
WORKDIR /hath
COPY --from=builder /usr/local/cargo/bin/hath-rust /usr/local/bin/hath-rust
CMD ["hath-rust"]