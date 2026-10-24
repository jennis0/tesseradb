# syntax=docker/dockerfile:1
#
# The `mosaica` binary on a distroless glibc base. The server tunes glibc's malloc, so the runtime
# is glibc. See docker/README.md for how to run it.
#
# The compiler is the one in the rust image below; rust-toolchain.toml is left out of the context
# so rustup does not fetch another. Both base images are pinned by digest, so a rebuild of the same
# source uses the same compiler and runtime libraries. To move to newer ones, read the current
# digests with `docker buildx imagetools inspect rust:1-bookworm` and
# `docker buildx imagetools inspect gcr.io/distroless/cc-debian12:nonroot`.
#
#   docker build --build-arg MOSAICA_BUILD_COMMIT=$(git rev-parse HEAD) \
#     --build-arg MOSAICA_VERSION=$(python3 scripts/version.py) -t mosaica .

FROM rust:1-bookworm@sha256:48b7f435b8fc6378ce43b44f15e3a47fd8919b2825f835c39bd4bd3da50af5b8 AS build
WORKDIR /src
ARG MOSAICA_BUILD_COMMIT=unknown
ENV MOSAICA_BUILD_COMMIT=${MOSAICA_BUILD_COMMIT} \
    CARGO_PROFILE_RELEASE_STRIP=symbols
COPY Cargo.toml Cargo.lock ./
COPY crates crates
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,target=/src/target,sharing=locked \
    cargo build --release --locked -p mosaica \
    && install -D target/release/mosaica /out/usr/local/bin/mosaica \
    && install -d -o 65532 -g 65532 /out/var/lib/mosaica /out/etc/mosaica

FROM gcr.io/distroless/cc-debian12:nonroot@sha256:9dac0a79194e45a7da0158a9c6da57b217585af0786db3845d1f0ec1a0dd182f
ARG MOSAICA_BUILD_COMMIT=unknown
ARG MOSAICA_VERSION=unknown
LABEL org.opencontainers.image.title="mosaica" \
      org.opencontainers.image.description="Serve an interactive map over billions of points from one machine, with every view computed over what each viewer may see." \
      org.opencontainers.image.source="https://github.com/mosaicadb/mosaica" \
      org.opencontainers.image.url="https://github.com/mosaicadb/mosaica" \
      org.opencontainers.image.documentation="https://github.com/mosaicadb/mosaica/blob/main/docs/guides/docker.md" \
      org.opencontainers.image.licenses="Apache-2.0" \
      org.opencontainers.image.authors="Joe Ennis <joe.ennis0@gmail.com>" \
      org.opencontainers.image.version="${MOSAICA_VERSION}" \
      org.opencontainers.image.revision="${MOSAICA_BUILD_COMMIT}"
COPY --from=build /out/ /
COPY LICENSE THIRD_PARTY_LICENSES /usr/share/doc/mosaica/
WORKDIR /etc/mosaica
VOLUME ["/var/lib/mosaica"]
EXPOSE 8080 8081 8082
USER 65532:65532
# A large bundle can take minutes to open; failures in the start period are not counted.
HEALTHCHECK --interval=15s --timeout=5s --start-period=10m --start-interval=2s \
    CMD ["/usr/local/bin/mosaica", "health"]
ENTRYPOINT ["/usr/local/bin/mosaica"]
CMD ["serve"]
