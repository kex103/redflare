#!/usr/bin/env python
import redis
from subprocess import call
import subprocess
import os
import sys
import time
import unittest
import socket
from test_util import TestUtil
from timeout_tests import TimeoutTests
from cluster_tests import ClusterTests
from admin_tests import AdminTests
from config_tests import ConfigTests
from sharding_tests import ShardingTests
from command_tests import CommandTests

class TestRedFlareProxy(TestUtil):

    def test_single_backend_no_timeout(self):
        self.start_redis_server(6380)
        self.start_proxy("tests/conf/testconfig1.toml")

        TestUtil.verify_redis_connection(1531)

    def test_no_backend_failure(self):
        # Spawn a proxy with no backend. Verify that it complains about invalid config.
        self.start_proxy("tests/conf/nobackend.toml")
        TestUtil.verify_redis_error(1533, expect_conn_error=True)

        # Then spawn a proxy pointing to an invalid backend. Verify the redis error should be "Not connected"
        self.start_proxy("tests/conf/timeout1.toml")
        TestUtil.verify_redis_error(1531, "ERROR: Not connected")

# Test successful, multiple (4) backends, no timeout. verify that the sharding is correct.
    def test_multiple_backend_no_timeout(self):
        self.start_redis_server(6381)
        self.start_redis_server(6382)
        self.start_redis_server(6383)
        self.start_redis_server(6384)
        self.start_proxy("tests/conf/multishard1.toml")

        TestUtil.verify_redis_connection(1533)

        TestUtil.populate_redis_key(6381, "key1")
        self.assert_redis_key(1533, "key1")
        TestUtil.populate_redis_key(6382, "key2")
        self.assert_redis_key(1533, "key2")

    def test_multiple_backend_with_timeout(self):
        # TODO: Set delay at 1 at first, then verify timeout when delay set to 101.
        self.start_redis_server(6381)
        self.start_redis_server(6380)
        self.start_redis_server(6383)
        self.start_redis_server(6384)
        self.start_delayer(6382, 6380, 101)
        self.start_proxy("tests/conf/multishard2.toml")

        TestUtil.verify_redis_connection(1533)

        TestUtil.populate_redis_key(6384, "key1")
        TestUtil.verify_redis_error(1533, "ERROR: Not connected")

    def test_hashtags(self):
        self.start_redis_server(6381)
        self.start_redis_server(6382)
        self.start_redis_server(6383)
        self.start_redis_server(6384)
        ports = [6381, 6382, 6383, 6384]
        self.start_proxy("tests/conf/multishardtags1.toml")
        TestUtil.verify_redis_connection(1533)

        TestUtil.populate_redis_key(1533, "key1")
        self.assert_redis_key(6381, "key1")
        TestUtil.populate_redis_key(1533, "key4")
        self.assert_redis_key(6384, "key4")

        # Verify single hashtag doesn't work.
        TestUtil.populate_redis_key(1533, "/key4")
        self.assert_redis_key(6381, "/key4")
        TestUtil.populate_redis_key(1533, "key/4")
        self.assert_redis_key(6381, "key/4")
        TestUtil.populate_redis_key(1533, "key4/")
        self.assert_redis_key(6383, "key4/")

        # Verify that // corresponds to the same hash.
        TestUtil.populate_redis_key(1533, "key4//")
        self.assert_redis_key(6383, "key4//")
        TestUtil.populate_redis_key(1533, "key4///")
        self.assert_redis_key(6383, "key4///")
        TestUtil.populate_redis_key(1533, "//key4", "teste")
        self.assert_redis_key(6383, "//key4", "teste")

        # Verify that /4/ corresponds to the same hash.
        TestUtil.populate_redis_key(1533, "4", "/value534")
        self.assert_redis_key(6381, "4", "/value534")
        TestUtil.populate_redis_key(1533, "key/4/", "/value5")
        self.assert_redis_key(6381, "key/4/", "/value5")
        TestUtil.populate_redis_key(1533, "adaerr/4/", "/value2")
        self.assert_redis_key(6381, "adaerr/4/", "/value2")

        # TODO: Verify hashtag pairs.

        # TODO: Verify that more than 2 chars in a hashtag is invalid.

    def test_auth(self):
        # Start a server with auth.password required.
        self.start_redis_server(6381, password="password1")
        self.start_delayer(incoming_port=6380, outgoing_port=6381, delay=1, admin_port=6382)

        # 1. Verify that with config with the correct auth config can access the server.
        self.start_proxy("tests/conf/auth1.toml", tag=1)
        TestUtil.verify_redis_connection(1531)

        # 2. Verify that with right config, the server can be disconnected, reconnected, and will still work.
        conn_to_delayer1 = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        conn_to_delayer1.connect(("0.0.0.0", 6382))
        conn_to_delayer1.sendall("SETDELAY 400")

        TestUtil.verify_redis_error(1531, "Proxy timed out")
        TestUtil.verify_redis_error(1531, "ERROR: Not connected")

        conn_to_delayer1.sendall("SETDELAY 1")
        time.sleep(1.5)
        TestUtil.verify_redis_connection(1531)

        # 3. Verify that with the right config, the proxy can be reconfigured to the wrong one, and will not work.
        r = redis.Redis(port=1530)
        response = r.execute_command("LOADCONFIG tests/conf/auth2.toml")
        self.assertTrue(response)
        response = r.execute_command("SWITCHCONFIG")
        self.assertTrue(response)
        TestUtil.verify_redis_error(1531, "ERROR: Not connected")

        response = r.execute_command("SHUTDOWN")
        self.assertTrue(response);

        # 4. Verify that without config, the server is considered down (unauthorized)
        self.start_proxy("tests/conf/testconfig1.toml", tag=2)
        TestUtil.verify_redis_error(1531, "NOAUTH Authentication required.")

        # 5. Verify that with no config, the proxy can be reconfigured to the correct one,and will work.
        r = redis.Redis(port=1530)
        response = r.execute_command("LOADCONFIG tests/conf/auth1.toml")
        self.assertTrue(response)
        response = r.execute_command("SWITCHCONFIG")
        self.assertTrue(response)
        time.sleep(1)
        TestUtil.verify_redis_connection(1531)

    def test_db(self):
        self.start_redis_server(6380)
        self.start_redis_server(6382)
        delayer = self.start_delayer(incoming_port=6381, outgoing_port=6380, delay=0, admin_port=6383)
        ports = [6380, 6382]
        r1 = redis.Redis(port=6380)
        r2 = redis.Redis(port=6380, db=1)
        r3 = redis.Redis(port=6380, db=2)

        # 1. Verify that db is selected properly.
        self.start_proxy("tests/conf/db1.toml")

        TestUtil.populate_redis_key(1531, "key1")
        TestUtil.populate_redis_key(1531, "key2")
        TestUtil.populate_redis_key(1531, "key3")
        TestUtil.populate_redis_key(1531, "key4")
        TestUtil.populate_redis_key(1531, "key5")

        response = r1.execute_command("DBSIZE")
        self.assertEquals(response, 0)
        response = r2.execute_command("DBSIZE")
        self.assertEquals(response, 3)
        response = r3.execute_command("DBSIZE")
        self.assertEquals(response, 0)

        TestUtil.flush_keys(ports)

        # 2. Verify switching configs with a different db.
        r = redis.Redis(port=1530)
        response = r.execute_command("LOADCONFIG tests/conf/db2.toml")
        self.assertTrue(response)
        response = r.execute_command("SWITCHCONFIG")
        self.assertTrue(response)

        TestUtil.populate_redis_key(1531, "key1")
        TestUtil.populate_redis_key(1531, "key2")
        TestUtil.populate_redis_key(1531, "key3")
        TestUtil.populate_redis_key(1531, "key4")
        TestUtil.populate_redis_key(1531, "key5")

        response = r1.execute_command("DBSIZE")
        self.assertEquals(response, 0)
        response = r2.execute_command("DBSIZE")
        self.assertEquals(response, 0)
        response = r3.execute_command("DBSIZE")
        self.assertEquals(response, 3)

        TestUtil.flush_keys(ports)

        # 3. Verify that disconnecting and reconnecting results in same db being used.
        delayer.sendall("SETDELAY 400")
        TestUtil.verify_redis_error(1531, "Proxy timed out", key="key1")
        TestUtil.verify_redis_error(1531, "ERROR: Not connected", key="key1")
        delayer.sendall("SETDELAY 0")
        time.sleep(2)

        TestUtil.populate_redis_key(1531, "key1")
        TestUtil.populate_redis_key(1531, "key2")
        TestUtil.populate_redis_key(1531, "key3")
        TestUtil.populate_redis_key(1531, "key4")
        TestUtil.populate_redis_key(1531, "key5")

        response = r1.execute_command("DBSIZE")
        self.assertEquals(response, 0)
        response = r2.execute_command("DBSIZE")
        self.assertEquals(response, 0)
        response = r3.execute_command("DBSIZE")
        self.assertEquals(response, 3)

# Test multiple backends, 
if __name__ == "__main__":
    TestRedFlareProxy.setupClass()
    unittest.main()