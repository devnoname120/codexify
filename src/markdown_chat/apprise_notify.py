import json
import logging
import sys
from urllib.parse import parse_qsl, urlencode, urlsplit, urlunsplit


def service_url(url):
    parts = urlsplit(url)
    keys = {key for key, _ in parse_qsl(parts.query, keep_blank_values=True)}
    defaults = {"format": "markdown", "overflow": "split", "redirect": "no"}
    extra = urlencode({key: value for key, value in defaults.items() if key not in keys})
    query = parts.query + ("&" if parts.query and extra else "") + extra
    return urlunsplit(parts._replace(query=query))


def send(payload):
    import apprise

    logging.disable(logging.CRITICAL)
    notifier = apprise.Apprise()
    urls = payload["urls"]
    if not urls or not all(notifier.add(service_url(url)) for url in urls):
        return False
    return notifier.notify(
        title=payload["title"],
        body=payload["body"],
        body_format=apprise.NotifyFormat.MARKDOWN,
    ) is True


def main():
    try:
        payload = json.loads(sys.stdin.buffer.read().decode("utf-8"))
        return 0 if send(payload) else 1
    except Exception:
        # Provider errors can contain credentials or message bodies.
        return 1


if __name__ == "__main__":
    sys.exit(main())
