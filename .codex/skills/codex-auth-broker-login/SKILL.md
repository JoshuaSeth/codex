---
name: codex-auth-broker-login
description: Use when recovering, re-enrolling, or validating PitchAI Codex auth broker ChatGPT accounts for codex-dev, including Outlook one-time-code accounts, Google MFA accounts, and Apple/private-relay accounts through isolated browser sessions.
---

# Codex Auth Broker Account Login Recovery

## Objective

Recover or re-enroll Codex ChatGPT accounts into the local auth broker so `codex-dev` can rotate accounts automatically when a token, quota, refresh, or rate-limit issue occurs.

This skill is repo-local on purpose. Do not copy it into `/root/.codex/skills` unless explicitly asked.

## Hard Rules

- Do not print, commit, paste, or store secrets in this repo: `auth.json`, refresh tokens, OAuth callback `code` values, SMS codes, M365 Graph tokens, Apple passwords, Google passwords, Telegram bot tokens, or broker admin tokens.
- Do not touch the user's active browser window. Use a separate Chrome process with an isolated cloned profile and a dedicated `--remote-debugging-port`.
- Do not use `open -a "Google Chrome" "$AUTH_URL"` or AppleScript against the foreground browser for account enrollment.
- Do not mark quota-limited accounts as login failures. `rate_limited` with valid usage metadata means auth works and the account is waiting for quota reset.
- Keep temporary auth URLs, callback URLs, and CDP logs out of git and delete them after recovery.
- Prefer loud, explicit failure over silent fallback when a login did not produce a broker import.

## Account Inventory

The broker has been used with these account labels:

- Outlook mailbox one-time-code accounts:
  - `sales@pitchai.net`
  - `info@pitchai.net`
  - `elise@pitchai.net`
  - `support@pitchai.net`
  - `onboarding.bigi.net` (broker label; OpenAI login code mailbox is `onboarding@pitchai.net`)
- Google accounts:
  - `jozuasethvanderbijl@gmail.com`
  - `seth.vanderbijl@pitchai.net`
- Apple/private-relay account:
  - `svxjvmk78b@privaterelay.appleid.com`

Treat this list as the expected enrollment set. If the broker has fewer accounts, re-enroll the missing ones instead of assuming they were intentionally removed.

## Broker Context

Runtime:

- Broker service: `auth-token-server`
- Local URL: `http://127.0.0.1:38188`
- Health check: `http://127.0.0.1:38188/healthz`
- Persistent data: `/srv/auth-token-server/data`
- Root-only env file: `/etc/auth-token-server/auth-token-server.env`
- Admin UI: `http://127.0.0.1:38188/admin`

`codex-dev` configures broker access in `scripts/codex-dev-wrapper.sh`.

Important wrapper behavior:

- Reads broker client/admin tokens from `/etc/auth-token-server/auth-token-server.env`.
- Exports `CODEX_AUTH_BROKER_URL`, `CODEX_AUTH_BROKER_TOKEN`, `CODEX_AUTH_BROKER_CLIENT_NAME`, and rotation settings for normal runs.
- Exports `CODEX_AUTH_BROKER_IMPORT_ON_LOGIN=1` when admin broker env is available.
- On `codex-dev login`, waits for `$CODEX_HOME/auth.json`, imports that file to `/v1/admin/accounts/import`, then probes the imported account.
- On normal startup, imports an existing `$CODEX_HOME/auth.json` when `CODEX_AUTH_BROKER_IMPORT_EXISTING_ON_STARTUP` is enabled.

## Preflight

Check broker health:

```bash
curl -sfS http://127.0.0.1:38188/healthz
```

Probe all broker accounts:

```bash
/usr/local/sbin/codex-auth-broker-probe-all
```

Use JSON output when you need exact timestamps or to diff state:

```bash
/usr/local/sbin/codex-auth-broker-probe-all --json
```

Interpretation:

- `available`: auth is valid and broker can select the account.
- active session metadata is informational only; it must not reserve or block an otherwise valid account.
- `rate_limited`: auth is usually valid; quota is the blocker. Check 5h and weekly reset fields.
- `auth_invalid`: token refresh/login is broken; re-enroll this account.
- `disabled`: broker will not select it until re-enabled.

## Common Enrollment Pattern

Use a unique `CODEX_HOME` per account so an enrollment does not overwrite another working login.

