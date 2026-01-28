FROM rust:1.90-slim-bookworm AS codex-builder

WORKDIR /src

RUN apt-get update && apt-get install -y --no-install-recommends \
  ca-certificates \
  build-essential \
  libssl-dev \
  pkg-config \
  && rm -rf /var/lib/apt/lists/*

COPY codex-rs /src/codex-rs
WORKDIR /src/codex-rs
# Reduce peak memory usage during LTO-heavy builds in constrained Docker environments.
ENV CARGO_BUILD_JOBS=1
ENV CARGO_PROFILE_RELEASE_LTO=thin
RUN cargo build -p codex-cli --release --locked

FROM node:24-slim

ARG TZ
ENV TZ="$TZ"

# Install basic development tools, ca-certificates, and iptables/ipset, then clean up apt cache to reduce image size
RUN apt-get update && apt-get install -y --no-install-recommends \
  aggregate \
  ca-certificates \
  curl \
  dnsutils \
  fzf \
  gh \
  git \
  gnupg2 \
  iproute2 \
  ipset \
  iptables \
  jq \
  less \
  man-db \
  python3 \
  python3-pip \
  python3-venv \
  procps \
  unzip \
  ripgrep \
  tesseract-ocr \
  zsh \
  && rm -rf /var/lib/apt/lists/*

# Prepare a dedicated global install prefix (avoid polluting /usr/local).
RUN mkdir -p /usr/local/share/npm-global

# Install global packages
ENV NPM_CONFIG_PREFIX=/usr/local/share/npm-global
ENV PATH=$PATH:/usr/local/share/npm-global/bin

# Install codex (Node wrapper) from this checkout.
COPY codex-cli/package.json codex-cli/package-lock.json /tmp/codex-cli/
COPY codex-cli/bin /tmp/codex-cli/bin
RUN set -eu; \
  cd /tmp/codex-cli; \
  tgz="$(npm pack --silent)"; \
  npm install -g "./$tgz"; \
  cd /; \
  rm -rf /tmp/codex-cli; \
  npm cache clean --force; \
  rm -rf /usr/local/share/npm-global/lib/node_modules/@openai/codex/node_modules/.cache; \
  rm -rf /usr/local/share/npm-global/lib/node_modules/@openai/codex/tests; \
  rm -rf /usr/local/share/npm-global/lib/node_modules/@openai/codex/docs

# Replace the bundled native Codex binary with the one built from this checkout.
#
# Note: the npm package is `@openai/codex`, and it selects the native binary by
# target triple at runtime (e.g. `x86_64-unknown-linux-musl` or
# `aarch64-unknown-linux-musl`). Populate the appropriate vendor folder for the
# image architecture so `codex` works both locally (Apple Silicon) and in Azure.
COPY --from=codex-builder /src/codex-rs/target/release/codex /tmp/codex
RUN set -eu; \
  pkg="/usr/local/share/npm-global/lib/node_modules/@openai/codex"; \
  arch="$(uname -m)"; \
  case "$arch" in \
    x86_64) triple="x86_64-unknown-linux-musl" ;; \
    aarch64) triple="aarch64-unknown-linux-musl" ;; \
    *) echo "Unsupported arch for native codex binary: $arch" >&2; exit 1 ;; \
  esac; \
  mkdir -p "$pkg/vendor/$triple/codex"; \
  mv /tmp/codex "$pkg/vendor/$triple/codex/codex"; \
  chmod 755 "$pkg/vendor/$triple/codex/codex"

# PitchAI SharePoint tooling dependencies + assets
USER root
COPY codex-cli/scripts/pitchai_sharepoint_requirements.txt /tmp/pitchai_sharepoint_requirements.txt
RUN mkdir -p /opt/pitchai \
  && python3 -m venv /opt/pitchai/venv \
  && /opt/pitchai/venv/bin/pip install --no-cache-dir -r /tmp/pitchai_sharepoint_requirements.txt \
  && rm -f /tmp/pitchai_sharepoint_requirements.txt
COPY codex-cli/scripts/pitchai_sharepoint_tool.py /opt/pitchai/sharepoint_tool.py
COPY tools/custom_tools/telegram_bot.py /opt/pitchai/telegram_bot.py
COPY codex-cli/scripts/pitchai_codex_config.toml /opt/pitchai/config.toml
COPY codex-cli/scripts/pitchai_sharepoint_inbox_prompt.md /opt/pitchai/sharepoint_inbox_prompt.md
COPY codex-cli/scripts/pitchai_run_sharepoint_inbox_job.sh /opt/pitchai/run_sharepoint_inbox_job.sh
COPY codex-cli/scripts/pitchai_elise_config.toml /opt/pitchai/elise_config.toml
COPY codex-cli/scripts/pitchai_elise_prompt.md /opt/pitchai/elise_prompt.md
COPY codex-cli/scripts/pitchai_elise_graph_tool.py /opt/pitchai/elise_graph_tool.py
COPY codex-cli/scripts/pitchai_run_elise_job.py /opt/pitchai/run_elise_job.py
COPY codex-cli/scripts/pitchai_run_codex_job.py /opt/pitchai/run_codex_job.py
COPY codex-cli/scripts/pitchai_mail_tagger_config.toml /opt/pitchai/mail_tagger_config.toml
COPY codex-cli/scripts/pitchai_mail_tagger_prompt.md /opt/pitchai/mail_tagger_prompt.md
COPY codex-cli/scripts/pitchai_mail_tagger_skill.md /opt/pitchai/mail_tagger_skill.md
COPY codex-cli/scripts/pitchai_mail_tagger_graph_tool.py /opt/pitchai/mail_tagger_graph_tool.py
COPY codex-cli/scripts/pitchai_pm_db_tool.py /opt/pitchai/pm_db_tool.py
COPY codex-cli/scripts/pitchai_run_mail_tagger_job.sh /opt/pitchai/run_mail_tagger_job.sh
COPY docs/pitchai_elise_agent.md /opt/pitchai/elise_agent.md
RUN chmod 755 \
  /opt/pitchai/sharepoint_tool.py \
  /opt/pitchai/elise_graph_tool.py \
  /opt/pitchai/run_sharepoint_inbox_job.sh \
  /opt/pitchai/run_elise_job.py \
  /opt/pitchai/run_codex_job.py \
  /opt/pitchai/mail_tagger_graph_tool.py \
  /opt/pitchai/pm_db_tool.py \
  /opt/pitchai/run_mail_tagger_job.sh \
  /opt/pitchai/telegram_bot.py
USER node

# Inside the container we consider the environment already sufficiently locked
# down, therefore instruct Codex CLI to allow running without sandboxing.
ENV CODEX_UNSAFE_ALLOW_NO_SANDBOX=1

# Copy and set up firewall script as root.
USER root
COPY codex-cli/scripts/init_firewall.sh /usr/local/bin/
RUN chmod 500 /usr/local/bin/init_firewall.sh

# Drop back to non-root.
USER node
