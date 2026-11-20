//! Machines that installed already, and the boot scripts each machine gets.
//!
//! A machine whose boot order puts the network first would install again
//! on every boot. So the server remembers each machine that downloaded the
//! installer, by MAC address and SMBIOS UUID, in a small JSON file beside
//! the artefacts, and tells it `exit 1` next time: iPXE hands back to the
//! firmware, which boots the next option, the disk.
//!
//! The TFTP `boot.ipxe` is only [`chain_script`]: it asks the HTTP server
//! for the real script with the machine's MAC, UUID and architecture in the
//! query, and [`decide`] picks [`install_script`] or [`EXIT_SCRIPT`].

use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{Arch, MacAddress, NetbootError};

/// Install tickets outstanding at once. A lab boots a handful of machines;
/// past this, a machine is sent to its disk until one finishes or expires.
pub const MAX_INSTALL_TICKETS: usize = 256;

/// How long an install ticket lasts: long enough for a slow install and a
/// ten-minute wait for the operator's answer about a used disk.
pub const INSTALL_TICKET_LIFETIME: std::time::Duration = std::time::Duration::from_secs(3600);

/// One install script served, waiting for its installer's reports.
#[derive(Debug, Clone)]
struct Issued {
    client: BootClient,
    issued: std::time::Instant,
    disk_reported: bool,
}

/// The install tickets of one `relish netboot` session.
///
/// Each install script carries a fresh random ticket on the installer's
/// command line, and the installer sends it back with its reports: the
/// disk it found (`POST /disk`) and, once the image is written and checked,
/// that it installed (`POST /installed`). A report is believed only with a
/// ticket issued to the machine it names, so nobody else on the LAN can
/// mark a machine installed or ask questions about its disk, and a machine
/// counts as installed when its install succeeded, not when it fetched the
/// installer.
#[derive(Debug, Default)]
pub struct InstallTickets {
    tickets: std::collections::HashMap<String, Issued>,
}

impl InstallTickets {
    /// A new ticket for `client`, or `None` while [`MAX_INSTALL_TICKETS`]
    /// are outstanding.
    pub fn issue(&mut self, client: &BootClient, now: std::time::Instant) -> Option<String> {
        self.expire(now);
        if self.tickets.len() >= MAX_INSTALL_TICKETS {
            return None;
        }
        loop {
            let ticket = format!("{:032x}", rand::random::<u128>());
            if !self.tickets.contains_key(&ticket) {
                self.tickets.insert(
                    ticket.clone(),
                    Issued {
                        client: client.clone(),
                        issued: now,
                        disk_reported: false,
                    },
                );
                return Some(ticket);
            }
        }
    }

    /// True the first time `client` reports its disk with `ticket`.
    pub fn report_disk(
        &mut self,
        ticket: &str,
        client: &BootClient,
        now: std::time::Instant,
    ) -> bool {
        self.expire(now);
        match self.tickets.get_mut(ticket) {
            Some(issued) if issued.client.same_machine(client) && !issued.disk_reported => {
                issued.disk_reported = true;
                true
            }
            _ => false,
        }
    }

    /// True if `ticket` was issued to `client`; it's then used up.
    pub fn complete(&mut self, ticket: &str, client: &BootClient, now: std::time::Instant) -> bool {
        self.expire(now);
        let valid = self
            .tickets
            .get(ticket)
            .is_some_and(|issued| issued.client.same_machine(client));
        if valid {
            self.tickets.remove(ticket);
        }
        valid
    }

    /// Installs that were sent an install script and haven't reported
    /// success, or expired.
    pub fn in_flight(&self, now: std::time::Instant) -> usize {
        self.tickets
            .values()
            .filter(|issued| now.duration_since(issued.issued) < INSTALL_TICKET_LIFETIME)
            .count()
    }

    fn expire(&mut self, now: std::time::Instant) {
        self.tickets
            .retain(|_, issued| now.duration_since(issued.issued) < INSTALL_TICKET_LIFETIME);
    }
}

