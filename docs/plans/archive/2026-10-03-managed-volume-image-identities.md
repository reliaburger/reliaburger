# Preserve managed volume identities

Issue: #528.

Write failing overlap and dotted-image regression tests first. Append `.img` to the complete volume name and use it for provision, remount and owned retirement. Check each app's complete managed-volume and restore-artifact layout at configuration admission and again before agent provisioning. Add a Linux sentinel/remount regression, explain the storage identity in Chapter 5, and run `make ci`.

Backing image naming changes the durable storage format. The release train coordinates the durable-state generation bump; no old-name fallback or migration is provided before 1.0.

Linux mount/format qualification requires root and loop devices and cannot run on this macOS host.
