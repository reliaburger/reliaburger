//! Confirm, then wipe: what happens to a machine whose disk isn't blank.
//!
//! The installer looks at its target disk before writing anything. A blank
//! disk installs at once. Otherwise it posts what it found to
//! `POST /disk?mac=…&uuid=…` ([`DiskReport::parse`] reads it) and gets back
//! a ticket, then polls `GET /disk/<ticket>` until it reads `wipe`,
//! `decline` or `install`. Only the operator answers: interactively at the
//! `relish netboot` terminal ([`ask_operator`]), or in advance with
//! `--wipe <mac>`. [`decide`] is the whole decision table; nothing a
//! machine sends can turn a "no" into a "yes", because no endpoint accepts
//! a decision.

use std::collections::HashMap;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;

use super::MacAddress;
use super::installed::BootClient;

/// How long a question waits for the operator, counted from the report.
/// The installer polls for a little longer (15 minutes), so it always
/// hears the "no answer" decline rather than giving up first.
pub const QUESTION_TIMEOUT: Duration = Duration::from_secs(600);

/// The most a disk report may hold. lsblk's output for a disk with a
/// hundred partitions is a few kilobytes.
pub const MAX_REPORT_BYTES: usize = 64 * 1024;

/// One row under the disk in the report: a partition, or anything else
/// lsblk lists beneath it (an LVM volume, a RAID member).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Volume {
    /// lsblk's `TYPE`: `part` for a partition.
    pub kind: String,
    /// Its filesystem type, such as `ext4` or `vfat`.
    pub fstype: Option<String>,
    /// Its filesystem label.
    pub label: Option<String>,
    /// Its GPT partition name.
    pub partlabel: Option<String>,
}

/// What the installer found on its target disk: the output of
/// `lsblk -nbpPo NAME,TYPE,SIZE,PTTYPE,FSTYPE,LABEL,PARTLABEL <disk>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskReport {
    /// The disk's device path, such as `/dev/mmcblk0`.
    pub name: String,
    /// Its size in bytes.
    pub size: u64,
    /// Its partition table, `gpt` or `dos`, if it has one.
    pub table: Option<String>,
    /// A filesystem written straight onto the whole disk, if any.
    pub fstype: Option<String>,
    /// That filesystem's label.
    pub label: Option<String>,
    /// Everything under the disk.
    pub volumes: Vec<Volume>,
}

/// Why a disk report was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ReportError {
    #[error("the report is empty")]
    Empty,
    #[error("line {line} isn't lsblk -P output: {reason}")]
    Syntax { line: usize, reason: &'static str },
    #[error("the first line isn't a disk")]
    NoDisk,
    #[error("the disk's SIZE {0:?} isn't a number of bytes")]
    Size(String),
}