```bash
ACCOUNT="sales@pitchai.net"
LABEL="$ACCOUNT"
HOME_DIR="/root/.codex/enroll-${ACCOUNT%@*}-$(date +%Y%m%d%H%M%S)"

rm -rf "$HOME_DIR"
mkdir -p "$HOME_DIR"
chmod 700 "$HOME_DIR"

CODEX_HOME="$HOME_DIR" \
CODEX_AUTH_BROKER_IMPORT_LABEL="$LABEL" \
CODEX_DEV_AUTO_BUILD=0 \
codex-dev login
```

After browser success, verify that the wrapper prints an import success message. Then keep a root-only backup for later recovery:

```bash
BACKUP="/root/.codex/${ACCOUNT%@*}_auth.json"
cp "$HOME_DIR/auth.json" "$BACKUP"
chmod 600 "$BACKUP"
```

Then probe:

```bash
/usr/local/sbin/codex-auth-broker-probe-all
```

If `codex-dev login` exits successfully but no broker import happens, inspect whether the wrapper could read admin broker env. Do not manually paste tokens into commands; fix wrapper/env access or rerun as root.

## Outlook Mailbox Accounts

Accounts:

- `sales@pitchai.net`
- `info@pitchai.net`
- `elise@pitchai.net`
- `support@pitchai.net`
- `onboarding.bigi.net` uses the same Outlook mailbox one-time-code flow, with broker label `onboarding.bigi.net` and Microsoft 365 mailbox `onboarding@pitchai.net`.

These typically log in with OpenAI's one-time email code flow through Microsoft mailboxes.

Workflow:

1. Start `codex-dev login` with an isolated `CODEX_HOME` and `CODEX_AUTH_BROKER_IMPORT_LABEL` matching the mailbox.
2. Open the printed login URL in an isolated browser profile.
3. If OpenAI shows an account picker, choose "Log in to another account" unless the exact mailbox is shown.
4. Enter the mailbox address.
5. If a password page appears, choose the one-time-code option instead of trying mailbox passwords.
6. Read the latest OpenAI verification code from that mailbox through the M365 Graph app-only tooling.
7. Fill the code in the OpenAI browser page.
8. Click the Codex consent/authorization `Continue` button.
9. Wait for local login success, wrapper import, and broker probe.

Use the M365 skill/scripts to mint a Graph token and read mail without user reauth. The relevant script lives globally:

```bash
/root/.codex/skills/m365/scripts/mint_graph_token.mjs
```

Use Graph to query the Inbox for recent OpenAI messages. Keep the access token out of logs:

```bash
MAILBOX="sales@pitchai.net"
TOKEN="$(/root/.codex/skills/m365/scripts/mint_graph_token.mjs)"
curl -sfS \
  -H "Authorization: Bearer $TOKEN" \
  "https://graph.microsoft.com/v1.0/users/$MAILBOX/mailFolders/Inbox/messages?%24top=10&%24orderby=receivedDateTime%20desc&%24select=subject,receivedDateTime,from,bodyPreview"
```

Extract the newest OpenAI one-time code from `bodyPreview` or the message body, then enter it in the browser. Do not store the code.

Observed recovery notes:

- `sales@pitchai.net` had become `auth_invalid`; a fresh browser one-time-code login successfully wrote `auth.json`, imported to the broker, and became selectable.
- `info@pitchai.net` was recoverable by importing/probing an existing backup token; a separate browser attempt showed OpenAI can route mailbox accounts to a password page, where the correct path is the one-time-code option.
- `elise@pitchai.net` and `support@pitchai.net` had valid auth during recovery; their blocker was quota/rate limiting, not login.

## Google Accounts

Accounts:

- `jozuasethvanderbijl@gmail.com`
- `seth.vanderbijl@pitchai.net`

Workflow:

1. Start `codex-dev login` with a unique `CODEX_HOME` and label matching the Google account.
2. Open the auth URL in an isolated browser profile.
3. Select or enter the matching Google account.
4. Complete Google MFA with the saved session or the PitchAI 2FA helper.
5. Click the Codex authorization `Continue` button.
6. Wait for wrapper import and probe the broker.

Known Google 2FA helpers:

```bash
ssh root@37.27.67.52 /opt/2fa-server/bin/google-code-jozua-gmail
ssh root@37.27.67.52 /opt/2fa-server/bin/google-code-seth-pitchai
```

Use the helper output only in the browser login form. Do not write codes into PM comments, git, or durable logs.

Observed recovery notes:

- `jozuasethvanderbijl@gmail.com` was auth-valid after broker probing and available for selection.
- `seth.vanderbijl@pitchai.net` was auth-valid but weekly quota-limited during recovery; that is not a login problem.

