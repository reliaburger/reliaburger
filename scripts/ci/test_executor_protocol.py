"""Native framing/child execution regressions; actual isolation runs in test-linux.

RB_EXECUTOR_HOST_FIXTURE compiles a separate fixture without clone3/mount setup.
The production embedded helper has no switch to disable isolation.
"""
import os
import pathlib
import socket
import struct
import subprocess
import sys
import tempfile
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[2]


def text(value):
    raw = value.encode()
    return struct.pack('!I', len(raw)) + raw


def command(sequence, argv, env=(), cwd='/'):
    data = struct.pack('!QIIII', sequence, len(argv), len(env), os.getuid(), os.getgid())
    data += text(cwd) + b''.join(map(text, argv)) + b''.join(map(text, env))
    return struct.pack('!I', len(data)) + data


def exact(connection, count):
    data = b''
    while len(data) < count:
        part = connection.recv(count - len(data))
        if not part:
            raise EOFError('executor ended before its receipt')
        data += part
    return data


class ExecutorProtocol(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.root = tempfile.TemporaryDirectory(prefix='rb-executor-protocol-')
        cls.binary = pathlib.Path(cls.root.name) / 'helper'
        subprocess.run(['cc', '-O2', '-std=c11', '-Wall', '-Wextra', '-Werror',
                        '-DRB_EXECUTOR_HOST_FIXTURE', str(ROOT / 'src/bun/reusable_executor/helper.c'),
                        '-o', str(cls.binary)], check=True, capture_output=True)
        if sys.platform == 'darwin':
            # macOS checks a newly written executable before its first launch.
            # Keep that cold-file check outside the socket receipt deadline.
            warm = subprocess.run([str(cls.binary)], capture_output=True, timeout=30)
            if warm.returncode != 125:
                raise RuntimeError(f'helper argument refusal failed: {warm.returncode}, {warm.stderr!r}')

    @classmethod
    def tearDownClass(cls):
        cls.root.cleanup()

    def setUp(self):
        self.fixture = tempfile.TemporaryDirectory(prefix='rb-executor-socket-')
        self.addCleanup(self.fixture.cleanup)
        self.path = str(pathlib.Path(self.fixture.name) / 'control')
        self.listener = socket.socket(socket.AF_UNIX)
        self.addCleanup(self.listener.close)
        self.listener.bind(self.path)
        self.listener.listen(1)
        self.listener.settimeout(2)
        self.process = subprocess.Popen([str(self.binary), self.path], stderr=subprocess.PIPE)
        self.addCleanup(self.stop_process)
        try:
            self.connection, _ = self.listener.accept()
        except socket.timeout as error:
            status = self.process.poll()
            if status is not None:
                detail = self.process.stderr.read().decode(errors='replace')
                raise RuntimeError(f'helper exited before handshake: {status}, {detail}') from error
            raise RuntimeError(f'helper {self.process.pid} did not connect within the receipt deadline') from error
        self.addCleanup(self.connection.close)
        self.connection.settimeout(2)
        self.assertEqual(exact(self.connection, 8), b'RBEX0001')
        directory = os.open(self.fixture.name, os.O_RDONLY)
        try:
            self.connection.sendmsg([b'F'], [(socket.SOL_SOCKET, socket.SCM_RIGHTS,
                                            struct.pack('i', directory))])
        finally:
            os.close(directory)

    def stop_process(self):
        try:
            self.process.wait(timeout=2)
        except subprocess.TimeoutExpired:
            self.process.kill()
            self.process.wait(timeout=2)
        self.process.stderr.close()

    def execute(self, sequence, argv, env=(), cwd='/'):
        self.connection.sendall(command(sequence, argv, env, cwd))
        self.assertEqual(exact(self.connection, 9), b"\x01" + struct.pack("!Q", sequence))
        output = {1: b'', 2: b''}
        while True:
            kind, received = struct.unpack('!BQ', exact(self.connection, 9))
            self.assertEqual(received, sequence)
            if kind == 2:
                stream, length = struct.unpack('!BI', exact(self.connection, 5))
                self.assertIn(stream, output)
                self.assertLessEqual(length, 4096)
                output[stream] += exact(self.connection, length)
            elif kind == 3:
                code, = struct.unpack('!i', exact(self.connection, 4))
                break
            else:
                self.fail(f'unexpected frame {kind}')
        self.connection.sendall(b'C' + struct.pack('!Q', sequence))
        self.assertEqual(exact(self.connection, 9), b'\x04' + struct.pack('!Q', sequence))
        return code, output

    def test_distinct_commands_exit_and_do_not_inherit_environment(self):
        code, output = self.execute(1, ['/bin/sh', '-c',
                                       'printf "%s" "$VALUE"; printf err >&2; exit 7'], ['VALUE=first'])
        self.assertEqual((code, output), (7, {1: b'first', 2: b'err'}))
        code, output = self.execute(2, ['/bin/sh', '-c', 'printf "%s" "${VALUE-unset}"'])
        self.assertEqual((code, output), (0, {1: b'unset', 2: b''}))

    def test_fast_exits_preserve_both_output_streams(self):
        for sequence in range(1, 1025):
            code, output = self.execute(sequence, ['/bin/sh', '-c',
                'printf finished; printf diagnostic >&2'])
            self.assertEqual((code, output), (0, {1: b'finished', 2: b'diagnostic'}))

    def test_duplicate_sequence_is_refused_without_executing(self):
        self.execute(1, ['/bin/true'])
        marker = pathlib.Path(self.fixture.name) / 'must-not-run'
        self.connection.sendall(command(1, ['/bin/sh', '-c', f'touch {marker}']))
        self.assertEqual(self.connection.recv(1), b'')
        self.assertFalse(marker.exists())

    def test_oversized_frame_is_refused_before_allocation(self):
        self.connection.sendall(struct.pack('!I', 65537))
        self.assertEqual(self.connection.recv(1), b'')

    def test_privileged_helper_uid_is_refused(self):
        data = struct.pack('!QIIII', 1, 1, 0, 65536, 0) + text('/') + text('/bin/true')
        self.connection.sendall(struct.pack('!I', len(data)) + data)
        self.assertEqual(self.connection.recv(1), b'')

    def test_embedded_nul_is_refused_without_executing(self):
        self.connection.sendall(command(1, ['/bin/true\0ignored']))
        self.assertEqual(self.connection.recv(1), b'')


@unittest.skipUnless(sys.platform == 'linux', 'host child ownership requires Linux')
class NativeHostExecutorProtocol(ExecutorProtocol):
    @classmethod
    def setUpClass(cls):
        cls.root = tempfile.TemporaryDirectory(prefix='rb-host-executor-protocol-')
        cls.binary = pathlib.Path(cls.root.name) / 'helper'
        subprocess.run(['cc', '-O2', '-std=c11', '-Wall', '-Wextra', '-Werror',
                        '-DRB_EXECUTOR_HOST', '-DRB_EXECUTOR_HOST_FIXTURE',
                        str(ROOT / 'src/bun/reusable_executor/helper.c'),
                        '-o', str(cls.binary)], check=True, capture_output=True)

    def test_detached_descendants_are_retired_before_the_slot_is_reused(self):
        import time
        marker = pathlib.Path(self.fixture.name) / 'escaped-command'
        code, _ = self.execute(1, ['/bin/sh', '-c',
            f'setsid /bin/sh -c "sleep 0.2; echo escaped > {marker}" & exit 0'])
        self.assertEqual(code, 0)
        self.assertEqual(self.execute(2, ['/bin/true'])[0], 0)
        time.sleep(0.35)
        self.assertFalse(marker.exists(), 'cleanup must retire children in new sessions too')
