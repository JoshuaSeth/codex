from __future__ import annotations

import json
from textwrap import shorten
from typing import Optional

from markdown import markdown

from .models import RawEvent, TimelineCard

ROLE_ICONS = {
    "user": "user",
    "assistant": "bot",
    "system": "settings",
}

EVENT_ICON_MAP = {
    "user_event": "message-square",
    "assistant_event": "bot",
    "thinking": "brain",
    "tool": "wrench",
    "exec": "terminal",
    "tokens": "activity",
    "ghost_snapshot": "copy",
    "turn_context": "sliders",
    "reasoning": "brain-circuit",
    "system_notice": "bell",
    "warning": "alert-triangle",
    "event": "info",
}


def _icon_class_for_name(icon_name: str) -> str:
    return f"icon icon-{icon_name}"


def _icon_class(key: str, fallback: str = "info") -> str:
    icon_name = EVENT_ICON_MAP.get(key, fallback)
    return _icon_class_for_name(icon_name)


def _truncate_text(value: Optional[str], *, width: int = 600) -> Optional[str]:
    if not value:
        return None
    single = value.strip()
    if len(single) <= width:
        return single
    return shorten(single, width=width, placeholder="…")


def _render_markdown(value: Optional[str]) -> Optional[str]:
    if not value:
        return None
    return markdown(value, extensions=["fenced_code", "tables", "sane_lists"], output_format="html5")


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
            icon_class = _icon_class_for_name(ROLE_ICONS.get(role, "message-square"))
            title = f"{role.title()} message"
            presentation = "hidden" if role == "user" else "summary-collapsed"
            return TimelineCard(
                timestamp=event.timestamp,
                delta_seconds=delta,
                kind="message",
                title=title,
                body=text,
                body_html=_render_markdown(text),
                icon="",
                icon_class=icon_class,
                presentation=presentation,
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
                icon_class=_icon_class("tool"),
                presentation="summary-collapsed",
                raw=event.raw,
            )
    if event.type == "event_msg":
        msg_type = payload.get("type")
        if msg_type == "user_message":
            markdown_body = _render_markdown(payload.get("message"))
            return TimelineCard(
                timestamp=event.timestamp,
                delta_seconds=delta,
                kind="user_event",
                title="User Command",
                body=payload.get("message"),
                body_html=markdown_body,
                icon="",
                icon_class=_icon_class("user_event"),
                presentation="full",
                raw=event.raw,
            )
        if msg_type == "agent_message":
            markdown_body = _render_markdown(payload.get("message"))
            return TimelineCard(
                timestamp=event.timestamp,
                delta_seconds=delta,
                kind="assistant_event",
                title="Assistant",
                body=payload.get("message"),
                body_html=markdown_body,
                icon="",
                icon_class=_icon_class("assistant_event"),
                presentation="full",
                raw=event.raw,
            )
        if msg_type == "agent_reasoning":
            markdown_body = _render_markdown(payload.get("text"))
            return TimelineCard(
                timestamp=event.timestamp,
                delta_seconds=delta,
                kind="reasoning",
                title="Agent reasoning",
                body=payload.get("text"),
                body_html=markdown_body,
                icon="",
                icon_class=_icon_class("reasoning"),
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
                icon_class=_icon_class("thinking"),
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
                icon_class=_icon_class("tokens"),
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
            icon_class=_icon_class("thinking"),
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
            icon_class=_icon_class("exec"),
            presentation="summary-open",
            raw=event.raw,
        )
    presentation = "hidden" if event.type in {"turn_context", "response_item"} else "summary-collapsed"
    return TimelineCard(
        timestamp=event.timestamp,
        delta_seconds=delta,
        kind=event.type,
        title=f"Event: {event.type}",
        body=_truncate_text(json.dumps(event.payload, ensure_ascii=False, indent=2), width=1200),
        icon="",
        icon_class=_icon_class("event"),
        presentation=presentation,
        raw=event.raw,
    )