/// The record's file name, in the served directory.
pub const RECORD_FILE: &str = "netboot-installed.json";

/// What a machine said about itself when it asked for its boot script.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BootClient {
    /// The MAC address iPXE booted from.
    pub mac: Option<MacAddress>,
    /// The SMBIOS UUID, lowercase, when the firmware has a real one.
    pub uuid: Option<String>,
    /// iPXE's `${buildarch}`.
    pub arch: Option<Arch>,
}

impl BootClient {
    /// Build from the query iPXE sent. Values that don't parse are dropped
    /// rather than refused: an odd UUID shouldn't stop an install.
    pub fn from_query(mac: Option<&str>, uuid: Option<&str>, arch: Option<&str>) -> Self {
        BootClient {
            mac: mac.and_then(|m| m.parse().ok()),
            uuid: uuid.and_then(normalise_uuid),
            arch: arch.and_then(Arch::from_ipxe_name),
        }
    }

    /// `mac=…&uuid=…` for the URLs in the install script, so the installer
    /// download says whose it is.
    pub fn query(&self) -> String {
        let mut parts = Vec::new();
        if let Some(mac) = self.mac {
            parts.push(format!("mac={mac}"));
        }
        if let Some(uuid) = &self.uuid {
            parts.push(format!("uuid={uuid}"));
        }
        parts.join("&")
    }

    /// True if `other` is this machine: the same MAC or the same UUID. Two
    /// clients that say nothing about themselves aren't the same machine.
    pub fn same_machine(&self, other: &BootClient) -> bool {
        (self.mac.is_some() && self.mac == other.mac)
            || (self.uuid.is_some() && self.uuid == other.uuid)
    }

    fn describe(&self) -> String {
        let mac = self
            .mac
            .map_or("unknown MAC".to_string(), |m| m.to_string());
        match &self.uuid {
            Some(uuid) => format!("{mac} (UUID {uuid})"),
            None => mac,
        }
    }
}

impl std::fmt::Display for BootClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.describe())
    }
}

/// A UUID in its usual text form, lowercase; `None` for one that isn't, or
/// for the all-zero and all-`f` UUIDs that some firmware reports for every
/// machine (matching on those would skip machines that never installed).
fn normalise_uuid(input: &str) -> Option<String> {
    let lower = input.trim().to_ascii_lowercase();
    let shape_ok = lower.len() == 36
        && lower.char_indices().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_hexdigit(),
        });
    let digits: String = lower.chars().filter(|c| *c != '-').collect();
    let placeholder = digits.chars().all(|c| c == '0') || digits.chars().all(|c| c == 'f');
    (shape_ok && !placeholder).then_some(lower)
}

/// One machine that downloaded the installer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstalledMachine {
    /// Its MAC address, `aa:bb:cc:dd:ee:ff`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mac: Option<String>,
    /// Its SMBIOS UUID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uuid: Option<String>,
    /// iPXE's architecture name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arch: Option<String>,
    /// When it fetched the installer, in seconds since the Unix epoch.
    pub installed_at: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct RecordFile {
    machines: Vec<InstalledMachine>,
}

/// The machines that installed from this directory, kept in a JSON file.
#[derive(Debug)]
pub struct InstalledRecord {
    path: PathBuf,
    machines: Vec<InstalledMachine>,
}

