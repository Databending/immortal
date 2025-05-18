FROM debian:bookworm-slim
RUN  cat /etc/resolv.conf && apt-get update
RUN apt -y install pkg-config libssl-dev ca-certificates
WORKDIR /app
COPY --from=build /app/target/release/server /app/server
RUN chmod +x server 
ENTRYPOINT ["./server"]
