FROM rust:slim-bookworm as build
ARG GIT_TOKEN 
COPY ./ /app/
WORKDIR /app
RUN cat /etc/resolv.conf && apt-get update
RUN apt-get -y install pkg-config libssl-dev git protobuf-compiler
RUN touch ~/.gitconfig
RUN echo "[credential]\n\thelper = store" > ~/.gitconfig
RUN git config --global credential.helper store
RUN echo "https://oauth2:$GIT_TOKEN@gitlab.databending.ca" > ~/.git-credentials
RUN rustup default nightly
RUN cargo +nightly build --bin server --release


FROM debian:bookworm-slim
RUN  cat /etc/resolv.conf && apt-get update
RUN apt -y install pkg-config libssl-dev ca-certificates
WORKDIR /app
COPY --from=build /app/target/release/server /app/server
RUN chmod +x server 
ENTRYPOINT ["./server"]
