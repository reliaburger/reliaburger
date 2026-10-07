# Distinct managed volume paths can share a loop image and reformat each other’s data

Suggested priority: **P1**. Affects **v0.1.4**; verified at `f4757e7789d3672d21f15ca203031d6604d7e11a`.

Verification: Validation/path reproduced; Linux formatting path verified.

### Problem


On rootful Linux with ext4/xfs managed volumes, `/data.a` and `/data.b` are valid distinct volume mount paths, but `path.with_extension("img")` maps both to the same host `data.img`. Creating the second volume runs `fallocate` and `mkfs.ext4 -F` on the first volume's backing image. This can corrupt/reformat its data, then either mount the same filesystem again or fail after damage has already occurred.

Even `/data` and `/data.backup` collide. Test-owned volume provisioning already detects overlapping artifact paths, but ordinary production app provisioning does not run that check.

### Reproduction / verification


`Config::validate` accepts an app with these two managed volumes:

```toml
[app.db]
image = "busybox"
[[app.db.volumes]]
path = "/data.a"
size = "128Mi"
[[app.db.volumes]]
path = "/data.b"
size = "128Mi"
```

Executed the current-library pure probe: `colliding_volumes_validate=true`, `/data.a -> /data.img`, `/data.b -> /data.img`. The filesystem formatting call chain was checked, but **no destructive Linux mount/format repro was run** on this macOS host.

### Evidence


- [src/config/validate.rs:335–370](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/config/validate.rs#L335-L370): volume paths validated independently; no artifact collision rejection.
- [src/bun/agent/volumes.rs:104–110](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/agent/volumes.rs#L104-L110): ordinary namespace provisions every volume independently.
- [src/grill/volume.rs:311](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/grill/volume.rs#L311), `383`: backing image path replaces the final extension.
- [src/grill/volume.rs:314–350](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/grill/volume.rs#L314-L350): allocation, forced format and mount act on that path.
- [src/grill/volume/owned.rs:59–77](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/grill/volume/owned.rs#L59-L77): existing overlap defense limited to test-owned storage.

### Fix / acceptance


Derive backing artifact names injectively (append `.img`, or encode the complete path), and validate all volume/artifact overlaps before any provisioning. Include ordinary namespaces, parent/child volumes, dotted names, and an image-artifact path used as a mountpoint. Linux integration test: write a sentinel to one sized volume, provision another with a formerly-colliding name, and verify independent filesystems/data plus correct reboot remounts. A durable format change may require a compatibility generation bump under the repository's pre-1.0 rules.

### Current implementation snippets

The excerpts below are verbatim from the audited checkout. Links are pinned to that commit.

[src/grill/volume.rs:305–324](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/grill/volume.rs#L305-L324)

```rust
    /// Creates a sparse file, formats it with ext4, and loop-mounts it.
    /// Writes beyond the quota fail with ENOSPC.
    #[cfg(target_os = "linux")]
    fn setup_loop_mount(&self, path: &Path, size_bytes: u64) -> Result<(), VolumeError> {
        use std::process::Command;

        let img_path = path.with_extension("img");

        // Create sparse file
        let status = Command::new("fallocate")
            .args(["-l", &size_bytes.to_string()])
            .arg(&img_path)
            .status()
            .map_err(|e| VolumeError::CreateFailed {
                path: path.display().to_string(),
                reason: format!("fallocate: {e}"),
            })?;
        if !status.success() {
            return Err(VolumeError::CreateFailed {
                path: path.display().to_string(),
```

[src/grill/volume.rs:329–343](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/grill/volume.rs#L329-L343)

```rust
        // Format with ext4
        let status = Command::new("mkfs.ext4")
            .args(["-F", "-q"])
            .arg(&img_path)
            .status()
            .map_err(|e| VolumeError::CreateFailed {
                path: path.display().to_string(),
                reason: format!("mkfs.ext4: {e}"),
            })?;
        if !status.success() {
            return Err(VolumeError::CreateFailed {
                path: path.display().to_string(),
                reason: "mkfs.ext4 failed".to_string(),
            });
        }
```

[src/bun/agent/volumes.rs:85–101](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/agent/volumes.rs#L85-L101)

```rust
        let provisioning = async move {
            tokio::task::spawn_blocking(move || {
                if crate::testkit::lease::valid_test_namespace(&namespace) {
                    manager.prepare_test_storage(&namespace, &app, &spec)?;
                } else {
                    for volume in spec.volumes.iter().filter(|volume| volume.source.is_none()) {
                        manager.create_managed_volume(
                            &namespace,
                            &app,
                            &volume.path,
                            volume.size.as_deref(),
                        )?;
                    }
                }
                Ok::<(), crate::grill::volume::VolumeError>(())
            })
            .await
```
