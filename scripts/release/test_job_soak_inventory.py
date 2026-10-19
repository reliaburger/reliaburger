"""Kernel objects need exact private ownership evidence, never a name exemption."""
import json
import os
from pathlib import Path
import tempfile
import unittest

import job_soak_inventory as inventory


class Ownership(unittest.TestCase):
    def setUp(self):
        self.temp=tempfile.TemporaryDirectory(); self.addCleanup(self.temp.cleanup)
        self.root=Path(self.temp.name); self.prefix="a"*32
        self.identity="default__executor-"+self.prefix+"-reuse-0"
        self.path=self.root/"record.json"
    def write(self, **changes):
        record=dict(instance_id=self.identity,boot_id="boot",generation="b"*32,phase={"state":"owned"},
                    spec={"linux":{"cgroupsPath":"/reliaburger/default/executor-"+self.prefix+"-reuse/0"}})
        record.update(changes); self.path.write_text(json.dumps(record)); self.path.chmod(0o600)
        return record
    def test_exact_owned_generation_with_same_boot_is_recognised(self):
        self.write()
        record=inventory.safe_json(self.path,self.root,os.geteuid())
        proof=inventory.owner_proof(record,self.identity,self.prefix,"boot")
        self.assertEqual(proof["id"],self.identity)
        self.assertNotIn("spec",proof)
    def test_prefix_is_not_authority_without_boot_generation_and_ownership(self):
        for change in ({"boot_id":"previous"},{"generation":""},{"phase":{"state":"retired"}},{"instance_id":"other"}):
            self.write(**change)
            with self.subTest(change=change), self.assertRaises(inventory.InvalidInventory):
                inventory.owner_proof(inventory.safe_json(self.path,self.root,os.geteuid()),self.identity,self.prefix,"boot")
    def test_refuses_symlinks_hardlinks_writable_records_and_unsafe_parents(self):
        self.write(); link=self.root/"link"; link.symlink_to(self.path)
        with self.assertRaises(inventory.InvalidInventory): inventory.safe_json(link,self.root,os.geteuid())
        link.unlink(); os.link(self.path,link)
        with self.assertRaises(inventory.InvalidInventory): inventory.safe_json(self.path,self.root,os.geteuid())
        link.unlink(); self.path.chmod(0o666)
        with self.assertRaises(inventory.InvalidInventory): inventory.safe_json(self.path,self.root,os.geteuid())
        self.path.chmod(0o600); self.root.chmod(0o777)
        with self.assertRaises(inventory.InvalidInventory): inventory.safe_json(self.path,self.root,os.geteuid())
    def test_other_namespace_or_out_of_pool_slot_cannot_be_exempted(self):
        for identity in (self.identity.replace("default__","foreign__"),self.identity[:-1]+"99", "default__web-0"):
            with self.assertRaises(inventory.InvalidInventory):
                inventory.owner_proof(self.write(instance_id=identity),identity,self.prefix,"boot")
    def test_native_owner_and_cgroup_identity_must_agree(self):
        host=self.identity.replace("reuse","host")
        record=dict(boot_id="boot",nonce="c"*32,phase={"state":"running","pid":42},
                    launch={"instance_id":host,"spec":{"linux":{"cgroupsPath":"/reliaburger/default/executor-"+self.prefix+"-host/0/helper"}}})
        proof=inventory.owner_proof(record,host,self.prefix,"boot")
        self.assertEqual(proof["runtime"],"process")
        self.assertNotIn("nonce",proof)
        record["launch"]["spec"]["linux"]["cgroupsPath"]="/foreign"
        with self.assertRaises(inventory.InvalidInventory): inventory.owner_proof(record,host,self.prefix,"boot")
    def test_hashes_veth_names_the_same_way_as_runtime(self):
        self.assertEqual(inventory.veth("a"),"veth-a-h")
        self.assertEqual(len(inventory.veth(self.identity)),15)

    def test_native_helper_binds_its_real_launch_cgroup_despite_unused_oci_path(self):
        host=self.identity.replace("reuse","host")
        record=dict(boot_id="boot",nonce="c"*32,phase={"state":"running","pid":42},
                    launch={"instance_id":host,"spec":{"linux":{"cgroupsPath":"/unused"},
                    "process":{"args":["/data/instances/process-owners/host-executors/"+host+"/helper","/private/socket",
                    "/sys/fs/cgroup/reliaburger/default/executor-"+self.prefix+"-host/0/helper/cgroup.procs"]}}})
        self.assertEqual(inventory.owner_proof(record,host,self.prefix,"boot")["runtime"],"process")
        record["launch"]["spec"]["process"]["args"][2]="/foreign/cgroup.procs"
        with self.assertRaises(inventory.InvalidInventory): inventory.owner_proof(record,host,self.prefix,"boot")


    def test_native_and_container_owners_cannot_cross_runtime_pools(self):
        host=self.identity.replace("reuse","host")
        with self.assertRaises(inventory.InvalidInventory):
            inventory.owner_proof(self.write(instance_id=host),host,self.prefix,"boot")
        fresh=self.identity.replace("-reuse","")
        native=dict(boot_id="boot",nonce="c"*32,phase={"state":"running","pid":42},
                    launch={"instance_id":fresh,"spec":{"linux":{"cgroupsPath":"/reliaburger/default/executor-"+self.prefix+"/0"}}})
        with self.assertRaises(inventory.InvalidInventory): inventory.owner_proof(native,fresh,self.prefix,"boot")