impl InstalledRecord {
    /// Read the record at `path`; a missing file is an empty record.
    pub fn load(path: &Path) -> Result<Self, NetbootError> {
        let machines = match std::fs::read(path) {
            Ok(bytes) => {
                serde_json::from_slice::<RecordFile>(&bytes)
                    .map_err(|e| {
                        NetbootError::io(
                            path.display().to_string(),
                            std::io::Error::new(std::io::ErrorKind::InvalidData, e),
                        )
                    })?
                    .machines
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(NetbootError::io(path.display().to_string(), e)),
        };
        Ok(InstalledRecord {
            path: path.to_path_buf(),
            machines,
        })
    }

    /// Every machine remembered so far.
    pub fn machines(&self) -> &[InstalledMachine] {
        &self.machines
    }

    /// True if a machine with this MAC or this UUID installed already.
    pub fn contains(&self, client: &BootClient) -> bool {
        let mac = client.mac.map(|m| m.to_string());
        self.machines.iter().any(|machine| {
            (mac.is_some() && machine.mac == mac)
                || (client.uuid.is_some() && machine.uuid == client.uuid)
        })
    }

    /// Remember `client` and save the file. Returns false (and writes
    /// nothing) when it's already remembered or says nothing to identify it.
    pub fn remember(&mut self, client: &BootClient, now: u64) -> Result<bool, NetbootError> {
        if (client.mac.is_none() && client.uuid.is_none()) || self.contains(client) {
            return Ok(false);
        }
        self.machines.push(InstalledMachine {
            mac: client.mac.map(|m| m.to_string()),
            uuid: client.uuid.clone(),
            arch: client.arch.map(|a| a.ipxe_name().to_string()),
            installed_at: now,
        });
        self.save()?;
        Ok(true)
    }

    /// Forget every machine with `client`'s MAC or UUID and save the file:
    /// a machine whose disk wasn't wiped didn't install. Returns false (and
    /// writes nothing) when there was nothing to forget.
    pub fn forget(&mut self, client: &BootClient) -> Result<bool, NetbootError> {
        let mac = client.mac.map(|m| m.to_string());
        let before = self.machines.len();
        self.machines.retain(|machine| {
            !((mac.is_some() && machine.mac == mac)
                || (client.uuid.is_some() && machine.uuid == client.uuid))
        });
        if self.machines.len() == before {
            return Ok(false);
        }
        self.save()?;
        Ok(true)
    }

    fn save(&self) -> Result<(), NetbootError> {
        let file = RecordFile {
            machines: self.machines.clone(),
        };
        let json = serde_json::to_vec_pretty(&file).map_err(|e| {
            NetbootError::io(
                self.path.display().to_string(),
                std::io::Error::new(std::io::ErrorKind::InvalidData, e),
            )
        })?;
        let temporary = self.path.with_extension("json.part");
        std::fs::write(&temporary, json)
            .and_then(|()| std::fs::rename(&temporary, &self.path))
            .map_err(|e| NetbootError::io(self.path.display().to_string(), e))
    }
}

/// What a machine's boot script tells it to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootDecision {
    /// Fetch and run the installer.
    Install,
    /// It installed already: boot from its disk.
    AlreadyInstalled,
    /// `--mac` doesn't list it: boot whatever comes next.
    NotAllowed,
    /// The operator kept its disk this session: boot whatever comes next.
    Declined,
}

/// Decide what `client` gets, given the record, the `--mac` allow-list
/// (empty allows everyone), `--reinstall`, and whether the operator
/// declined to wipe its disk this session.
pub fn decide(
    client: &BootClient,
    record: &InstalledRecord,
    allowed: &[MacAddress],
    reinstall: bool,
    declined: bool,
) -> BootDecision {
    if !allowed.is_empty() && !client.mac.is_some_and(|mac| allowed.contains(&mac)) {
        return BootDecision::NotAllowed;
    }
    if declined {
        return BootDecision::Declined;
    }
    if !reinstall && record.contains(client) {
        return BootDecision::AlreadyInstalled;
    }
    BootDecision::Install
}

/// The `boot.ipxe` served over TFTP: hand over to HTTP, saying who's asking.
///
/// `${netX/mac}` is the interface iPXE configured last, the one that just
/// booted. `exit 1` if HTTP fails, so the firmware moves on to the disk.
pub fn chain_script(server: Ipv4Addr, http_port: u16) -> String {
    format!(
        "#!ipxe\n\
         # relish netboot: ask for this machine's boot script.\n\
         chain --autofree http://{server}:{http_port}/boot.ipxe?mac=${{netX/mac}}&uuid=${{uuid}}&arch=${{buildarch}} || exit 1\n"
    )
}

