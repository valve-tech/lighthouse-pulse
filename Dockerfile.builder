# Build image for building executables on Gitlab-CI.
FROM rust:1

# Install cross for cross-compilation
ENV CROSS_REMOTE=true
RUN cargo install cross --git https://github.com/cross-rs/cross

# Add Docker's official GPG key
RUN apt-get update
RUN apt-get install ca-certificates curl
RUN install -m 0755 -d /etc/apt/keyrings
RUN curl -fsSL https://download.docker.com/linux/debian/gpg -o /etc/apt/keyrings/docker.asc
RUN chmod a+r /etc/apt/keyrings/docker.asc

# Add the repository to Apt sources
RUN echo \
  "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/docker.asc] https://download.docker.com/linux/debian \
  $(. /etc/os-release && echo "$VERSION_CODENAME") stable" | \
  tee /etc/apt/sources.list.d/docker.list > /dev/null

# Install docker-cli for use in pipeline builds
RUN apt update && apt install -y docker-ce-cli

# Convenience for local builds
WORKDIR /app
RUN git config --global --add safe.directory /app
