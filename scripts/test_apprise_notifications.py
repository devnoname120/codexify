import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import unittest
from unittest.mock import patch
from urllib.parse import parse_qsl, urlsplit

import apprise
import requests

HELPER = Path(__file__).resolve().parents[1] / "src/markdown_chat/apprise_notify.py"
spec = importlib.util.spec_from_file_location("codexify_apprise", HELPER)
helper = importlib.util.module_from_spec(spec)
spec.loader.exec_module(helper)


class AppriseNotifications(unittest.TestCase):
    def setUp(self):
        self.calls = []

    def request(self, session, method, url, **kwargs):
        self.calls.append((method, url, kwargs))
        response = requests.Response()
        response.status_code = 500 if "fail.test" in url else 200
        response._content = b'{"status":1,"id":"test","event":"message"}'
        response.url = url
        return response

    def send(self, urls, body="**Complete Markdown**\n\nWith unicode: é 日本語"):
        with patch.object(requests.Session, "request", autospec=True, side_effect=self.request):
            return helper.send({"urls": urls, "title": "Codexify - test", "body": body})

    def test_real_ntfy_pushover_and_webhook_adapters(self):
        urls = [
            "ntfys://notify.test/project?image=no",
            "pover://" + "u" * 30 + "@" + "a" * 30,
            "jsons://hook.test/events",
        ]
        self.assertTrue(self.send(urls))
        self.assertEqual(len(self.calls), 3)
        ntfy = next(call for call in self.calls if "notify.test" in call[1])
        pushover = next(call for call in self.calls if "api.pushover.net" in call[1])
        webhook = next(call for call in self.calls if "hook.test" in call[1])
        self.assertIn("Complete Markdown", str(ntfy[2]))
        self.assertIn("Complete Markdown", str(pushover[2]))
        self.assertIn("Complete Markdown", str(webhook[2]))
        for _, _, kwargs in self.calls:
            self.assertFalse(kwargs.get("allow_redirects", True))

    def test_validation_finishes_before_any_send(self):
        self.assertFalse(self.send(["ntfys://notify.test/topic", "unknown-codexify-test://bad"]))
        self.assertEqual(self.calls, [])

    def test_partial_provider_failure_is_not_reported_as_success(self):
        self.assertFalse(self.send(["ntfys://notify.test/topic", "jsons://fail.test/events"]))
        self.assertEqual(len(self.calls), 2)

    def test_long_ntfy_messages_keep_the_end(self):
        body = "complete line of text\n" * 1800 + "FINAL SENTINEL"
        self.assertTrue(self.send(["ntfys://notify.test/topic?image=no"], body))
        self.assertGreater(len(self.calls), 1)
        self.assertIn("FINAL SENTINEL", str(self.calls[-1][2]))

    def test_user_options_survive_default_application(self):
        url = helper.service_url("ntfys://notify.test/topic?format=text&overflow=upstream&redirect=no")
        self.assertIn("format=text", url)
        self.assertIn("overflow=upstream", url)

    def test_duplicate_query_parameters_and_fragments_are_preserved(self):
        original = "jsons://hook.test/events?+x-one=first&+x-one=second&format=text#group"
        converted = urlsplit(helper.service_url(original))
        self.assertEqual(converted.fragment, "group")
        self.assertEqual(parse_qsl(converted.query)[:3], parse_qsl(urlsplit(original).query))

    def test_complete_source_markdown_is_passed_to_the_library(self):
        body = "\n## Source\n\n" + "    whitespace and 日本語\n" * 1000
        with patch.object(apprise.Apprise, "notify", return_value=True) as notify:
            self.assertTrue(helper.send({"urls":["ntfys://notify.test/topic"], "title":"Test", "body":body}))
        self.assertEqual(notify.call_args.kwargs["body"], body)
        self.assertEqual(notify.call_args.kwargs["body_format"], apprise.NotifyFormat.MARKDOWN)

    def test_invalid_stdin_and_provider_urls_do_not_leak_content(self):
        for data in [b"malformed SECRET", json.dumps({"urls": ["bad://SECRET"], "title": "test", "body": "PRIVATE"}).encode()]:
            result = subprocess.run([sys.executable, "-I", str(HELPER)], input=data, capture_output=True, timeout=10)
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(result.stdout, b"")
            self.assertEqual(result.stderr, b"")


if __name__ == "__main__":
    unittest.main()
