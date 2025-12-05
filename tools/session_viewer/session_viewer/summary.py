from __future__ import annotations

import json
from textwrap import shorten
from typing import Optional

from .models import RawEvent, TimelineCard

ROLE_ICONS = {
    "user": "lucide-user",
    "assistant": "lucide-robot",
    "system": "lucide-cog",
}

EVENT_ICON_MAP = {
    "user_event": "lucide-message-square",
    "assistant_event": "lucide-robot",
    "thinking": "lucide-brain",
    "tool": "lucide-wrench",
    "exec": "lucide-terminal",
    "tokens": "lucide-activity",
    "ghost_snapshot": "lucide-copy",
    "turn_context": "lucide-sliders",
    "reasoning": "lucide-lightbulb",
    "system_notice": "lucide-bell",
    "warning": "lucide-alert-triangle",
    "event": "lucide-info",
}


def _truncate_text(value: Optional[str], *, width: int = 600) -> Optional[str]:
    if not value:
        return None
    single = value.strip()
    if len(single) <= width:
        return single
    return shorten(single, width=width, placeholder="…")


def to_card(event: RawEvent, delta: Optional[float]) -> Optional[TimelineCard]:
    payload = event.payload
    if event.type == "response_item":
        item_type = payload.get("type")
        if item_type == "message":
            role = payload.get("role", "assistant")
            text_chunks = []
            for part in payload.get("content", []):
                if part.get("type") == "input_text":
                    text_chunks.append(part.get("text", ""))
                elif part.get("type") == "output_text":
                    text_chunks.append(part.get("text", ""))
            text = "\n".join(text_chunks).strip()
            icon_class = ROLE_ICONS.get(role, "lucide-message-circle")
            title = f"{role.title()} message"
            return TimelineCard(
                timestamp=event.timestamp,
                delta_seconds=delta,
                kind="message",
                title=title,
                body=_truncate_text(text, width=800),
                icon="",
                icon_class=icon_class,
                presentation="summary-collapsed",
                raw=event.raw,
            )
        if item_type == "tool_result":
            tool_name = payload.get("tool_name", "tool")
            output_chunks = []
            for part in payload.get("content", []):
                if part.get("type") == "output_text":
                    output_chunks.append(part.get("text", ""))
            return TimelineCard(
                timestamp=event.timestamp,
                delta_seconds=delta,
                kind="tool",
                title=f"Tool result: {tool_name}",
                body=_truncate_text("\n".join(output_chunks), width=800),
                icon="",
                icon_class=EVENT_ICON_MAP.get("tool", "lucide-wrench"),
                presentation="summary-collapsed",
                raw=event.raw,
            )
    if event.type == "event_msg":
        msg_type = payload.get("type")
        if msg_type == "user_message":
            return TimelineCard(
                timestamp=event.timestamp,
                delta_seconds=delta,
                kind="user_event",
                title="User Command",
                body=_truncate_text(payload.get("message")),
                icon="",
                icon_class=EVENT_ICON_MAP.get("user_event", "lucide-message-square"),
                presentation="summary-collapsed",
                raw=event.raw,
            )
        if msg_type == "agent_message":
            return TimelineCard(
                timestamp=event.timestamp,
                delta_seconds=delta,
                kind="assistant_event",
                title="Assistant",
                body=_truncate_text(payload.get("message")),
                icon="",
                icon_class=EVENT_ICON_MAP.get("assistant_event", "lucide-robot"),
                presentation="summary-collapsed",
                raw=event.raw,
            )
        if msg_type == "thinking":
            return TimelineCard(
                timestamp=event.timestamp,
                delta_seconds=delta,
                kind="thinking",
                title="Agent reasoning",
                body=_truncate_text(payload.get("text")),
                icon="",
                icon_class=EVENT_ICON_MAP.get("thinking", "lucide-brain"),
                presentation="summary-collapsed",
                raw=event.raw,
            )
        if msg_type == "token_count":
            info = payload.get("info") or {}
            total = info.get("total_token_usage") or {}
            text = f"Input {total.get('input_tokens')}, Output {total.get('output_tokens')}"
            return TimelineCard(
                timestamp=event.timestamp,
                delta_seconds=delta,
                kind="tokens",
                title="Token usage",
                body=text,
                icon="",
                icon_class=EVENT_ICON_MAP.get("tokens", "lucide-activity"),
                presentation="hidden",
                raw=event.raw,
            )
    if event.type == "thinking":
        return TimelineCard(
            timestamp=event.timestamp,
            delta_seconds=delta,
            kind="thinking",
            title="Agent reasoning",
            body=_truncate_text(payload.get("text")),
            icon="",
            icon_class=EVENT_ICON_MAP.get("thinking", "lucide-brain"),
            presentation="summary-collapsed",
            raw=event.raw,
        )
    if event.type == "exec":
        command = payload.get("command")
        stdout = payload.get("stdout") or payload.get("output")
        stderr = payload.get("stderr")
        code = []
        if stdout:
            code.append(stdout.strip())
        if stderr:
            code.append("STDERR:\n" + stderr.strip())
        return TimelineCard(
            timestamp=event.timestamp,
            delta_seconds=delta,
            kind="exec",
            title=f"Shell command: {command}",
            code="\n\n".join(code) if code else None,
            icon="",
            icon_class=EVENT_ICON_MAP.get("exec", "lucide-terminal"),
            presentation="summary-open",
            raw=event.raw,
        )
    # fallback
    return TimelineCard(
        timestamp=event.timestamp,
        delta_seconds=delta,
        kind=event.type,
        title=f"Event: {event.type}",
        body=_truncate_text(json.dumps(event.payload, ensure_ascii=False, indent=2), width=1200),
        icon="",
        icon_class=EVENT_ICON_MAP.get("event", "lucide-info"),
        presentation="summary-collapsed",
        raw=event.raw,
    )