class Records(unittest.TestCase):
    """Why a record can't be read must survive into the finding (#946)."""
    def setUp(self):
        self.temp=tempfile.TemporaryDirectory(); self.addCleanup(self.temp.cleanup)
        self.root=Path(self.temp.name); self.path=self.root/"record.json"
    def test_a_missing_record_is_unavailable_not_invalid_json(self):
        with self.assertRaises(inventory.InvalidInventory) as caught:
            inventory.safe_json(self.path,self.root,os.geteuid())
        self.assertNotIn("invalid owner JSON",str(caught.exception))
        self.assertIn("unavailable",str(caught.exception))
    def test_an_unsafe_record_keeps_its_reason(self):
        self.path.write_text("{}"); self.path.chmod(0o666)
        with self.assertRaises(inventory.InvalidInventory) as caught:
            inventory.safe_json(self.path,self.root,os.geteuid())
        self.assertEqual(str(caught.exception),"unsafe owner record")
    def test_an_unparseable_record_reports_its_path_size_and_edges(self):
        body=b'{"allocations":{"default__soak-writer-0":'+b"7"*100
        self.path.write_bytes(body); self.path.chmod(0o600)
        with self.assertRaises(inventory.InvalidInventory) as caught:
            inventory.safe_json(self.path,self.root,os.geteuid())
        message=str(caught.exception)
        self.assertTrue(message.startswith("invalid owner JSON"),message)
        self.assertIn("record.json",message)
        self.assertIn("%d bytes"%len(body),message)
        self.assertIn(repr(body[:16]),message)
        self.assertIn(repr(body[-16:]),message)
    def test_an_empty_record_says_it_is_empty(self):
        self.path.write_bytes(b""); self.path.chmod(0o600)
        with self.assertRaises(inventory.InvalidInventory) as caught:
            inventory.safe_json(self.path,self.root,os.geteuid())
        self.assertIn("0 bytes",str(caught.exception))


class Storage(unittest.TestCase):
    def setUp(self):
        self.temp=tempfile.TemporaryDirectory(); self.addCleanup(self.temp.cleanup)
        self.root=Path(self.temp.name)
        (self.root/"a").mkdir(); (self.root/"a"/"f").write_bytes(b"x"*65536)
        (self.root/"g").write_bytes(b"y"*8192)
    def test_sizes_a_tree_in_kib_like_du(self):
        expected=sum(os.lstat(p).st_blocks for p in [self.root,self.root/"a",self.root/"a"/"f",self.root/"g"])*512//1024
        self.assertEqual(inventory.storage_kb(self.root),expected)
    def test_a_missing_tree_uses_no_space(self):
        self.assertEqual(inventory.storage_kb(self.root/"absent"),0)
    def test_counts_a_hard_linked_file_once(self):
        before=inventory.storage_kb(self.root)
        os.link(self.root/"g",self.root/"a"/"g2")
        self.assertEqual(inventory.storage_kb(self.root),before)
    def test_an_entry_removed_during_the_walk_is_skipped(self):
        # Jobs remove files under instances/ while the walk runs (#961).
        from unittest import mock
        real=os.lstat
        def vanishing(path,*args,**kwargs):
            if str(path).endswith("/g"): raise FileNotFoundError(2,"No such file or directory",str(path))
            return real(path,*args,**kwargs)
        with mock.patch.object(inventory.os,"lstat",side_effect=vanishing):
            self.assertGreater(inventory.storage_kb(self.root),0)
    def test_other_errors_fail_and_name_the_path(self):
        from unittest import mock
        real=os.lstat
        def denied(path,*args,**kwargs):
            if str(path).endswith("/f"): raise PermissionError(13,"Permission denied",str(path))
            return real(path,*args,**kwargs)
        with mock.patch.object(inventory.os,"lstat",side_effect=denied):
            with self.assertRaisesRegex(inventory.InvalidInventory,"/a/f"):
                inventory.storage_kb(self.root)


