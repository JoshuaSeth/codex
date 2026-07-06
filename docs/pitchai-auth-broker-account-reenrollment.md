# PitchAI Codex Auth Broker Account Re-enrollment

This is the recovery runbook for re-enrolling the PitchAI ChatGPT/Codex accounts when a master refresh token expires or is rejected.

## Target Accounts

- `jozuasethvanderbijl@gmail.com`
- `seth.vanderbijl@pitchai.net`
- `sales@pitchai.net`
- `info@pitchai.net`
- `elise@pitchai.net`
- `support@pitchai.net`
- `onboarding.bigi.net` (broker label; logs in with mailbox `onboarding@pitchai.net`)

## Normal Health Check

Run this from the Codex host:

```bash
/usr/local/sbin/codex-auth-broker-probe-all
```

The probe refreshes access tokens when needed, stores rotated refresh tokens back into the broker, and prints only sanitized quota fields. Use `--json` for automation.

## Re-enroll One Outlook Mailbox Account

Use a clean `CODEX_HOME` per account so the resulting `auth.json` is unambiguous:

```bash
ACCOUNT="info@pitchai.net"
HOME_DIR="/root/.codex/enroll-${ACCOUNT%@*}"
rm -rf "$HOME_DIR"
mkdir -p "$HOME_DIR"

CODEX_HOME="$HOME_DIR" \
CODEX_AUTH_BROKER_IMPORT_LABEL="$ACCOUNT" \
CODEX_DEV_AUTO_BUILD=0 \
codex-dev login
```

When the browser asks for a code, read the latest OpenAI verification email from the matching Microsoft 365 mailbox. After the browser reaches the local Codex success page, `codex-dev` imports the new refresh token into auth-token-server and immediately probes usage.

For `onboarding.bigi.net`, keep the broker label as `onboarding.bigi.net` but use the PitchAI shared mailbox `onboarding@pitchai.net` for the OpenAI one-time-code login. Microsoft 365 does not currently have a `bigi.net` tenant domain or a literal `onboarding.bigi.net` mailbox.

## Re-enroll One Google Account

Use the same isolated `CODEX_HOME` pattern. If Google asks for authenticator MFA, use the 2FA helpers:

```bash
ssh root@37.27.67.52 /opt/2fa-server/bin/google-code-jozua-gmail
ssh root@37.27.67.52 /opt/2fa-server/bin/google-code-seth-pitchai
```

Enter the fresh 6-digit code in the browser, finish the Codex consent screen, and let `codex-dev login` return normally so broker import/probe runs.

## Verify Enrollment

```bash
/usr/local/sbin/codex-auth-broker-probe-all --json
```

Each target account should show `availability=available` unless its current 5-hour or weekly Codex window is genuinely exhausted. Active session metadata is informational only; it must not be treated as exclusive capacity or a reason to skip an otherwise valid account.

## Operational Notes

- Broker data lives at `/srv/auth-token-server/data`.
- Broker secrets are loaded from `/etc/auth-token-server/auth-token-server.env`.
- The admin UI is `http://127.0.0.1:38188/admin` from the host or via SSH tunnel.
- `codex-dev` imports current `auth.json` on login and startup when broker admin env is available.
- The dispatcher voice/app-server path must launch through `codex-dev-wrapper.sh`, not the raw Codex binary, so broker env and login import are present.