impl DiskReport {
    /// Read lsblk's `KEY="value"` lines. The first must be the disk; every
    /// line after it is something on the disk.
    pub fn parse(text: &str) -> Result<DiskReport, ReportError> {
        let mut rows = text
            .lines()
            .enumerate()
            .filter(|(_, line)| !line.trim().is_empty())
            .map(|(index, line)| parse_row(line).map_err(|reason| syntax(index + 1, reason)));
        let disk = rows.next().ok_or(ReportError::Empty)??;
        if disk.get("TYPE") != Some("disk") {
            return Err(ReportError::NoDisk);
        }
        let size = disk.get("SIZE").unwrap_or_default();
        let size = size
            .parse()
            .map_err(|_| ReportError::Size(size.to_string()))?;
        let volumes = rows
            .map(|row| {
                row.map(|row| Volume {
                    kind: row.get("TYPE").unwrap_or_default().to_string(),
                    fstype: row.value("FSTYPE"),
                    label: row.value("LABEL"),
                    partlabel: row.value("PARTLABEL"),
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(DiskReport {
            name: disk.get("NAME").unwrap_or_default().to_string(),
            size,
            table: disk.value("PTTYPE"),
            fstype: disk.value("FSTYPE"),
            label: disk.value("LABEL"),
            volumes,
        })
    }

    /// True when there's nothing on the disk to lose: no partition table,
    /// no filesystem, nothing beneath it.
    pub fn is_blank(&self) -> bool {
        self.table.is_none() && self.fstype.is_none() && self.volumes.is_empty()
    }

    /// One line for the operator, such as `/dev/mmcblk0, 7.8 GB, gpt,
    /// 2 partitions: vfat "EFI", ext4 "ThinOS"`. Labels come from the
    /// machine, so they're quoted and escaped: a label can't move the
    /// cursor or clear the operator's terminal.
    pub fn summary(&self) -> String {
        let size = format!("{:.1} GB", self.size as f64 / 1e9);
        let mut parts = vec![self.name.escape_debug().to_string(), size];
        if self.is_blank() {
            parts.push("blank".to_string());
            return parts.join(", ");
        }
        if let Some(table) = &self.table {
            parts.push(table.escape_debug().to_string());
        }
        if let Some(fstype) = &self.fstype {
            parts.push(describe(Some(fstype), self.label.as_ref()));
        }
        if !self.volumes.is_empty() {
            let count = self.volumes.iter().filter(|v| v.kind == "part").count();
            let what = if count == 1 {
                "partition"
            } else {
                "partitions"
            };
            let contents: Vec<String> = self
                .volumes
                .iter()
                .map(|v| describe(v.fstype.as_ref(), v.label.as_ref().or(v.partlabel.as_ref())))
                .collect();
            parts.push(format!("{count} {what}: {}", contents.join(", ")));
        }
        parts.join(", ")
    }
}

/// `ext4 "ThinOS"`, `swap`, or `(no filesystem)`. `{:?}` quotes the label
/// and escapes anything that isn't printable.
fn describe(fstype: Option<&String>, label: Option<&String>) -> String {
    let fstype = fstype.map_or("(no filesystem)".to_string(), |f| {
        f.escape_debug().to_string()
    });
    match label {
        Some(label) => format!("{fstype} {label:?}"),
        None => fstype,
    }
}

fn syntax(line: usize, reason: &'static str) -> ReportError {
    ReportError::Syntax { line, reason }
}

/// One line of `lsblk -P`: `KEY="value"` pairs, in order.
struct Row(Vec<(String, String)>);

impl Row {
    fn get(&self, key: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// The value, or `None` when it's missing or empty (lsblk prints `""`
    /// for "none").
    fn value(&self, key: &str) -> Option<String> {
        self.get(key).filter(|v| !v.is_empty()).map(str::to_string)
    }
}

/// Parse `KEY="value" KEY="value" …`. lsblk writes `"`, `\`, `$`, `` ` ``
/// and unprintable bytes as `\xNN`, so a value never holds a raw quote.
fn parse_row(line: &str) -> Result<Row, &'static str> {
    let mut pairs = Vec::new();
    let mut rest = line.trim();
    while !rest.is_empty() {
        let (key, after) = rest.split_once('=').ok_or("a field without =")?;
        if key.is_empty() || !key.chars().all(|c| c.is_ascii_uppercase() || c == '-') {
            return Err("a field name that isn't lsblk's");
        }
        let after = after.strip_prefix('"').ok_or("a value without quotes")?;
        let (raw, after) = after.split_once('"').ok_or("an unterminated value")?;
        pairs.push((key.to_string(), unescape(raw)?));
        rest = after.trim_start();
        if !after.is_empty() && after.len() == rest.len() {
            return Err("fields not separated by spaces");
        }
    }
    if pairs.is_empty() {
        return Err("no fields");
    }
    Ok(Row(pairs))
}

/// Decode lsblk's `\xNN` escapes.
fn unescape(raw: &str) -> Result<String, &'static str> {
    let mut bytes = Vec::with_capacity(raw.len());
    let mut input = raw.bytes();
    while let Some(byte) = input.next() {
        if byte != b'\\' {
            bytes.push(byte);
            continue;
        }
        let escape: Vec<u8> = input.by_ref().take(3).collect();
        let hex = match escape.as_slice() {
            [b'x', high, low] => [*high, *low],
            _ => return Err("an escape that isn't \\xNN"),
        };
        let hex = std::str::from_utf8(&hex).map_err(|_| "an escape that isn't \\xNN")?;
        bytes.push(u8::from_str_radix(hex, 16).map_err(|_| "an escape that isn't \\xNN")?);
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// What the operator said, or why there's no answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    /// `y` or `yes`.
    Yes,
    /// Anything else, including a bare Enter.
    No,
    /// Nothing within [`QUESTION_TIMEOUT`], or stdin closed.
    NoAnswer,
    /// stdin isn't a terminal, so nobody can be asked.
    NoTerminal,
}

/// Read one line the operator typed: only `y` or `yes` (any case) is a yes.
pub fn parse_answer(line: &str) -> Answer {
    match line.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => Answer::Yes,
        _ => Answer::No,
    }
}

/// What happens to the disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WipeDecision {
    /// It's blank: install without wiping anything.
    Install,
    /// Wipe it, then install.
    WipeAndInstall,
    /// Leave it untouched.
    Decline,
}

impl WipeDecision {
    /// The word `GET /disk/<ticket>` answers with.
    pub fn word(self) -> &'static str {
        match self {
            WipeDecision::Install => "install",
            WipeDecision::WipeAndInstall => "wipe",
            WipeDecision::Decline => "decline",
        }
    }
}

/// The decision that needs no question: a blank disk installs, and a disk
/// on a machine `--wipe` lists is wiped. `None` means ask the operator.
pub fn without_asking(
    report: &DiskReport,
    mac: Option<MacAddress>,
    pre_approved: &[MacAddress],
) -> Option<WipeDecision> {
    if report.is_blank() {
        Some(WipeDecision::Install)
    } else if mac.is_some_and(|mac| pre_approved.contains(&mac)) {
        Some(WipeDecision::WipeAndInstall)
    } else {
        None
    }
}

/// The whole decision table. Only a blank disk, a `--wipe` MAC or the
/// operator's yes lets the installer write; everything else declines.
pub fn decide(
    report: &DiskReport,
    mac: Option<MacAddress>,
    pre_approved: &[MacAddress],
    answer: Answer,
) -> WipeDecision {
    without_asking(report, mac, pre_approved).unwrap_or(match answer {
        Answer::Yes => WipeDecision::WipeAndInstall,
        Answer::No | Answer::NoAnswer | Answer::NoTerminal => WipeDecision::Decline,
    })
}

/// A question for the operator, answered by [`ask_operator`].
#[derive(Debug)]
pub struct Question {
    /// Who's asking, such as `6c:4b:90:12:34:56 (UUID …)`.
    pub machine: String,
    /// What's on its disk ([`DiskReport::summary`]).
    pub summary: String,
    /// When to stop waiting for an answer.
    pub deadline: Instant,
    /// Where the answer goes.
    pub reply: oneshot::Sender<Answer>,
}

/// Ask the operator each question in turn, reading answers from `lines`
/// (stdin, one line per answer). Lines typed before a question is shown
/// are dropped, so a stray Enter can't answer a question nobody saw. A
/// question whose deadline passes, or that's still queued when it does,
/// gets [`Answer::NoAnswer`].
pub async fn ask_operator(
    mut questions: mpsc::UnboundedReceiver<Question>,
    mut lines: mpsc::UnboundedReceiver<String>,
    print: impl Fn(String),
) {
    while let Some(question) = questions.recv().await {
        while lines.try_recv().is_ok() {}
        let answer = if question.deadline <= Instant::now() {
            print(format!(
                "{}: no answer in time; leaving its disk alone",
                question.machine
            ));
            Answer::NoAnswer
        } else {
            print(format!(
                "{}: {} — wipe? [y/N]",
                question.machine, question.summary
            ));
            match tokio::time::timeout_at(question.deadline, lines.recv()).await {
                Ok(Some(line)) => parse_answer(&line),
                Ok(None) | Err(_) => Answer::NoAnswer,
            }
        };
        // The machine may have gone; then nobody's waiting for the answer.
        let _ = question.reply.send(answer);
    }
}

/// The disk questions of one `relish netboot` session: each ticket's
/// decision, once there is one, and the machines that were declined.
#[derive(Debug, Default)]
pub struct DiskSession {
    tickets: HashMap<String, Option<WipeDecision>>,
    declined: Vec<BootClient>,
}

impl DiskSession {
    /// A new ticket, waiting for its decision.
    pub fn open(&mut self) -> String {
        loop {
            let ticket = format!("{:016x}", rand::random::<u64>());
            if !self.tickets.contains_key(&ticket) {
                self.tickets.insert(ticket.clone(), None);
                return ticket;
            }
        }
    }