/// The install script, as `image/netboot/boot.ipxe` does it but with this
/// server's address and port written in, and the installer URL carrying
/// `client` so the server can remember the machine.
///
/// `reliaburger.ask` is where the installer reports a disk that isn't
/// blank and waits for the operator's answer ([`super::wipe`]), and
/// `reliaburger.done` where it says it installed. Both carry the install
/// `ticket` ([`InstallTickets`]). They're left out when the machine sent
/// nothing to key them on: the installer then refuses a used disk, as it
/// always did, and the machine is never recorded as installed. The script never passes
/// `reliaburger.wipe=1`: only the operator's yes or `--wipe` wipes a disk.
///
/// With `reinstall` (`relish netboot --reinstall`) it adds
/// `reliaburger.reinstall=1`. The installer then treats a disk that holds
/// Reliaburger already like any other used disk, so it asks, rather than
/// booting it.
///
/// Arguments to a UKI replace its built-in command line (Secure Boot
/// off), so this passes the console too. `--autofree`: a failed attempt
/// mustn't leave the installer registered, or iPXE hands it to the next
/// try as an initrd.
pub fn install_script(
    server: Ipv4Addr,
    http_port: u16,
    client: &BootClient,
    reinstall: bool,
    ticket: Option<&str>,
) -> String {
    let query = client.query();
    let (query, mut ask) = match ticket {
        Some(ticket) if !query.is_empty() => (
            format!("?{query}"),
            format!(
                " reliaburger.ask=http://{server}:{http_port}/disk?{query}&ticket={ticket} \
                 reliaburger.done=http://{server}:{http_port}/installed?{query}&ticket={ticket}"
            ),
        ),
        _ if !query.is_empty() => (format!("?{query}"), String::new()),
        _ => (String::new(), String::new()),
    };
    if reinstall {
        ask.push_str(" reliaburger.reinstall=1");
    }
    format!(
        "#!ipxe\n\
         # relish netboot: install Reliaburger on this machine.\n\
         iseq ${{buildarch}} arm64 && set console ttyAMA0 || set console ttyS0\n\
         set base http://{server}:{http_port}/${{buildarch}}\n\
         echo reliaburger: chaining ${{base}}/installer.efi\n\
         chain --autofree ${{base}}/installer.efi{query} reliaburger.url=${{base}}{ask} console=tty0 console=${{console}},115200\n"
    )
}

/// The script for a machine that installed already.
/// `exit 1` rather than `exit`: UEFI firmware may stop at its boot menu
/// when a boot option returns success, but moves on after a failure.
pub const EXIT_SCRIPT: &str = "#!ipxe\n\
# relish netboot: nothing to install here.\n\
echo reliaburger: this machine installed already, booting the next option\n\
exit 1\n";

/// The script for a machine `--mac` doesn't list.
pub const NOT_ALLOWED_SCRIPT: &str = "#!ipxe\n\
# relish netboot: this machine is not on the --mac list.\n\
echo reliaburger: not on the netboot server --mac list, booting the next option\n\
exit 1\n";

/// The script for a machine whose disk the operator kept this session.
pub const DECLINED_SCRIPT: &str = "#!ipxe\n\
# relish netboot: the operator kept this machine's disk.\n\
echo reliaburger: the operator declined to wipe this disk, booting the next option\n\
exit 1\n";

#[cfg(test)]
mod tests {
    use super::*;

    const MAC: &str = "52:54:00:12:34:56";
    const UUID: &str = "4C4C4544-0042-3510-8051-B4C04F4B4E32";

    fn client(mac: Option<&str>, uuid: Option<&str>) -> BootClient {
        BootClient::from_query(mac, uuid, Some("x86_64"))
    }