## Apple / Private Relay Account

Account:

- `svxjvmk78b@privaterelay.appleid.com`

This account is the most sensitive because Apple/iCloud login can touch device prompts, SMS fallback, and active browser sessions. Use an isolated browser process on `travel-macbook`; do not manipulate the user's focused browser.

### Actual Successful Pattern

1. Start `codex-dev login` on the server in a foreground TTY so the local callback server remains alive.
2. Capture the printed OpenAI auth URL.
3. On `travel-macbook`, clone Chrome's `Default` profile into `/tmp/codex-svx-apple-*`, excluding caches and lock/singleton files.
4. Launch a separate Chrome binary with the cloned profile, remote debugging on `127.0.0.1:9365`, and an offscreen window.
5. Run the CDP driver locally on `travel-macbook` over SSH because SSH TCP forwarding to the Mac can be blocked with `administratively prohibited`.
6. Use CDP to click the existing `svxjvmk78b@privaterelay.appleid.com` OpenAI account picker entry.
7. Click Codex consent `Continue`.
8. Read the callback URL from Chrome's CDP page metadata. It will point at `http://localhost:<port>/auth/callback?...` on the Mac.
9. Rewrite the callback host to `http://127.0.0.1:<port>` and `curl` it on the server where `codex-dev login` is listening.
10. If the callback redirects to `/success`, also request `/success` on the server so the login process exits and writes `auth.json`.
11. Copy the resulting `auth.json` to a root-only backup and probe the broker.

### Start Server-Side Login

Use a TTY session, not background `nohup`, because background login attempts can exit before the callback is replayed:

```bash
ACCOUNT="svxjvmk78b@privaterelay.appleid.com"
HOME_DIR="/root/.codex/enroll-svxjvmk78b-apple-$(date +%Y%m%d%H%M%S)"

rm -rf "$HOME_DIR"
mkdir -p "$HOME_DIR"
chmod 700 "$HOME_DIR"

CODEX_HOME="$HOME_DIR" \
CODEX_AUTH_BROKER_IMPORT_LABEL="$ACCOUNT" \
CODEX_DEV_AUTO_BUILD=0 \
codex-dev login
```

Leave that command running while driving the browser from another shell/tool call.

### Launch Isolated Chrome On travel-macbook

Run this over SSH on `travel-macbook`. The key point is that the cloned profile preserves useful session state without attaching to the active user browser.

```bash
AUTH_URL='paste-the-openai-auth-url-here'
RUN_ROOT="/tmp/codex-svx-apple-$(date +%s)"
SRC="$HOME/Library/Application Support/Google/Chrome/Default"

mkdir -p "$RUN_ROOT"
rsync -a \
  --exclude 'Cache' \
  --exclude 'Code Cache' \
  --exclude 'GPUCache' \
  --exclude 'Service Worker/CacheStorage' \
  --exclude 'Singleton*' \
  "$SRC/" "$RUN_ROOT/Default/"

"/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" \
  --user-data-dir="$RUN_ROOT" \
  --profile-directory=Default \
  --remote-debugging-address=127.0.0.1 \
  --remote-debugging-port=9365 \
  --no-first-run \
  --no-default-browser-check \
  --window-position=-32000,-32000 \
  --window-size=1200,900 \
  "$AUTH_URL" >/tmp/codex-svx-apple-chrome.log 2>&1 &
```

Do not use `open` for this. Calling `open` can reuse or focus the user's active browser.

### Drive CDP Locally On travel-macbook

TCP forwarding from the server to `travel-macbook` may fail:

```text
channel open failed: administratively prohibited: open failed
```

When that happens, run the CDP script on the Mac through SSH. The script should:

- GET `http://127.0.0.1:9365/json/list`.
- Connect to the page's `webSocketDebuggerUrl`.
- Inspect DOM text and buttons.
- Click the `svxjvmk78b@privaterelay.appleid.com` account row if present.
- Click `Continue` on the Codex authorization page.
- Poll `/json/list` until the page URL contains `/auth/callback`.
- Print only the final callback URL to stdout or a root-readable temp file.

Keep the full callback URL out of persistent logs because it contains a one-time OAuth code.

### Replay Callback On Server

The callback URL captured from the Mac uses the Mac's localhost. Replay it against the server-side login listener:

```bash
CALLBACK_URL='paste-callback-url-from-cdp'
SERVER_CALLBACK_URL="${CALLBACK_URL/http:\\/\\/localhost/http:\\/\\/127.0.0.1}"

curl -i "$SERVER_CALLBACK_URL"
curl -i "http://127.0.0.1:1455/success"
```

