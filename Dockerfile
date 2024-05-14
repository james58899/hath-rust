# syntax=docker/dockerfile:1
FROM --platform=linux/amd64 rust:alpine AS builder

WORKDIR /usr/src/myapp
RUN apk add --no-cache build-base musl-dev perl
RUN --mount=type=bind,target=. --mount=type=cache,target=/root/.cargo cargo fetch
RUN --mount=type=bind,rw,target=. --mount=type=cache,target=/root/.cargo --mount=type=cache,target=target,id=target cargo install --path .

FROM alpine
WORKDIR /hath
COPY --from=builder /usr/local/cargo/bin/hath-rust /usr/local/bin/hath-rust
ENTRYPOINT ["hath-rust"]
