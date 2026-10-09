# syntax=docker/dockerfile:1
#
# The `mosaica` binary on a distroless glibc base. The server tunes glibc's malloc, so the runtime
# is glibc. See docker/README.md for how to run it.
#
# The compiler is the one in the rust image named by RUST_VERSION; rust-toolchain.toml is left out
# of the context so rustup does not fetch another.
#
#   docker build --build-arg MOSAICA_BUILD_COMMIT=$(git rev-parse HEAD) -t mosaica .

ARG RUST_VERSION=1
ARG DEBIAN=bookworm

FROM rust:${RUST_VERSION}-${DEBIAN} AS build
WORKDIR /src
ARG MOSAICA_BUILD_COMMIT=unknown
ENV MOSAICA_BUILD_COMMIT=${MOSAICA_BUILD_COMMIT} \
    CARGO_PROFILE_RELEASE_STRIP=symbols
COPY Cargo.toml Cargo.lock ./
COPY crates crates
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,target=/src/target,sharing=locked \
    cargo build --release --locked -p mosaica-cli \
    && install -D target/release/mosaica /out/usr/local/bin/mosaica \
    && install -d -o 65532 -g 65532 /out/var/lib/mosaica /out/etc/mosaica

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /out/ /
WORKDIR /etc/mosaica
VOLUME ["/var/lib/mosaica"]
EXPOSE 8080 8081 8082
USER 65532:65532
# A large bundle can take minutes to open; failures in the start period are not counted.
HEALTHCHECK --interval=15s --timeout=5s --start-period=10m --start-interval=2s \
    CMD ["/usr/local/bin/mosaica", "health"]
ENTRYPOINT ["/usr/local/bin/mosaica"]
CMD ["serve"]
