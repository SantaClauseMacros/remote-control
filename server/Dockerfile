# Build the relay as a small static-ish binary and run it on a distroless base.
FROM rust:1-slim AS build
WORKDIR /src
COPY . .
RUN cargo build --release -p rc-relay

FROM gcr.io/distroless/cc-debian12
COPY --from=build /src/target/release/rc-relay /usr/local/bin/rc-relay
# 9878/tcp: the rendezvous port. Nothing else is exposed.
EXPOSE 9878
ENTRYPOINT ["/usr/local/bin/rc-relay"]
CMD ["--bind", "0.0.0.0:9878"]
