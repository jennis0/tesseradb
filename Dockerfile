# syntax=docker/dockerfile:1
#
# The `tessera` binary on a distroless glibc base. The server tunes glibc's malloc, so the runtime
# is glibc rather than musl. See docker/README.md for how to run it.
#
#   docker build --build-arg TESSERA_BUILD_COMMIT=$(git rev-parse HEAD) -t tessera .

ARG RUST_VERSION=1
ARG DEBIAN=bookworm

FROM rust:${RUST_VERSION}-${DEBIAN} AS build
WORKDIR /src
ARG TESSERA_BUILD_COMMIT=unknown
ENV TESSERA_BUILD_COMMIT=${TESSERA_BUILD_COMMIT} \
    CARGO_PROFILE_RELEASE_STRIP=symbols
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates crates
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,target=/src/target,sharing=locked \
    cargo build --release --locked -p tessera-cli \
    && install -D target/release/tessera /out/usr/local/bin/tessera \
    && install -d -o 65532 -g 65532 /out/var/lib/tessera /out/etc/tessera

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /out/ /
WORKDIR /etc/tessera
VOLUME ["/var/lib/tessera"]
EXPOSE 8080 8081 8082
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/tessera"]
CMD ["serve"]
