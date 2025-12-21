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

# Ensure default node user has access to /usr/local/share
RUN mkdir -p /usr/local/share/npm-global && \
  chown -R node:node /usr/local/share

ARG USERNAME=node

# Set up non-root user
USER node

# Install global packages
ENV NPM_CONFIG_PREFIX=/usr/local/share/npm-global
ENV PATH=$PATH:/usr/local/share/npm-global/bin

# Install codex
COPY dist/codex.tgz codex.tgz
RUN npm install -g codex.tgz \
  && npm cache clean --force \
  && rm -rf /usr/local/share/npm-global/lib/node_modules/codex-cli/node_modules/.cache \
  && rm -rf /usr/local/share/npm-global/lib/node_modules/codex-cli/tests \
  && rm -rf /usr/local/share/npm-global/lib/node_modules/codex-cli/docs

# PitchAI SharePoint tooling dependencies + assets
USER root
COPY scripts/pitchai_sharepoint_requirements.txt /tmp/pitchai_sharepoint_requirements.txt
RUN mkdir -p /opt/pitchai \
  && python3 -m venv /opt/pitchai/venv \
  && /opt/pitchai/venv/bin/pip install --no-cache-dir -r /tmp/pitchai_sharepoint_requirements.txt \
  && rm -f /tmp/pitchai_sharepoint_requirements.txt
COPY scripts/pitchai_sharepoint_tool.py /opt/pitchai/sharepoint_tool.py
COPY scripts/pitchai_codex_config.toml /opt/pitchai/config.toml
COPY scripts/pitchai_sharepoint_inbox_prompt.md /opt/pitchai/sharepoint_inbox_prompt.md
COPY scripts/pitchai_run_sharepoint_inbox_job.sh /opt/pitchai/run_sharepoint_inbox_job.sh
RUN chmod 755 /opt/pitchai/sharepoint_tool.py /opt/pitchai/run_sharepoint_inbox_job.sh
USER node

# Inside the container we consider the environment already sufficiently locked
# down, therefore instruct Codex CLI to allow running without sandboxing.
ENV CODEX_UNSAFE_ALLOW_NO_SANDBOX=1

# Copy and set up firewall script as root.
USER root
COPY scripts/init_firewall.sh /usr/local/bin/
RUN chmod 500 /usr/local/bin/init_firewall.sh

# Drop back to non-root.
USER node