class Collection(unittest.TestCase):
    def setUp(self):
        self.temp=tempfile.TemporaryDirectory(); self.addCleanup(self.temp.cleanup)
        self.root=Path(self.temp.name); self.prefix="a"*32
        (self.root/"batch-executor-id").write_text(self.prefix)
        (self.root/"batch-executor-id").chmod(0o600)
        self.root.chmod(0o700)
        self.empty=dict(runc=[],netns=[],veth=[],cgroup=[],lease=[])
    def collect(self, kernels):
        from unittest import mock
        with mock.patch.object(inventory,"kernel",side_effect=kernels), \
             mock.patch.object(inventory,"boot_id",return_value="boot"), \
             mock.patch.object(inventory,"storage_kb",return_value=0), \
             mock.patch.object(inventory.time,"sleep") as sleep:
            value=inventory.collect(self.root)
        self.pauses=sleep.call_count
        return value
    def publish_host_owner(self, slot):
        identity="default__executor-"+self.prefix+"-host-"+str(slot)
        directory=self.root/"instances/process-owners"/identity
        directory.mkdir(parents=True,mode=0o700)
        record=dict(boot_id="boot",nonce="c"*32,phase={"state":"prepared"},
                    launch={"instance_id":identity,"spec":{"linux":{"cgroupsPath":"/reliaburger/default/executor-"+self.prefix+"-host/"+str(slot)+"/helper"}}})
        path=directory/"owner.json"; path.write_text(json.dumps(record)); path.chmod(0o600)
    def test_a_slot_cgroup_without_an_owner_record_is_named_as_such(self):
        # A killed Bun's executor was retired and forgotten, but its slot's
        # parent cgroup stayed (#952). There is no record to be invalid.
        leftover=dict(self.empty,cgroup=["default/executor-"+self.prefix+"-reuse/3"])
        value=self.collect([leftover]*(2*inventory.ATTEMPTS))
        self.assertEqual(value["errors"],["default__executor-"+self.prefix+"-reuse-3: resources without an owner record"])
    def test_a_host_slot_whose_owner_record_is_published_after_the_quick_retries_is_not_reported(self):
        # Bun makes a cold host slot's cgroup before it publishes the slot's
        # owner record, and on a busy node that gap lasts seconds (#980).
        starting=dict(self.empty,cgroup=["default/executor-"+self.prefix+"-host/3"])
        scans=[]
        def kernel(root):
            scans.append(root)
            if len(scans)==7: self.publish_host_owner(3)
            return starting
        value=self.collect(kernel)
        self.assertEqual(value["errors"],[])
        self.assertEqual([owner["id"] for owner in value["owners"]],["default__executor-"+self.prefix+"-host-3"])
        self.assertGreater(self.pauses,0)
    def test_a_host_slot_cgroup_that_never_gets_a_record_fails_after_the_grace(self):
        leftover=dict(self.empty,cgroup=["default/executor-"+self.prefix+"-host/3"])
        value=self.collect([leftover]*(2*inventory.ATTEMPTS))
        self.assertEqual(value["errors"],["default__executor-"+self.prefix+"-host-3: resources without an owner record"])
        self.assertEqual(self.pauses,inventory.ATTEMPTS-1)
        self.assertGreaterEqual(self.pauses*inventory.PAUSE_SECONDS,5)
    def test_a_clean_inventory_does_not_wait(self):
        value=self.collect([self.empty]*2)
        self.assertEqual(value["errors"],[])
        self.assertEqual(self.pauses,0)
    def test_an_unreadable_lease_journal_is_retried_like_owner_records(self):
        value=self.collect([inventory.InvalidInventory("invalid owner JSON"),self.empty,self.empty])
        self.assertTrue(value["complete"])
        self.assertEqual(value["errors"],[])
    def test_a_lease_journal_unreadable_on_every_attempt_fails_the_inventory(self):
        with self.assertRaises(inventory.InvalidInventory):
            self.collect([inventory.InvalidInventory("invalid owner JSON")]*(2*inventory.ATTEMPTS))