    fn empty_record(dir: &Path) -> InstalledRecord {
        InstalledRecord::load(&dir.join(RECORD_FILE)).unwrap()
    }

    #[test]
    fn a_query_from_ipxe_becomes_a_client() {
        let c = client(Some(MAC), Some(UUID));
        assert_eq!(c.mac.unwrap().to_string(), MAC);
        assert_eq!(
            c.uuid.as_deref(),
            Some("4c4c4544-0042-3510-8051-b4c04f4b4e32")
        );
        assert_eq!(c.arch, Some(Arch::X86_64));
        assert_eq!(
            c.query(),
            "mac=52:54:00:12:34:56&uuid=4c4c4544-0042-3510-8051-b4c04f4b4e32"
        );
    }

    #[test]
    fn placeholder_and_malformed_uuids_are_dropped() {
        for bad in [
            "00000000-0000-0000-0000-000000000000",
            "FFFFFFFF-FFFF-FFFF-FFFF-FFFFFFFFFFFF",
            "",
            "${uuid}",
            "4c4c4544004235108051b4c04f4b4e32",
        ] {
            assert_eq!(client(None, Some(bad)).uuid, None, "{bad}");
        }
        assert_eq!(client(Some("not a mac"), None).mac, None);
    }

    #[test]
    fn a_new_machine_installs_and_is_remembered_by_mac_or_uuid() {
        let dir = tempfile::tempdir().unwrap();
        let mut record = empty_record(dir.path());
        let machine = client(Some(MAC), Some(UUID));
        assert_eq!(
            decide(&machine, &record, &[], false, false),
            BootDecision::Install
        );
        assert!(record.remember(&machine, 1_790_000_000).unwrap());
        assert!(!record.remember(&machine, 1_790_000_001).unwrap());

        let reread = empty_record(dir.path());
        assert_eq!(reread.machines().len(), 1);
        assert_eq!(reread.machines()[0].arch.as_deref(), Some("x86_64"));
        let same_mac = client(Some(MAC), None);
        let same_uuid = client(Some("52:54:00:00:00:01"), Some(UUID));
        for known in [&machine, &same_mac, &same_uuid] {
            assert_eq!(
                decide(known, &reread, &[], false, false),
                BootDecision::AlreadyInstalled
            );
        }
        let stranger = client(Some("52:54:00:00:00:02"), None);
        assert_eq!(
            decide(&stranger, &reread, &[], false, false),
            BootDecision::Install
        );
    }

    #[test]
    fn reinstall_ignores_the_record() {
        let dir = tempfile::tempdir().unwrap();
        let mut record = empty_record(dir.path());
        let machine = client(Some(MAC), None);
        record.remember(&machine, 1).unwrap();
        assert_eq!(
            decide(&machine, &record, &[], true, false),
            BootDecision::Install
        );
    }

    #[test]
    fn the_mac_list_turns_away_everyone_else_even_without_a_mac() {
        let dir = tempfile::tempdir().unwrap();
        let record = empty_record(dir.path());
        let allowed = [MAC.parse().unwrap()];
        assert_eq!(
            decide(&client(Some(MAC), None), &record, &allowed, false, false),
            BootDecision::Install
        );
        assert_eq!(
            decide(
                &client(Some("52:54:00:00:00:01"), None),
                &record,
                &allowed,
                false,
                false
            ),
            BootDecision::NotAllowed
        );
        assert_eq!(
            decide(&client(None, Some(UUID)), &record, &allowed, false, false),
            BootDecision::NotAllowed
        );
    }

    #[test]
    fn a_client_with_nothing_to_identify_it_is_not_remembered() {
        let dir = tempfile::tempdir().unwrap();
        let mut record = empty_record(dir.path());
        assert!(!record.remember(&BootClient::default(), 1).unwrap());
        assert!(!dir.path().join(RECORD_FILE).exists());
    }

