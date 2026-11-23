# Plan: Raspberry Pi 4 and 5 as an appliance platform

*Written 16 November 2026, after the first Wyse night. Issue: [#1971](https://github.com/reliaburger/reliaburger/issues/1971), milestone Later. It builds on the appliance as 0.3.0 ships it: [the product plan](2026-10-01-plan-appliance-product.md), the design doc `docs/design/appliance.md` and the book's chapter 15a. Those files arrive on `main` with the 0.3.0 train (#490), so a few links here resolve only after it merges.*

## Why

Many people who want to try a bare-metal Reliaburger cluster own Raspberry Pis, not spare PCs. Today the appliance can't run on one, and since 0.3.0 `relish netboot` says so: it names a Pi's network boot request and logs that Pis aren't supported yet. The manual and the features page say the same.

The appliance is UEFI-only. It boots systemd-boot and unified kernel images (UKIs), keeps `/usr` in A/B slots verified by dm-verity, and falls back after a bad update through systemd-boot's boot counting. It runs Ubuntu's generic kernel. A Pi has none of that. It boots through its own firmware, Ubuntu supports it on a separate kernel, and its A/B mechanism is the firmware's `tryboot`. This plan is how the Pi 4 and Pi 5 become a second boot platform without forking the appliance.

## What we found

Checked on 16 November 2026. Raspberry Pi documentation links are pinned to [documentation@c6a6496](https://github.com/raspberrypi/documentation/tree/c6a64966af136da21a1e55551c5a395cb6987fce/documentation/asciidoc/computers), and bootloader release notes to [rpi-eeprom@3831e51](https://github.com/raspberrypi/rpi-eeprom/tree/3831e51dba65f4493f3f88c1d049de73165c082e).

**A Pi doesn't network boot out of the box.** A new Pi 4's EEPROM leaves `BOOT_ORDER` at its default, `0xf41`: SD card, then USB, then repeat. A new Pi 5's EEPROM sets `0xf461`: SD, NVMe, USB. Neither tries the network until someone changes the boot order, from a booted Pi (`rpi-eeprom-config`, `raspi-config`) or with Raspberry Pi Imager's bootloader image on an SD card ([the boot order table](https://github.com/raspberrypi/documentation/blob/c6a64966af136da21a1e55551c5a395cb6987fce/documentation/asciidoc/computers/raspberry-pi/eeprom-bootloader.adoc), [enabling network boot](https://www.raspberrypi.com/documentation/computers/remote-access.html#network-boot-your-raspberry-pi)). A Wyse falls through to PXE on a blank disk by itself; a Pi never does. The "network install" both models have at power-on boots Raspberry Pi's own signed Imager over HTTPS, not ours.

**Its network boot is PXE in name only.** The bootloader's DISCOVER says `PXEClient:Arch:00000:UNDI:002001` in option 60 and architecture 0 in option 93 ([boot-net.adoc](https://github.com/raspberrypi/documentation/blob/c6a64966af136da21a1e55551c5a395cb6987fce/documentation/asciidoc/computers/raspberry-pi/boot-net.adoc)). It wants option 43 to hold a PXE boot menu whose entry reads "Raspberry Pi Boot", the encoding dnsmasq's `pxe-service=0,"Raspberry Pi Boot"` produces. It then fetches files by name over TFTP: `config.txt`, the firmware (`start4.elf` and `fixup4.dat` on a Pi 4; a Pi 5 has its firmware in the EEPROM), a device tree, the kernel, `cmdline.txt` and an initramfs, first under a directory named after its serial number ([nobodd's walkthrough](https://nobodd.readthedocs.io/en/latest/explain-pi-netboot.html)). No iPXE, no UEFI. A ProxyDHCP beside the home router works; Raspberry Pi's own tutorial runs dnsmasq in proxy mode. TFTP `blksize` up to 1468 and `tsize` probes are supported, which is exactly what relish's TFTP server already does.

**Pi 5 HTTP boot needs our key in every Pi.** The Pi 5 (and a Pi 4 with a 2022 or later bootloader) can fetch a `boot.img` over HTTP, but "all HTTP downloads must be signed", against a public key written into the EEPROM. A public EEPROM update wipes that key ([boot-http.adoc](https://github.com/raspberrypi/documentation/blob/c6a64966af136da21a1e55551c5a395cb6987fce/documentation/asciidoc/computers/raspberry-pi/boot-http.adoc)). TFTP needs no signing.

**`tryboot` is the Pi's boot counting, with a watchdog for hangs.** `autoboot.txt` on the first FAT partition names a boot partition and a `[tryboot]` one; `reboot '0 tryboot'` boots the second once. The flag is cleared before the firmware starts, so a crash or reset brings back the old partition ([autoboot.adoc](https://github.com/raspberrypi/documentation/blob/c6a64966af136da21a1e55551c5a395cb6987fce/documentation/asciidoc/computers/config_txt/autoboot.adoc), [bootflow-eeprom.adoc](https://github.com/raspberrypi/documentation/blob/c6a64966af136da21a1e55551c5a395cb6987fce/documentation/asciidoc/computers/raspberry-pi/bootflow-eeprom.adoc)). A hang isn't a reset, so a hung trial boot needs the hardware watchdog: `kernel_watchdog_timeout` in `config.txt` hands it to Linux, and systemd's `RuntimeWatchdogSec` feeds it ([boot.adoc](https://github.com/raspberrypi/documentation/blob/c6a64966af136da21a1e55551c5a395cb6987fce/documentation/asciidoc/computers/config_txt/boot.adoc)). Commit is rewriting `autoboot.txt`. Since 25.10 Ubuntu itself uses `tryboot` for its kernels, but not its root filesystem ([piboot-try](https://ubuntu.com/hardware/docs/boards/explanations/piboot-ab/)).

**Ubuntu 26.04 still has a separate Pi kernel.** `linux-raspi` 7.0 is in resolute, and the generic arm64 kernel lacks the Pi's SD controllers, the Pi 4's PCIe controller and the device trees ([Launchpad](https://api.launchpad.net/1.0/ubuntu/+archive/primary?ws.op=getPublishedSources&source_name=linux-raspi&distro_series=https://api.launchpad.net/1.0/ubuntu/resolute)). The firmware blobs are in `linux-firmware-raspi` ([file list](https://packages.ubuntu.com/resolute/arm64/linux-firmware-raspi/filelist)). Ubuntu boots Pis straight from the Pi firmware and dropped U-Boot from classic images in 20.10 ([the change](https://code.launchpad.net/~waveform/ubuntu-release-upgrader/+git/ubuntu-release-upgrader/+merge/406663)). A Pi 5 needs a bootloader from late 2025 to boot 26.04's kernel ([LP: #2146383](https://bugs.launchpad.net/bugs/2146383)).

**Making the Pi look like UEFI only half works.** The community UEFI firmware for the Pi 4, [pftf/RPi4](https://github.com/pftf/RPi4), is maintained, but it limits RAM to 3 GB by default and needs drivers the generic kernel lacks. The Pi 5's, [worproject/rpi5-uefi](https://github.com/worproject/rpi5-uefi), is archived and doesn't boot current boards. Sidero's Pi 4 guide takes the pftf route ([guide](https://github.com/siderolabs/sidero/blob/f812a18c33abc7ce62f07f0201c5e36defd9b1fa/website/content/v0.6/Guides/rpi4-as-servers.md#L21-L27)); it doesn't cover the Pi 5.

**U-Boot is what most other appliance OSes use.** Talos loads `u-boot.bin` from the Pi firmware on both models ([sbc-raspberrypi](https://github.com/siderolabs/sbc-raspberrypi/blob/9fa9252cca1a832a6154e3f431927302bb09d9bb/installers/rpi_5/src/config.txt)), as do NixOS's SD images ([sd-image-aarch64.nix](https://github.com/NixOS/nixpkgs/blob/994c1f3d017712bec91cd1198b3ee8fb66fd3868/nixos/modules/installer/sd-card/sd-image-aarch64.nix)) and balenaOS ([rollbacks](https://docs.balena.io/reference/os/updates/rollbacks)). Ubuntu Core boots firmware-direct and switches kernels with `tryboot` ([snapd's piboot](https://github.com/canonical/snapd/blob/master/bootloader/piboot.go)). U-Boot implements enough of UEFI to run an EFI application, so in principle it can run systemd-boot and our UKIs.

**mkosi has no Pi bootloader.** `Bootloader=` knows systemd-boot, UKIs and GRUB ([mkosi(1)](https://github.com/systemd/mkosi/blob/2fa9a248a2fecebb0dd64a34c0fecdc5ca8335b9/mkosi/resources/man/mkosi.1.md)); [mkosi#2096](https://github.com/systemd/mkosi/issues/2096) closed without one. A Pi image is `Bootloader=none` plus a FAT boot partition we assemble ourselves.

## Decisions to make

The maintainer decides these before any code. Each has a recommendation.

1. **How a Pi boots: through U-Boot to systemd-boot, or straight from the firmware.**
   - *U-Boot's UEFI* (recommended, if spike P0 passes): the Pi firmware loads U-Boot, U-Boot runs systemd-boot as an EFI application, and systemd-boot boots our UKIs with boot counting. The image, `relish os`, the boot check, `systemd-sysupdate` and the A/B layout stay as they are; only the first stage differs. The risks are U-Boot's EFI variable storage on a Pi (systemd-boot's counting renames files on the ESP, but `LoaderBootCountPath` and the default entry are EFI variables) and a reset during a trial boot, which U-Boot doesn't count by itself.
   - *Firmware-direct with `tryboot`*: two FAT boot partitions, each with a kernel, initramfs and a `cmdline.txt` naming its `/usr` slot's verity root hash, `autoboot.txt` to pick one, the watchdog for hangs. Proven by Ubuntu Core and Ubuntu's own `piboot-try`, and no third bootloader to ship. But it's a second update path: a Pi variant of `slot.rs`, `sysupdate.d`, the boot check and the fallback detection, each tested twice from then on.
2. **The first install: SD card, or network boot.** Recommended: the SD card first. `relish image write` already writes a disk image to a stick (design doc §2.8); writing it to an SD card for a Pi is the same command with a Pi image, and a Pi user expects to flash a card. Network boot follows (P4), because every Pi needs its boot order changed before it'll try, which takes an SD card anyway.
3. **Which models.** Recommended: Pi 4 B and Pi 5 with 4 GB or more, booting from SD, USB or (Pi 5) NVMe. The Pi 3 and Zero 2 W have 1 GB or less, below the Wyse's 2 GB and the 1.0 GB `MemAvailable` the Wyse gate demands. Compute Modules come after, since changing their EEPROM needs `rpiboot`.
4. **The lab.** We have no Pi. Recommended: two Pi 4 B 4 GB and one Pi 5 8 GB, the same three-node shape as the Wyse gate, with SD cards and one NVMe HAT. QEMU's `raspi4b` machine can't stand in for them: it has no PCIe or Ethernet (unverified for the latest QEMU; P0 checks).

## Workstreams

Each is one PR into the release train that carries it, with its tests, the manual and the book. Estimates are engineer-weeks.

### P0. Spike: U-Boot to systemd-boot on a Pi 4 and a Pi 5 (~1 week, on hardware)

A throwaway image, by hand, to answer decision 1:
- the Pi firmware loads `u-boot.bin` (from Ubuntu's `u-boot-rpi`), which runs systemd-boot from the ESP, which boots a UKI built on `linux-raspi`, with the device tree the firmware passed through;
- the UKI finds and verifies `/usr` on SD, USB and (Pi 5) NVMe;
- systemd-boot's boot counting: does a UKI marked `+3-0` count down across reboots, does a failed `boot-complete.target` reboot into the next try, and after three, the previous UKI? Does `systemd-bless-boot` rename it once blessed? Are `LoaderBootCountPath` and the other variables `relish os` reads present?
- a hang on a trial boot: with the watchdog on, does a reset count as a failed try?
- the arm64 UKI size against the 512 MiB ESP and `/usr` against its 1.1 GiB slot, with `linux-raspi` in place of `linux-generic`.

**Go:** all of it works on both models. **No-go:** any of it doesn't, and the fix isn't a U-Boot setting. Then we build firmware-direct with `tryboot`. The record goes in `docs/qualification/`.

### P1. The Pi image (~2 weeks)

- A `raspi` mkosi profile on the aarch64 image: `linux-raspi` instead of `linux-generic`, `linux-firmware-raspi`, and the FAT partition the Pi firmware reads: `config.txt`, the firmware files and device trees, and either `u-boot.bin` or the kernel and `cmdline.txt`, per P0.
- `config.txt` turns on the watchdog (`dtparam=watchdog=on`, `kernel_watchdog_timeout`), and the image sets `RuntimeWatchdogSec`, whichever way the Pi boots.
- `appliance.yml` builds the profile as a third artefact, `reliaburger-os-raspi_<version>`, published beside x86_64 and aarch64 in the same signed channel. `src/os/channel.rs` learns a platform as well as an architecture, so an x86_64 or generic arm64 node never stages a Pi image, nor a Pi a generic one.
- The installer's driver test (`image/tests/installer_modules.py`) gains the Pi's SD, USB and NIC drivers and their soft dependencies, the check that found the Wyse's missing PHY driver.
- **Tests:** the profile's mkosi config parsed in the image tests; the channel's platform field, including a refusal to stage the wrong platform; the image booting under QEMU `virt` with the Pi kernel as far as it can (it lacks Pi hardware, so this proves the build, not the boot).

### P2. Install from an SD card, and claim (~1 week)

- `relish image download --platform raspi` and `relish image write` put the Pi image on an SD card or USB stick, with the seed or the RBSEED stick as on a PC.
- The Pi boots unclaimed, shows its claim key on HDMI, and `relish machines claim` works unchanged: claiming doesn't care what the machine is.
- **Tests:** `relish image write` of a Pi image (the partition table and the FAT contents it expects), and the snapshot of the seeds. The rest is the lab.

### P3. OS updates on a Pi (~1 week with U-Boot, ~3 with `tryboot`)

- With U-Boot (P0 passed): nothing new in `relish os`. The lab proves a fallback within 10 minutes on a Pi, as on the Wyse.
- With `tryboot`: a Pi path in `src/os/slot.rs` that writes the inactive boot partition and `/usr` slot, then `reboot '0 tryboot'`; a boot check that commits by rewriting `autoboot.txt`; and bun reading `/proc/device-tree/chosen/bootloader/tryboot` and `partition` to tell a trial boot and a fallback, as it reads `LoaderBootCountPath` today. Each path gets the unit tests the UEFI one has.

### P4. `relish netboot` for Pis (~2 weeks)

- **DHCP.** `answer` stops ignoring a Pi when the artefacts include a Pi platform. It replies as a ProxyDHCP with option 43 as a boot menu reading "Raspberry Pi Boot", and option 54 (our address) for TFTP. The Pi's `PXEClient` and architecture 0 already reach `Ignored::RaspberryPi` (0.3.0); it becomes `BootFile::RaspberryPi`.
- **TFTP.** It serves the files the bootloader asks for, from the Pi installer's netboot directory, under the serial-number prefix as well as the root. The installer is a kernel and an initramfs, since a Pi can't run an EFI installer.
- **One-time setup.** Every Pi needs its boot order changed. `relish image write --pi-netboot-setup` writes an SD card that updates the EEPROM to `BOOT_ORDER=0xf21` (SD, then network), from Raspberry Pi's own recovery bootloader and EEPROM image, with our boot order set by `rpi-eeprom-config`. After one boot from it, a Pi with a blank or missing card netboots, and once the installer has written its card it boots from SD.
- **Tests:** the Pi's DISCOVER, captured byte for byte in the lab, against `answer`, as `the_wyse_3040s_4011_request_gets_a_reply_without_option_43` does for the Wyse; the TFTP file sequence with the serial prefix and its fallback to the root; the two `proptest` properties extended to Pi requests (no address handed out, no reply to a non-PXE client).

### P5. Docs, the website and an exit test (~1 week)

- Manual chapter 17 gains a Pi section; book chapter 15a, the Pi's boot chain and what we chose; `docs/design/appliance.md`, the platform in the channel and the Pi boot path; the features page row moves from planned to shipped.
- **Exit test**, three Pis, the Wyse gate's five criteria with the Pi's own numbers: 3 of 3 claimed; a working cluster within 60 minutes of power-on (SD cards written beforehand count inside the hour); `MemAvailable` ≥ 1.0 GB under the tour; SD wear at the measured write rate, reported but not gated, since cards vary too much; a broken update falls back by itself within 10 minutes.

## What's out

- **Pi 5 HTTP boot.** It needs our key in every Pi's EEPROM, which a public bootloader update wipes. TFTP does the same job.
- **Secure boot on the Pi.** It burns a key hash into OTP for good. As on x86, Secure Boot waits for its own plan.
- **Pi 3, Zero 2 W, and Compute Modules** for now (decision 3).
- **Wi-Fi.** Nodes are wired, as everywhere else.

## Order

P0 decides between two shapes, so nothing else starts until it's recorded. Then P1, P2 and P3 make a Pi you flash and claim, which is most of the value; P4 and P5 follow. Roughly 6 to 9 engineer-weeks, plus the hardware. The milestone is chosen when the maintainer schedules it.
