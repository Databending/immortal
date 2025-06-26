FROM debian:bookworm-slim
RUN  cat /etc/resolv.conf && apt-get update
RUN apt -y install pkg-config libssl-dev ca-certificates
WORKDIR /app
COPY target/release/server /app/server
COPY bin/tokio-console /app/tokio-console
RUN chmod +x server 
RUN chmod +x tokio-console 
ENTRYPOINT ["./server"]
