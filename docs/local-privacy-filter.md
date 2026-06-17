# Local Privacy Filter

PitchAI can opt in to a local privacy filter in the Codex runtime path before Responses API payloads leave the device. The filter uses OpenAI's `openai/privacy-filter` local model through the `opf` Python package from <https://github.com/openai/privacy-filter>. It does not use a remote Hugging Face inference call.

## Enable

Install the local OPF package and let it download the model checkpoint locally:

```bash
python3 -m venv ~/.opf-venv
~/.opf-venv/bin/pip install git+https://github.com/openai/privacy-filter.git
PITCHAI_CODEX_PRIVACY_FILTER_PYTHON=~/.opf-venv/bin/python \
  PITCHAI_CODEX_PRIVACY_MIDDLEWARE=1 \
  codex
```

The OPF package downloads the `openai/privacy-filter` checkpoint into local storage on first use. To use a pre-downloaded checkpoint, set `PITCHAI_CODEX_PRIVACY_FILTER_MODEL=/path/to/privacy_filter`. CPU is the default device; override with `PITCHAI_CODEX_PRIVACY_FILTER_DEVICE`.

## Behavior

When enabled, Codex runs the local detector over outbound user text and tool payload text in `codex-rs/core` request construction. Detected real PII is replaced with deterministic realistic fake values, such as fake names, addresses, emails, phone numbers, account-like identifiers, dates, and URLs.

The reversible real-to-fake mapping is kept only in the in-memory Codex session. It is not serialized into Responses API payloads and the `PrivacyFilter` debug representation reports only a mapping count.

When model output items stream back, Codex restores known fake values to the original real values before forwarding output downstream to the TUI/runtime display path. The retained API `LastResponse` items remain in backend-visible fake form.

## Failure Mode

This feature is opt-in with `PITCHAI_CODEX_PRIVACY_MIDDLEWARE=1`. If it is enabled and the local OPF detector cannot start, cannot load the local model, or returns invalid JSON, request construction fails loudly instead of sending the original text upstream.

## Threat Model And Limits

The filter is intended to prevent detected PII in prompt and tool text from being sent to backend/model providers during a local Codex session. It does not protect against PII already present in files that tools choose to upload outside the Responses API text path, terminal output produced by local commands before Codex sees it, screenshots or binary attachments, or logs written by external tools.

Detection quality depends on the local OpenAI Privacy Filter model. Missed entities may still be sent upstream, and false positives may be anonymized. The mapping is session-local memory, so it is lost when the process exits.
