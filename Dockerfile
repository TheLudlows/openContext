FROM rust:1.96-bookworm AS build
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY deploy ./deploy
RUN cargo build --release --locked

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --uid 10001 --create-home context \
    && mkdir -p /data/files && chown -R context:context /data
COPY --from=build /build/target/release/opencontext /usr/local/bin/opencontext
COPY deploy/APALIS-LICENSE /usr/share/doc/opencontext/APALIS-LICENSE
USER context
ENV OC_FILES_DIR=/data/files
EXPOSE 8080
ENTRYPOINT ["opencontext"]
CMD ["api", "--bind", "0.0.0.0:8080"]