    /// Record `ticket`'s decision. A decline also remembers `client`, so it
    /// isn't asked again (or offered the installer) this session.
    pub fn settle(&mut self, ticket: &str, client: &BootClient, decision: WipeDecision) {
        self.tickets.insert(ticket.to_string(), Some(decision));
        if decision == WipeDecision::Decline && !self.is_declined(client) {
            self.declined.push(client.clone());
        }
    }

    /// `None` for an unknown ticket; `Some(None)` while it waits.
    pub fn decision(&self, ticket: &str) -> Option<Option<WipeDecision>> {
        self.tickets.get(ticket).copied()
    }

    /// True if a machine with this MAC or UUID was declined this session.
    pub fn is_declined(&self, client: &BootClient) -> bool {
        self.declined.iter().any(|machine| {
            (client.mac.is_some() && machine.mac == client.mac)
                || (client.uuid.is_some() && machine.uuid == client.uuid)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAC: &str = "6c:4b:90:12:34:56";

    /// What lsblk -nbpP prints for a Wyse 3040's eMMC holding ThinOS-style
    /// partitions (the labels are illustrative).
    const USED: &str = concat!(
        r#"NAME="/dev/mmcblk0" TYPE="disk" SIZE="7818182656" PTTYPE="gpt" FSTYPE="" LABEL="" PARTLABEL="""#,
        "\n",
        r#"NAME="/dev/mmcblk0p1" TYPE="part" SIZE="536870912" PTTYPE="gpt" FSTYPE="vfat" LABEL="EFI" PARTLABEL="EFI System""#,
        "\n",
        r#"NAME="/dev/mmcblk0p2" TYPE="part" SIZE="2147483648" PTTYPE="gpt" FSTYPE="ext4" LABEL="ThinOS" PARTLABEL="""#,
        "\n",
        r#"NAME="/dev/mmcblk0p3" TYPE="part" SIZE="1073741824" PTTYPE="gpt" FSTYPE="" LABEL="" PARTLABEL="""#,
        "\n",
        r#"NAME="/dev/mmcblk0p4" TYPE="part" SIZE="1073741824" PTTYPE="gpt" FSTYPE="swap" LABEL="" PARTLABEL="""#,
        "\n",
    );

    const BLANK: &str = r#"NAME="/dev/mmcblk0" TYPE="disk" SIZE="7818182656" PTTYPE="" FSTYPE="" LABEL="" PARTLABEL="""#;

    fn mac() -> MacAddress {
        MAC.parse().unwrap()
    }

    fn used() -> DiskReport {
        DiskReport::parse(USED).unwrap()
    }

    fn blank() -> DiskReport {
        DiskReport::parse(BLANK).unwrap()
    }

    #[test]
    fn a_used_disk_report_parses_into_the_disk_and_its_partitions() {
        let report = used();
        assert_eq!(report.name, "/dev/mmcblk0");
        assert_eq!(report.size, 7_818_182_656);
        assert_eq!(report.table.as_deref(), Some("gpt"));
        assert_eq!(report.fstype, None);
        assert_eq!(report.volumes.len(), 4);
        assert_eq!(report.volumes[1].fstype.as_deref(), Some("ext4"));
        assert_eq!(report.volumes[1].label.as_deref(), Some("ThinOS"));
        assert_eq!(report.volumes[0].partlabel.as_deref(), Some("EFI System"));
        assert_eq!(report.volumes[2].fstype, None);
        assert!(!report.is_blank());
    }

    #[test]
    fn a_disk_with_nothing_on_it_is_blank() {
        assert!(blank().is_blank());
        assert_eq!(blank().volumes, []);
    }

    #[test]
    fn a_filesystem_on_the_whole_disk_or_a_bare_table_is_not_blank() {
        let whole = DiskReport::parse(
            r#"NAME="/dev/sda" TYPE="disk" SIZE="8000000000" PTTYPE="" FSTYPE="ext4" LABEL="data" PARTLABEL="""#,
        )
        .unwrap();
        assert!(!whole.is_blank());
        let table = DiskReport::parse(
            r#"NAME="/dev/sda" TYPE="disk" SIZE="8000000000" PTTYPE="gpt" FSTYPE="" LABEL="" PARTLABEL="""#,
        )
        .unwrap();
        assert!(!table.is_blank());
    }

    #[test]
    fn lsblk_escapes_are_decoded() {
        let report = DiskReport::parse(concat!(
            r#"NAME="/dev/sda" TYPE="disk" SIZE="1" PTTYPE="dos" FSTYPE="" LABEL="" PARTLABEL="""#,
            "\n",
            r#"NAME="/dev/sda1" TYPE="part" SIZE="1" PTTYPE="dos" FSTYPE="ntfs" LABEL="My\x20\x22Data\x22" PARTLABEL="""#,
        ))
        .unwrap();
        assert_eq!(report.volumes[0].label.as_deref(), Some("My \"Data\""));
    }

    #[test]
    fn malformed_reports_are_refused() {
        assert_eq!(DiskReport::parse(""), Err(ReportError::Empty));
        assert_eq!(DiskReport::parse("\n\n"), Err(ReportError::Empty));
        assert_eq!(
            DiskReport::parse(r#"NAME="/dev/sda1" TYPE="part" SIZE="1""#),
            Err(ReportError::NoDisk)
        );
        assert_eq!(
            DiskReport::parse(r#"NAME="/dev/sda" TYPE="disk" SIZE="big""#),
            Err(ReportError::Size("big".to_string()))
        );
        for bad in [
            r#"NAME="/dev/sda TYPE="disk""#,
            r#"NAME=/dev/sda TYPE="disk" SIZE="1""#,
            r#"NAME="/dev/sda" TYPE="disk" SIZE="1" LABEL="\x2""#,
            "just some text",
        ] {
            assert!(
                matches!(DiskReport::parse(bad), Err(ReportError::Syntax { .. })),
                "{bad}"
            );
        }
    }

    #[test]
    fn the_summary_shows_size_table_and_what_each_partition_holds() {
        assert_eq!(
            used().summary(),
            r#"/dev/mmcblk0, 7.8 GB, gpt, 4 partitions: vfat "EFI", ext4 "ThinOS", (no filesystem), swap"#
        );
        assert_eq!(blank().summary(), "/dev/mmcblk0, 7.8 GB, blank");
    }

    #[test]
    fn the_summary_escapes_control_characters_in_labels() {
        let report = DiskReport::parse(concat!(
            r#"NAME="/dev/sda" TYPE="disk" SIZE="1000000000" PTTYPE="gpt" FSTYPE="" LABEL="" PARTLABEL="""#,
            "\n",
            r#"NAME="/dev/sda1" TYPE="part" SIZE="1" PTTYPE="gpt" FSTYPE="ext4" LABEL="\x1b[2J" PARTLABEL="""#,
        ))
        .unwrap();
        let summary = report.summary();
        assert!(!summary.contains('\x1b'), "{summary:?}");
        assert!(summary.contains(r#"ext4 "\u{1b}[2J""#), "{summary}");
    }

    #[test]
    fn only_y_or_yes_is_a_yes() {
        for yes in ["y", "Y", "yes", "YES", " yes \n"] {
            assert_eq!(parse_answer(yes), Answer::Yes, "{yes:?}");
        }
        for no in ["", "\n", "n", "no", "yep", "y es", "wipe"] {
            assert_eq!(parse_answer(no), Answer::No, "{no:?}");
        }
    }

    /// The decision table, row by row.
    #[test]
    fn the_decision_table() {
        let listed = [mac()];
        let other: MacAddress = "6c:4b:90:00:00:01".parse().unwrap();
        let rows: [(&str, DiskReport, &[MacAddress], Answer, WipeDecision); 9] = [
            (
                "blank",
                blank(),
                &[],
                Answer::NoTerminal,
                WipeDecision::Install,
            ),
            (
                "blank, listed",
                blank(),
                &listed,
                Answer::No,
                WipeDecision::Install,
            ),
            (
                "used + yes",
                used(),
                &[],
                Answer::Yes,
                WipeDecision::WipeAndInstall,
            ),
            (
                "used + listed",
                used(),
                &listed,
                Answer::NoTerminal,
                WipeDecision::WipeAndInstall,
            ),
            (
                "used + listed + no",
                used(),
                &listed,
                Answer::No,
                WipeDecision::WipeAndInstall,
            ),
            ("used + no", used(), &[], Answer::No, WipeDecision::Decline),
            (
                "used + no answer",
                used(),
                &[],
                Answer::NoAnswer,
                WipeDecision::Decline,
            ),
            (
                "used, no terminal, unlisted",
                used(),
                &[other],
                Answer::NoTerminal,
                WipeDecision::Decline,
            ),
            (
                "used, no terminal, nothing listed",
                used(),
                &[],
                Answer::NoTerminal,
                WipeDecision::Decline,
            ),
        ];
        for (name, report, pre_approved, answer, want) in rows {
            assert_eq!(
                decide(&report, Some(mac()), pre_approved, answer),
                want,
                "{name}"
            );
        }
    }

    #[test]
    fn a_machine_without_a_mac_is_never_pre_approved() {
        assert_eq!(
            decide(&used(), None, &[mac()], Answer::NoTerminal),
            WipeDecision::Decline
        );
        assert_eq!(
            decide(&used(), None, &[mac()], Answer::Yes),
            WipeDecision::WipeAndInstall
        );
    }

    #[test]
    fn only_a_used_unlisted_disk_needs_a_question() {
        assert_eq!(
            without_asking(&blank(), Some(mac()), &[]),
            Some(WipeDecision::Install)
        );
        assert_eq!(
            without_asking(&used(), Some(mac()), &[mac()]),
            Some(WipeDecision::WipeAndInstall)
        );
        assert_eq!(without_asking(&used(), Some(mac()), &[]), None);
    }

    #[test]
    fn decisions_have_the_words_the_installer_reads() {
        assert_eq!(WipeDecision::Install.word(), "install");
        assert_eq!(WipeDecision::WipeAndInstall.word(), "wipe");
        assert_eq!(WipeDecision::Decline.word(), "decline");
    }

    #[test]
    fn a_session_tracks_tickets_and_remembers_declined_machines() {
        let mut session = DiskSession::default();
        let machine = BootClient::from_query(Some(MAC), None, None);
        let first = session.open();
        let second = session.open();
        assert_ne!(first, second);
        assert!(first.len() >= 16 && first.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(session.decision(&first), Some(None));
        assert_eq!(session.decision("nope"), None);

        session.settle(&first, &machine, WipeDecision::WipeAndInstall);
        assert_eq!(
            session.decision(&first),
            Some(Some(WipeDecision::WipeAndInstall))
        );
        assert!(!session.is_declined(&machine));

        session.settle(&second, &machine, WipeDecision::Decline);
        assert!(session.is_declined(&machine));
        let same_mac_other_uuid = BootClient::from_query(
            Some(MAC),
            Some("4c4c4544-0042-3510-8051-b4c04f4b4e32"),
            None,
        );
        assert!(session.is_declined(&same_mac_other_uuid));
        let stranger = BootClient::from_query(Some("6c:4b:90:00:00:01"), None, None);
        assert!(!session.is_declined(&stranger));
        assert!(!session.is_declined(&BootClient::default()));
    }

    fn question(deadline: Instant) -> (Question, oneshot::Receiver<Answer>) {
        let (reply, answer) = oneshot::channel();
        let question = Question {
            machine: MAC.to_string(),
            summary: used().summary(),
            deadline,
            reply,
        };
        (question, answer)
    }

    #[tokio::test]
    async fn the_operator_is_asked_and_their_answer_goes_back() {
        let (ask, questions) = mpsc::unbounded_channel();
        let (type_line, lines) = mpsc::unbounded_channel();
        let printed = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = printed.clone();
        tokio::spawn(ask_operator(questions, lines, move |line| {
            sink.lock().unwrap().push(line)
        }));
        let (q, answer) = question(Instant::now() + Duration::from_secs(30));
        ask.send(q).unwrap();
        // Wait until the prompt is out, then answer.
        while printed.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
        type_line.send("y".to_string()).unwrap();
        assert_eq!(answer.await.unwrap(), Answer::Yes);
        let printed = printed.lock().unwrap();
        assert!(
            printed[0].starts_with(&format!("{MAC}: /dev/mmcblk0, 7.8 GB, gpt, 4 partitions")),
            "{printed:?}"
        );
        assert!(printed[0].ends_with("wipe? [y/N]"), "{printed:?}");
    }

    #[tokio::test]
    async fn lines_typed_before_the_question_do_not_answer_it() {
        let (ask, questions) = mpsc::unbounded_channel();
        let (type_line, lines) = mpsc::unbounded_channel();
        type_line.send("y".to_string()).unwrap();
        let printed = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let sink = printed.clone();
        tokio::spawn(ask_operator(questions, lines, move |line| {
            sink.lock().unwrap().push(line)
        }));
        let (q, answer) = question(Instant::now() + Duration::from_secs(30));
        ask.send(q).unwrap();
        while printed.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
        type_line.send("n".to_string()).unwrap();
        assert_eq!(answer.await.unwrap(), Answer::No);
    }

    #[tokio::test(start_paused = true)]
    async fn no_answer_before_the_deadline_is_no_answer() {
        let (ask, questions) = mpsc::unbounded_channel();
        let (_type_line, lines) = mpsc::unbounded_channel();
        tokio::spawn(ask_operator(questions, lines, |_| {}));
        let (q, answer) = question(Instant::now() + Duration::from_secs(600));
        ask.send(q).unwrap();
        assert_eq!(answer.await.unwrap(), Answer::NoAnswer);
    }

    #[tokio::test]
    async fn a_question_queued_past_its_deadline_is_not_shown() {
        let (ask, questions) = mpsc::unbounded_channel();
        let (_type_line, lines) = mpsc::unbounded_channel();
        let printed = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let sink = printed.clone();
        let (q, answer) = question(Instant::now());
        ask.send(q).unwrap();
        tokio::spawn(ask_operator(questions, lines, move |line| {
            sink.lock().unwrap().push(line)
        }));
        assert_eq!(answer.await.unwrap(), Answer::NoAnswer);
        let printed = printed.lock().unwrap();
        assert!(
            printed.iter().all(|l| !l.contains("wipe? [y/N]")),
            "{printed:?}"
        );
    }
}