Use the actual callback port from the auth URL if it is not `1455`.

Then back up and probe:

```bash
cp "$HOME_DIR/auth.json" /root/.codex/svxjvmk78b_auth.json
chmod 600 /root/.codex/svxjvmk78b_auth.json
/usr/local/sbin/codex-auth-broker-probe-all
```

### iCloud / SMS Fallback

If Apple requests SMS verification, use `travel-macbook`'s iMessage helper. Capture a cursor before triggering the SMS send so an old code is not reused:

```bash
CURSOR="$(ssh travel-macbook '~/bin/imessage-code --cursor')"
```

Click Apple's SMS fallback in the isolated browser, then wait for a new code:

```bash
ssh travel-macbook "~/bin/imessage-code --wait --after-rowid $CURSOR --timeout 120"
```

Fill the six code boxes in the isolated browser via CDP input/change events. Do not paste the code into logs.

For a quick check of the latest code parser output:

```bash
ssh travel-macbook '~/bin/imessage-code --latest --json || true'
```

Observed recovery note: the successful private-relay recovery did not need a fresh SMS code because the cloned Chrome profile already had a usable OpenAI/private-relay session in the account picker.

## Backup Token Re-Import

If a broker account is missing but a trusted backup `auth.json` exists under `/root/.codex`, launch `codex-dev` once with that backup as the active `CODEX_HOME/auth.json`. The wrapper's startup import can restore it to the broker.

Example:

```bash
ACCOUNT="info@pitchai.net"
HOME_DIR="/root/.codex/reimport-${ACCOUNT%@*}-$(date +%Y%m%d%H%M%S)"
mkdir -p "$HOME_DIR"
chmod 700 "$HOME_DIR"
cp "/root/.codex/${ACCOUNT%@*}_auth.json" "$HOME_DIR/auth.json"
chmod 600 "$HOME_DIR/auth.json"

CODEX_HOME="$HOME_DIR" \
CODEX_AUTH_BROKER_IMPORT_LABEL="$ACCOUNT" \
CODEX_DEV_AUTO_BUILD=0 \
codex-dev --help >/dev/null
```

Then probe all accounts. If the probe marks it `auth_invalid`, perform a fresh browser login instead.

## Validation

After enrolling any account:

```bash
/usr/local/sbin/codex-auth-broker-probe-all
```

Check that:

- The account label appears in broker output.
- The state is `available` or `rate_limited`, not `auth_invalid`.
- 5h and weekly quota fields are present when the service reports them.
- Reset timestamps make sense for quota-limited accounts.

Then launch a tiny `codex-dev` session with broker enabled if you need end-to-end selection verification:

```bash
CODEX_DEV_AUTO_BUILD=0 codex-dev --yolo "Say ok and stop."
```

Do not force a non-stop session for verification unless you explicitly need to exercise rotation.

## Cleanup

Remove temporary isolated browser state after Apple/private-relay recovery:

```bash
ssh travel-macbook "pkill -f '/tmp/codex-svx-apple' || true"
ssh travel-macbook "rm -rf /tmp/codex-svx-apple-* /tmp/codex-svx-apple-chrome.log"
```

Remove local temp files that contained auth URLs or callback URLs:

```bash
rm -f /tmp/codex-auth-url-* /tmp/codex-callback-url-* /tmp/codex-login-*.log
```

Never delete broker data under `/srv/auth-token-server/data` during login recovery unless the user explicitly asks for destructive broker maintenance.

## Failure Modes

- `auth broker 409 Conflict: No enabled account is currently available`: all accounts are disabled, invalid, cooling down, or quota-limited. Probe all accounts and inspect counts; this is not necessarily a wrapper bug.
- `auth_invalid`: re-login or import a valid backup token.
- `rate_limited`: wait for the listed 5h/weekly reset or rotate to another available account.
- Background login exits before callback: rerun `codex-dev login` in a foreground TTY and replay the callback while it is alive.
- Mac CDP forwarding blocked: run the CDP driver directly on `travel-macbook` over SSH instead of trying more port-forward variants.
- OpenAI mailbox flow asks for password: choose one-time-code login for Outlook mailboxes.
- Browser automation focuses user Chrome: stop immediately, kill only the isolated process by matching its `/tmp/codex-*` user-data-dir, and relaunch with a cloned profile and explicit `--user-data-dir`.
