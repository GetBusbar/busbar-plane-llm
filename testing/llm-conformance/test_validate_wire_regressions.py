import importlib.util
from pathlib import Path
import struct
import sys
import unittest
import zlib

spec = importlib.util.spec_from_file_location('llm_validate_wire', Path(__file__).with_name('validate.py'))
v = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = v
spec.loader.exec_module(v)


def frame(headers=b'', payload=b'', hlen=None):
    prelude = struct.pack('>II', 16 + len(headers) + len(payload), len(headers) if hlen is None else hlen)
    body = prelude + struct.pack('>I', zlib.crc32(prelude)) + headers + payload
    return body + struct.pack('>I', zlib.crc32(body))


class WireRegressions(unittest.TestCase):
    def test_non_string_discriminator_values_are_violations(self):
        schema = {
            'oneOf': [{'$ref': '#/definitions/branch'}],
            'discriminator': {'propertyName': 'type', 'mapping': {'known': '#/definitions/branch'}},
            'definitions': {'branch': {'type': 'object', 'properties': {'type': {'const': 'known'}}}},
        }
        checker = v.Checker(schema)
        for value in ([], {}, ['known'], {'type': 'known'}, 1, True):
            with self.subTest(value=value):
                out = []
                checker.check({'type': value}, schema, '', out)
                self.assertEqual([violation.rule for violation in out], ['discriminator'])
        out = []
        checker.check({'type': 'known'}, schema, '', out)
        self.assertEqual(out, [])

    def test_unterminated_sse_is_discarded(self):
        self.assertEqual(v.parse_sse('data: {}\n\ndata: [DONE]\n'), [(0, None, '{}')])

    def test_sse_line_endings(self):
        for ending in ('\r', '\n', '\r\n'):
            with self.subTest(ending=repr(ending)):
                self.assertEqual(v.parse_sse(f'data: a{ending}data: b{ending}{ending}'), [(0, None, 'a\nb')])

    def test_event_only_sse_is_ignored(self):
        self.assertEqual(v.parse_sse('event: keepalive\n\ndata:\n\n'), [(0, None, '')])

    def test_header_bounds(self):
        malformed = [frame(hlen=100), frame(b'\x05ab'), frame(b'\x01a'), frame(b'\x01a\x07\x00'), frame(b'\x01a\x07\x00\x05x')]
        malformed += [frame(b'\x01a' + bytes([kind])) for kind in (2, 3, 4, 5, 6, 8, 9)]
        for raw in malformed:
            with self.subTest(raw=raw):
                frames = v.parse_eventstream(raw + frame())
                self.assertTrue(frames[0]['error'])
                self.assertEqual(len(frames), 2)
                self.assertIsNone(frames[1]['error'])

    def test_valid_headers(self):
        self.assertEqual(v.parse_eventstream(frame(b'\x01a\x07\x00\x01x', b'{}'))[0]['headers'], {'a': 'x'})

    def test_non_json_constants(self):
        judge = object.__new__(v.Judge)
        judge.checker = lambda dialect: self.fail('invalid JSON reached schema checker')
        for token in ('NaN', 'Infinity', '-Infinity'):
            raw = ('{"temperature":' + token + '}').encode()
            with self.subTest(token=token):
                self.assertIsNotNone(v.Judge.parse_json(raw)[1])
                # request obtains the checker before decoding; use a harmless stub.
                judge.checker = lambda dialect: None
                self.assertEqual(judge.judge_request('gemini', 'success', raw)[0][0].rule, 'body.json')
                out = []
                judge.judge_sse('gemini', b'data: ' + raw + b'\n\n', out)
                self.assertEqual(out[0].rule, 'sse.data')


if __name__ == '__main__':
    unittest.main()
