"""Read-only request filter for every request the browser makes.

`collect` policy (the normal loop), from docs/signals/x-insighter-design.md:

- an X GraphQL operation (last path segment of `/i/api/graphql/<hash>/<Op>`,
  or `/graphql/<hash>/<Op>` on the api host) is allowed only as a GET whose
  name is neither a write (`_WRITE_OPERATION`) nor a private-content read
  (`_PRIVATE_OPERATION`: DMs, bookmarks, notifications). X sends every
  mutation as POST, so blocking non-GET GraphQL blocks all writes; the page's
  own bootstrap reads (e.g. `Viewer`) still run. Only the operations in
  `runner.PARSED_OPERATIONS` are turned into rows;
- any non-GET request to an X REST API path (`/i/api/1.1/`, `/i/api/2/`, any
  other `/i/api/` path, or `/1.1/`, `/2/` on the api host) is aborted, as is
  any REST read of DMs or notifications;
- any non-GET request to the upload host is aborted.

`login` policy (`--login`, operator-driven headful session): GraphQL operations
whose names look like writes are aborted, and non-GET REST calls are allowed
only for the login flow itself (`onboarding/`, `guest/activate.json`).
"""

from __future__ import annotations

import re
from dataclasses import dataclass
from typing import Literal
from urllib.parse import urlsplit

Policy = Literal["collect", "login"]

_SITE_HOSTS = frozenset({"x.com", "twitter.com"})
_API_HOSTS = frozenset({"api.x.com", "api.twitter.com"})
_UPLOAD_HOSTS = frozenset({"upload.x.com", "upload.twitter.com"})

_LOGIN_REST_WRITE_PREFIXES = ("onboarding/", "guest/activate.json")
_WRITE_OPERATION = re.compile(
    r"^(Create|Delete|Favorite|Unfavorite|Retweet|Unretweet|Follow|Unfollow|Bookmark|"
    r"Pin|Unpin|Mute|Unmute|Block|Unblock|Report|Send|Post|Remove|Add|Update|Edit|"
    r"Hide|Unhide|Vote|Subscribe|Unsubscribe|Join|Leave|Mark|Dm|DM)"
)
_PRIVATE_OPERATION = re.compile(r"(DM|Dm|Bookmark|Notification|Inbox|Conversation)")
_PRIVATE_REST = re.compile(r"(^|/)(dm|notifications|bookmarks?)(/|\.|$)")


@dataclass(frozen=True)
class Decision:
    allowed: bool
    reason: str


def _normalize_host(host: str) -> str:
    host = host.lower().rstrip(".")
    for prefix in ("www.", "mobile."):
        if host.startswith(prefix):
            return host[len(prefix) :]
    return host


def graphql_operation(url: str) -> str | None:
    """Operation name of an X GraphQL URL, or None when `url` is not one."""
    parts = urlsplit(url)
    host = _normalize_host(parts.hostname or "")
    segments = [segment for segment in parts.path.split("/") if segment]
    if host in _SITE_HOSTS and segments[:3] == ["i", "api", "graphql"]:
        rest = segments[3:]
    elif host in _API_HOSTS and segments[:1] == ["graphql"]:
        rest = segments[1:]
    else:
        return None
    # <hash>/<OpName>; the operation is always the last segment.
    return rest[-1] if len(rest) >= 2 else ""


def _rest_api_path(host: str, path: str) -> str | None:
    """Path below the REST API root (`1.1/...`, `2/...`, other `/i/api/...`)."""
    if host in _SITE_HOSTS and path.startswith("/i/api/"):
        return path[len("/i/api/") :]
    if host in _API_HOSTS and (path.startswith("/1.1/") or path.startswith("/2/")):
        return path[1:]
    return None


def decide(method: str, url: str, policy: Policy = "collect") -> Decision:
    """Decide whether the browser may send `method url`."""
    method = method.upper()
    operation = graphql_operation(url)
    if operation is not None:
        if policy == "login":
            if operation and not _WRITE_OPERATION.match(operation):
                return Decision(True, f"graphql {operation} (login)")
            return Decision(False, f"graphql {operation or '<none>'} is a write in login mode")
        if method != "GET":
            return Decision(False, f"{method} graphql {operation or '<none>'}")
        if not operation or _WRITE_OPERATION.match(operation):
            return Decision(False, f"graphql {operation or '<none>'} looks like a write")
        if _PRIVATE_OPERATION.search(operation):
            return Decision(False, f"graphql {operation} reads private content")
        return Decision(True, f"graphql {operation}")

    parts = urlsplit(url)
    host = _normalize_host(parts.hostname or "")
    if host in _UPLOAD_HOSTS and method != "GET":
        return Decision(False, f"{method} to upload host")
    rest_path = _rest_api_path(host, parts.path)
    if rest_path is not None and _PRIVATE_REST.search(rest_path.removeprefix("1.1/").removeprefix("2/")):
        return Decision(False, f"X API {rest_path} reads private content")
    if rest_path is not None and method != "GET":
        if policy == "login":
            tail = rest_path.removeprefix("1.1/")
            if tail.startswith(_LOGIN_REST_WRITE_PREFIXES):
                return Decision(True, f"{method} {rest_path} (login flow)")
        return Decision(False, f"{method} to X API {rest_path}")
    return Decision(True, "not an X write path")