    #[test]
    fn a_corrupt_record_is_an_error_not_an_empty_list() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(RECORD_FILE), b"{").unwrap();
        assert!(InstalledRecord::load(&dir.path().join(RECORD_FILE)).is_err());
    }

    #[test]
    fn the_tftp_script_chains_to_http_with_the_machine_in_the_query() {
        let script = chain_script(Ipv4Addr::new(192, 168, 1, 20), 8080);
        assert!(script.starts_with("#!ipxe\n"));
        assert!(script.contains(
            "chain --autofree http://192.168.1.20:8080/boot.ipxe?mac=${netX/mac}&uuid=${uuid}&arch=${buildarch} || exit 1"
        ));
    }

    #[test]
    fn the_install_script_matches_boot_ipxe_with_the_server_written_in() {
        let script = install_script(
            Ipv4Addr::new(192, 168, 1, 20),
            8081,
            &client(Some(MAC), None),
            false,
            Some("00ff"),
        );
        assert!(script.starts_with("#!ipxe\n"));
        assert!(script.contains("set base http://192.168.1.20:8081/${buildarch}\n"));
        assert!(script.contains(
            "chain --autofree ${base}/installer.efi?mac=52:54:00:12:34:56 reliaburger.url=${base} reliaburger.ask=http://192.168.1.20:8081/disk?mac=52:54:00:12:34:56&ticket=00ff reliaburger.done=http://192.168.1.20:8081/installed?mac=52:54:00:12:34:56&ticket=00ff console=tty0 console=${console},115200\n"
        ));
        assert!(script.contains("set console ttyAMA0 || set console ttyS0"));
        let anonymous = install_script(
            Ipv4Addr::LOCALHOST,
            8080,
            &BootClient::default(),
            false,
            None,
        );
        assert!(anonymous.contains("${base}/installer.efi reliaburger.url="));
    }

    /// The installer asks relish about a used disk at the URL this passes,
    /// keyed by the MAC and UUID the chain script sent.
    #[test]
    fn the_install_script_tells_the_installer_where_to_ask_about_a_used_disk() {
        let script = install_script(
            Ipv4Addr::new(192, 168, 1, 20),
            8081,
            &client(Some(MAC), Some(UUID)),
            false,
            Some("00ff"),
        );
        assert!(
            script.contains(
                " reliaburger.ask=http://192.168.1.20:8081/disk?mac=52:54:00:12:34:56&uuid=4c4c4544-0042-3510-8051-b4c04f4b4e32&ticket=00ff "
            ),
            "{script}"
        );
        assert!(!script.contains("reliaburger.wipe"), "{script}");
        // With nothing to key the question on, there's no question: the
        // installer refuses a used disk, as it always did.
        let anonymous = install_script(
            Ipv4Addr::LOCALHOST,
            8080,
            &BootClient::default(),
            false,
            None,
        );
        assert!(!anonymous.contains("reliaburger.ask"), "{anonymous}");
        assert!(!anonymous.contains("reliaburger.done"), "{anonymous}");
    }

    /// `--reinstall` tells the installer not to boot a disk that holds
    /// Reliaburger already, so it asks about that disk like any used one.
    /// It never skips the question: the script still has no wipe.
    #[test]
    fn a_reinstall_tells_the_installer_to_replace_reliaburger_too() {
        let machine = client(Some(MAC), Some(UUID));
        let again = install_script(
            Ipv4Addr::new(192, 168, 1, 20),
            8081,
            &machine,
            true,
            Some("00ff"),
        );
        assert!(again.contains(" reliaburger.reinstall=1 "), "{again}");
        assert!(!again.contains("reliaburger.wipe"), "{again}");
        let first = install_script(
            Ipv4Addr::new(192, 168, 1, 20),
            8081,
            &machine,
            false,
            Some("00ff"),
        );
        assert!(!first.contains("reliaburger.reinstall"), "{first}");
    }

    /// An install ticket is the machine's proof, on its disk report and its
    /// "installed" report, that relish sent it the install script: anyone
    /// else on the LAN can name a MAC, but not guess its ticket.
    #[test]
    fn an_install_ticket_vouches_only_for_the_machine_it_was_issued_to() {
        let now = std::time::Instant::now();
        let machine = client(Some(MAC), Some(UUID));
        let other = client(Some("52:54:00:00:00:02"), None);
        let mut tickets = InstallTickets::default();
        let ticket = tickets.issue(&machine, now).unwrap();
        assert!(ticket.len() >= 16 && ticket.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(!tickets.report_disk("0000", &machine, now));
        assert!(!tickets.report_disk(&ticket, &other, now));
        assert!(tickets.report_disk(&ticket, &machine, now));
        assert!(
            !tickets.report_disk(&ticket, &machine, now),
            "one disk report a ticket"
        );
        assert_eq!(tickets.in_flight(now), 1);
        assert!(!tickets.complete(&ticket, &other, now));
        assert!(tickets.complete(&ticket, &machine, now));
        assert!(
            !tickets.complete(&ticket, &machine, now),
            "a ticket is used once"
        );
        assert_eq!(tickets.in_flight(now), 0);
    }

    #[test]
    fn install_tickets_expire_and_are_capped() {
        let now = std::time::Instant::now();
        let machine = client(Some(MAC), None);
        let mut tickets = InstallTickets::default();
        let ticket = tickets.issue(&machine, now).unwrap();
        let later = now + INSTALL_TICKET_LIFETIME + std::time::Duration::from_secs(1);
        assert!(!tickets.complete(&ticket, &machine, later), "expired");
        for _ in 0..MAX_INSTALL_TICKETS {
            assert!(tickets.issue(&machine, later).is_some());
        }
        assert!(tickets.issue(&machine, later).is_none(), "full");
        assert_eq!(tickets.in_flight(later), MAX_INSTALL_TICKETS);
    }

    #[test]
    fn a_machine_is_the_same_by_its_mac_or_its_uuid() {
        let both = client(Some(MAC), Some(UUID));
        assert!(both.same_machine(&client(Some(MAC), None)));
        assert!(both.same_machine(&client(None, Some(UUID))));
        assert!(!both.same_machine(&client(Some("52:54:00:00:00:02"), None)));
        assert!(!BootClient::default().same_machine(&BootClient::default()));
    }

    #[test]
    fn a_machine_declined_this_session_gets_exit_even_with_reinstall() {
        let dir = tempfile::tempdir().unwrap();
        let record = empty_record(dir.path());
        let machine = client(Some(MAC), None);
        assert_eq!(
            decide(&machine, &record, &[], true, true),
            BootDecision::Declined
        );
        let allowed = [MAC.parse().unwrap()];
        assert_eq!(
            decide(&machine, &record, &allowed, false, true),
            BootDecision::Declined
        );
    }

    #[test]
    fn a_declined_machine_is_forgotten_so_it_is_asked_again_next_time() {
        let dir = tempfile::tempdir().unwrap();
        let mut record = empty_record(dir.path());
        let machine = client(Some(MAC), Some(UUID));
        let other = client(Some("52:54:00:00:00:01"), None);
        record.remember(&machine, 1).unwrap();
        record.remember(&other, 2).unwrap();
        assert!(record.forget(&client(Some(MAC), None)).unwrap());
        assert!(!record.forget(&client(Some(MAC), None)).unwrap());
        let reread = empty_record(dir.path());
        assert!(!reread.contains(&machine));
        assert!(reread.contains(&other));
    }

    #[test]
    fn machines_that_should_not_install_exit_with_a_failure() {
        for script in [EXIT_SCRIPT, NOT_ALLOWED_SCRIPT, DECLINED_SCRIPT] {
            assert!(script.starts_with("#!ipxe\n"));
            assert!(script.ends_with("exit 1\n"));
        }
    }
}
