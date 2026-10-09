//! The HTTP half: boot scripts, the installer and the disk image.
//!
//! iPXE fetches `/boot.ipxe?mac=…&uuid=…&arch=…` and gets the install
//! script or `exit 1` ([`super::installed`]). The install script fetches
//! `/<buildarch>/installer.efi`, and the installer then fetches the disk
//! image and its signed `SHA256SUMS` from the same directory. Only files
//! [`super::artefacts::load`] checked are served, and only while they're
//! unchanged on disk. `<buildarch>` is iPXE's name: `arm64` is served from
//! the `aarch64/` directory.
//!
//! An installer that finds a used disk reports it with `POST /disk` and
//! polls `GET /disk/<ticket>` for the operator's decision ([`super::wipe`]).

use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use futures_util::StreamExt;
use serde::Deserialize;
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::time::Instant;
use tokio_util::io::ReaderStream;

use super::artefacts::{Artefacts, ServedFile};
use super::installed::{
    BootClient, BootDecision, DECLINED_SCRIPT, EXIT_SCRIPT, InstalledRecord, NOT_ALLOWED_SCRIPT,
    decide, install_script,
};
use super::wipe::{
    self, Answer, DiskReport, DiskSession, MAX_REPORT_BYTES, Question, WipeDecision,
};
use super::{Arch, Log, MacAddress};

/// What the HTTP handlers share.
pub struct HttpState {
    /// The checked files.
    pub artefacts: Arc<Artefacts>,
    /// This server's address, written into the install script.
    pub server: Ipv4Addr,
    /// This server's HTTP port, written into the install script.
    pub port: u16,
    /// Machines that installed already.
    pub installed: Mutex<InstalledRecord>,
    /// When not empty, only these machines may install.
    pub allowed: Vec<MacAddress>,
    /// Install again even on machines in the record.
    pub reinstall: bool,
    /// Machines whose used disk may be wiped without asking (`--wipe`).
    pub wipe: Vec<MacAddress>,
    /// Where questions for the operator go; `None` when stdin isn't a
    /// terminal, so only `--wipe` machines are wiped.
    pub operator: Option<mpsc::UnboundedSender<Question>>,
    /// How long a question waits for the operator.
    pub question_timeout: Duration,
    /// The disk questions asked this session, and their answers.
    pub disks: Mutex<DiskSession>,
    /// Where request lines go.
    pub log: Log,
}

/// The query iPXE adds to its requests.
#[derive(Debug, Default, Deserialize)]
pub struct BootQuery {
    /// `${netX/mac}`.
    pub mac: Option<String>,
    /// `${uuid}`.
    pub uuid: Option<String>,
    /// `${buildarch}`.
    pub arch: Option<String>,
}

impl BootQuery {
    fn client(&self) -> BootClient {
        BootClient::from_query(
            self.mac.as_deref(),
            self.uuid.as_deref(),
            self.arch.as_deref(),
        )
    }
}

/// The HTTP routes.
pub fn router(state: Arc<HttpState>) -> Router {
    Router::new()
        .route("/boot.ipxe", get(boot_script))
        .route("/disk", post(report_disk))
        .route("/disk/{ticket}", get(disk_decision))
        .route("/{arch}/{file}", get(artefact))
        .with_state(state)
}

async fn boot_script(
    State(state): State<Arc<HttpState>>,
    Query(query): Query<BootQuery>,
) -> Response {
    let client = query.client();
    let declined = state.disks.lock().await.is_declined(&client);
    let decision = {
        let record = state.installed.lock().await;
        decide(&client, &record, &state.allowed, state.reinstall, declined)
    };
    let (script, what) = match decision {
        BootDecision::Install => (
            install_script(state.server, state.port, &client),
            "install".to_string(),
        ),
        BootDecision::AlreadyInstalled => (
            EXIT_SCRIPT.to_string(),
            "exit (installed already; --reinstall to install again)".to_string(),
        ),
        BootDecision::NotAllowed => (
            NOT_ALLOWED_SCRIPT.to_string(),
            "exit (not on the --mac list)".to_string(),
        ),
        BootDecision::Declined => (
            DECLINED_SCRIPT.to_string(),
            "exit (you kept its disk this session)".to_string(),
        ),
    };
    let arch = client.arch.map_or("unknown architecture", Arch::ipxe_name);
    (state.log)(format!("http: {client} ({arch}): boot.ipxe, {what}"));
    (
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        script,
    )
        .into_response()
}

/// `POST /disk?mac=…&uuid=…`: the installer reports a disk that isn't
/// blank. The answer is a ticket to poll, never a decision: deciding is
/// left to [`settle_disk`], which only the operator or `--wipe` can steer.
async fn report_disk(
    State(state): State<Arc<HttpState>>,
    Query(query): Query<BootQuery>,
    body: Bytes,
) -> Response {
    let client = query.client();
    if client.mac.is_none() && client.uuid.is_none() {
        return (StatusCode::BAD_REQUEST, "say which machine: ?mac=…\n").into_response();
    }
    if !state.allowed.is_empty() && !client.mac.is_some_and(|mac| state.allowed.contains(&mac)) {
        (state.log)(format!(
            "http: {client}: disk report refused (not on the --mac list)"
        ));
        return (StatusCode::FORBIDDEN, "not on the --mac list\n").into_response();
    }
    let report = if body.len() > MAX_REPORT_BYTES {
        Err("the report is too big".to_string())
    } else {
        DiskReport::parse(&String::from_utf8_lossy(&body)).map_err(|e| e.to_string())
    };
    let report = match report {
        Ok(report) => report,
        Err(error) => {
            (state.log)(format!("http: {client}: bad disk report: {error}"));
            return (StatusCode::BAD_REQUEST, format!("{error}\n")).into_response();
        }
    };
    let ticket = state.disks.lock().await.open();
    tokio::spawn(settle_disk(state.clone(), ticket.clone(), client, report));
    (StatusCode::ACCEPTED, format!("{ticket}\n")).into_response()
}

/// Decide what happens to a reported disk: at once for a blank disk, a
/// `--wipe` machine or one declined already this session; otherwise ask
/// the operator, if there's a terminal to ask on.
async fn settle_disk(
    state: Arc<HttpState>,
    ticket: String,
    client: BootClient,
    report: DiskReport,
) {
    let summary = report.summary();
    let already_declined = state.disks.lock().await.is_declined(&client);
    let (decision, why) = if already_declined {
        (WipeDecision::Decline, "declined earlier this session")
    } else if let Some(decision) = wipe::without_asking(&report, client.mac, &state.wipe) {
        let why = match decision {
            WipeDecision::WipeAndInstall => "--wipe lists it",
            _ => "nothing on it",
        };
        (decision, why)
    } else if let Some(operator) = &state.operator {
        let (reply, answer) = oneshot::channel();
        let question = Question {
            machine: client.to_string(),
            summary: summary.clone(),
            deadline: Instant::now() + state.question_timeout,
            reply,
        };
        let answer = if operator.send(question).is_ok() {
            answer.await.unwrap_or(Answer::NoAnswer)
        } else {
            Answer::NoAnswer
        };
        let why = match answer {
            Answer::Yes => "you said yes",
            Answer::No => "you said no",
            Answer::NoAnswer | Answer::NoTerminal => "no answer in time",
        };
        (wipe::decide(&report, client.mac, &state.wipe, answer), why)
    } else {
        let decision = wipe::decide(&report, client.mac, &state.wipe, Answer::NoTerminal);
        (
            decision,
            "no terminal to ask on, and --wipe doesn't list it",
        )
    };
    let what = match decision {
        WipeDecision::Install => "installing",
        WipeDecision::WipeAndInstall => "wiping it and installing",
        WipeDecision::Decline => "leaving it alone",
    };
    (state.log)(format!("http: {client}: disk {summary}: {what} ({why})"));
    if decision == WipeDecision::Decline {
        // It fetched the installer, but installed nothing: forget it, so a
        // later session asks again instead of sending it to its old disk.
        if let Err(e) = state.installed.lock().await.forget(&client) {
            (state.log)(format!("http: couldn't forget {client}: {e}"));
        }
    }
    state.disks.lock().await.settle(&ticket, &client, decision);
}

/// `GET /disk/<ticket>`: `wait` until there's a decision, then `install`,
/// `wipe` or `decline`. Read-only: nothing a machine sends here counts.
async fn disk_decision(
    State(state): State<Arc<HttpState>>,
    Path(ticket): Path<String>,
) -> Response {
    let word = match state.disks.lock().await.decision(&ticket) {
        None => return (StatusCode::NOT_FOUND, "no such ticket\n").into_response(),
        Some(None) => "wait",
        Some(Some(decision)) => decision.word(),
    };
    (
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        format!("{word}\n"),
    )
        .into_response()
}

async fn artefact(
    State(state): State<Arc<HttpState>>,
    Path((arch, file)): Path<(String, String)>,
    Query(query): Query<BootQuery>,
) -> Response {
    let Some(served) = find(&state.artefacts, &arch, &file) else {
        return (StatusCode::NOT_FOUND, "not served here\n").into_response();
    };
    if !served.unchanged() {
        (state.log)(format!(
            "http: refusing {}: it changed after relish netboot checked it; restart to check it again",
            served.path.display()
        ));
        return (StatusCode::CONFLICT, "changed since it was checked\n").into_response();
    }
    let opened = match tokio::fs::File::open(&served.path).await {
        Ok(opened) => opened,
        Err(e) => {
            (state.log)(format!("http: {}: {e}", served.path.display()));
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let client = query.client();
    // iPXE names the machine; the installer's own downloads (the image and
    // its sums) don't, so they're logged by file alone.
    (state.log)(match client.mac {
        Some(_) => format!("http: {client}: {arch}/{file}"),
        None => format!("http: {arch}/{file}"),
    });
    let stream = ReaderStream::new(opened);
    let body = if file == "installer.efi" {
        // Remember the machine as the installer's last chunk goes out; a
        // download that stops halfway never gets there. It has to happen
        // *before* that chunk is yielded: with Content-Length set, hyper
        // stops polling the body once the last byte is sent, so anything
        // chained after it would never run.
        let size = served.size;
        let mut sent = 0u64;
        let state = state.clone();
        Body::from_stream(stream.then(move |chunk| {
            let last = chunk.as_ref().is_ok_and(|bytes| {
                sent += bytes.len() as u64;
                sent >= size
            });
            let state = state.clone();
            let client = client.clone();
            async move {
                if last {
                    remember(state, client).await;
                }
                chunk
            }
        }))
    } else {
        Body::from_stream(stream)
    };
    (
        [
            (header::CONTENT_TYPE, content_type(&file).to_string()),
            (header::CONTENT_LENGTH, served.size.to_string()),
        ],
        body,
    )
        .into_response()
}

/// The checked file `/<arch>/<file>` names, if any.
fn find<'a>(artefacts: &'a Artefacts, arch: &str, file: &str) -> Option<&'a ServedFile> {
    let release = artefacts.architectures.get(&Arch::from_ipxe_name(arch)?)?;
    if file == "installer.efi" {
        release.installer()
    } else {
        release.files.get(file)
    }
}

fn content_type(file: &str) -> &'static str {
    if file.ends_with(".efi") {
        "application/efi"
    } else if file.ends_with(".ipxe") || file.ends_with(".SHA256SUMS") {
        "text/plain; charset=utf-8"
    } else {
        "application/octet-stream"
    }
}

async fn remember(state: Arc<HttpState>, client: BootClient) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let result = state.installed.lock().await.remember(&client, now);
    match result {
        Ok(true) => (state.log)(format!(
            "http: {client} has the installer; it gets exit from now on (--reinstall to install again)"
        )),
        Ok(false) => {}
        Err(e) => (state.log)(format!("http: couldn't remember {client}: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::super::artefacts::{self, fixture};
    use super::super::installed::RECORD_FILE;
    use super::*;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    const MAC: &str = "52:54:00:12:34:56";

    struct Fixture {
        dir: tempfile::TempDir,
        state: Arc<HttpState>,
        lines: Arc<std::sync::Mutex<Vec<String>>>,
    }

    fn fixture_with(allowed: Vec<MacAddress>, reinstall: bool) -> Fixture {
        fixture_asking(allowed, reinstall, vec![], None, Duration::from_secs(30))
    }

    fn fixture_asking(
        allowed: Vec<MacAddress>,
        reinstall: bool,
        wipe: Vec<MacAddress>,
        operator: Option<mpsc::UnboundedSender<Question>>,
        question_timeout: Duration,
    ) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let (pkcs8, key) = crate::upgrade::signing::generate_keypair().unwrap();
        fixture::write_signed(
            dir.path(),
            "x86_64",
            &fixture::borrowed(&fixture::release("x86_64")),
            &pkcs8,
        );
        fixture::write_signed(
            dir.path(),
            "aarch64",
            &fixture::borrowed(&fixture::release("arm64")),
            &pkcs8,
        );
        let artefacts = artefacts::load(dir.path(), &[key], |_| {}).unwrap();
        let lines = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = lines.clone();
        let state = Arc::new(HttpState {
            artefacts: Arc::new(artefacts),
            server: Ipv4Addr::new(192, 168, 1, 20),
            port: 8080,
            installed: Mutex::new(InstalledRecord::load(&dir.path().join(RECORD_FILE)).unwrap()),
            allowed,
            reinstall,
            wipe,
            operator,
            question_timeout,
            disks: Mutex::new(DiskSession::default()),
            log: Arc::new(move |line| sink.lock().unwrap().push(line)),
        });
        Fixture { dir, state, lines }
    }

    async fn get_path(
        state: &Arc<HttpState>,
        uri: &str,
    ) -> (StatusCode, Option<String>, Option<String>, Vec<u8>) {
        let response = router(state.clone())
            .oneshot(Request::get(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let header = |name| {
            response
                .headers()
                .get(name)
                .map(|v: &axum::http::HeaderValue| v.to_str().unwrap().to_string())
        };
        let content_type = header(header::CONTENT_TYPE);
        let length = header(header::CONTENT_LENGTH);
        let body = response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec();
        (status, content_type, length, body)
    }

    fn boot_uri(mac: &str) -> String {
        format!("/boot.ipxe?mac={mac}&uuid=4c4c4544-0042-3510-8051-b4c04f4b4e32&arch=x86_64")
    }

    #[tokio::test]
    async fn the_installer_is_served_as_efi_with_its_length_under_both_arch_names() {
        let f = fixture_with(vec![], false);
        let (status, content_type, length, body) =
            get_path(&f.state, "/x86_64/installer.efi").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(content_type.as_deref(), Some("application/efi"));
        assert_eq!(length.as_deref(), Some("9"));
        assert_eq!(body, b"installer");
        let (status, ..) = get_path(&f.state, "/arm64/installer.efi").await;
        assert_eq!(status, StatusCode::OK);
        let (status, ..) = get_path(&f.state, "/aarch64/installer.efi").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn the_disk_image_sums_and_signature_are_served_but_nothing_else() {
        let f = fixture_with(vec![], false);
        let version = fixture::VERSION;
        for file in [
            format!("reliaburger-os_{version}.raw.zst"),
            format!("reliaburger-os_{version}.SHA256SUMS"),
            format!("reliaburger-os_{version}.SHA256SUMS.sig"),
            "ipxe-snp-x86_64.efi".to_string(),
        ] {
            let (status, ..) = get_path(&f.state, &format!("/x86_64/{file}")).await;
            assert_eq!(status, StatusCode::OK, "{file}");
        }
        std::fs::write(f.dir.path().join("x86_64/secret.txt"), b"no").unwrap();
        for path in [
            "/x86_64/secret.txt",
            "/x86_64/netboot",
            "/x86_64/..%2Fx86_64%2Fsecret.txt",
            "/x86_64/netboot-installed.json",
            "/i386/installer.efi",
            "/x86_64",
        ] {
            let (status, ..) = get_path(&f.state, path).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
        }
    }

    #[tokio::test]
    async fn a_file_replaced_after_the_check_is_not_served() {
        let f = fixture_with(vec![], false);
        std::fs::write(
            f.dir.path().join(format!(
                "x86_64/reliaburger-os_{}.raw.zst",
                fixture::VERSION
            )),
            b"swapped in later",
        )
        .unwrap();
        let (status, ..) = get_path(
            &f.state,
            &format!("/x86_64/reliaburger-os_{}.raw.zst", fixture::VERSION),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn a_new_machine_gets_the_install_script_then_exit_once_it_has_the_installer() {
        let f = fixture_with(vec![], false);
        let (status, _, _, script) = get_path(&f.state, &boot_uri(MAC)).await;
        assert_eq!(status, StatusCode::OK);
        let script = String::from_utf8(script).unwrap();
        assert!(script.contains("set base http://192.168.1.20:8080/${buildarch}"));
        assert!(script.contains(
            "${base}/installer.efi?mac=52:54:00:12:34:56&uuid=4c4c4544-0042-3510-8051-b4c04f4b4e32 "
        ));

        let (status, ..) = get_path(
            &f.state,
            "/x86_64/installer.efi?mac=52:54:00:12:34:56&uuid=4c4c4544-0042-3510-8051-b4c04f4b4e32",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (_, _, _, script) = get_path(&f.state, &boot_uri(MAC)).await;
        assert_eq!(script, EXIT_SCRIPT.as_bytes());

        let record = std::fs::read_to_string(f.dir.path().join(RECORD_FILE)).unwrap();
        assert!(record.contains(MAC), "{record}");
        let lines = f.lines.lock().unwrap();
        assert!(
            lines.iter().any(|l| l.contains("has the installer")),
            "{lines:?}"
        );
    }

    #[tokio::test]
    async fn a_download_that_names_no_machine_is_logged_by_file_alone() {
        let f = fixture_with(vec![], false);
        let image = format!("/x86_64/reliaburger-os_{}.raw.zst", fixture::VERSION);
        let (status, ..) = get_path(&f.state, &image).await;
        assert_eq!(status, StatusCode::OK);
        let lines = f.lines.lock().unwrap();
        assert!(
            lines
                .iter()
                .any(|l| l == &format!("http: x86_64{}", &image[7..])),
            "{lines:?}"
        );
        assert!(
            !lines.iter().any(|l| l.contains("unknown MAC")),
            "{lines:?}"
        );
    }

    /// Over a real connection, as iPXE fetches it: hyper stops polling a
    /// body with a Content-Length once the last byte is out, which a
    /// `oneshot` test never shows.
    #[tokio::test]
    async fn a_machine_is_remembered_after_downloading_the_installer_over_tcp() {
        let f = fixture_with(vec![], false);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = router(f.state.clone());
        tokio::spawn(async move { axum::serve(listener, app).await });
        let response = reqwest::get(format!("http://{address}/x86_64/installer.efi?mac={MAC}"))
            .await
            .unwrap();
        assert!(response.status().is_success());
        response.bytes().await.unwrap();
        let record = std::fs::read_to_string(f.dir.path().join(RECORD_FILE)).unwrap();
        assert!(record.contains(MAC), "{record}");
    }

    #[tokio::test]
    async fn fetching_other_files_does_not_mark_a_machine_installed() {
        let f = fixture_with(vec![], false);
        get_path(&f.state, &format!("/x86_64/ipxe-snp-x86_64.efi?mac={MAC}")).await;
        let (_, _, _, script) = get_path(&f.state, &boot_uri(MAC)).await;
        assert_ne!(script, EXIT_SCRIPT.as_bytes());
    }

    #[tokio::test]
    async fn reinstall_serves_the_install_script_to_installed_machines() {
        let f = fixture_with(vec![], true);
        get_path(&f.state, &format!("/x86_64/installer.efi?mac={MAC}")).await;
        let (_, _, _, script) = get_path(&f.state, &boot_uri(MAC)).await;
        assert!(String::from_utf8(script).unwrap().contains("installer.efi"));
    }

    #[tokio::test]
    async fn a_machine_off_the_mac_list_gets_exit() {
        let f = fixture_with(vec![MAC.parse().unwrap()], false);
        let (_, _, _, script) = get_path(&f.state, &boot_uri("52:54:00:00:00:01")).await;
        assert_eq!(script, NOT_ALLOWED_SCRIPT.as_bytes());
        let (_, _, _, script) = get_path(&f.state, &boot_uri(MAC)).await;
        assert!(String::from_utf8(script).unwrap().contains("installer.efi"));
    }

    const USED: &str = concat!(
        r#"NAME="/dev/vda" TYPE="disk" SIZE="8589934592" PTTYPE="gpt" FSTYPE="" LABEL="" PARTLABEL="""#,
        "\n",
        r#"NAME="/dev/vda1" TYPE="part" SIZE="1048576" PTTYPE="gpt" FSTYPE="ext4" LABEL="ThinOS" PARTLABEL="""#,
        "\n",
    );
    const BLANK: &str = r#"NAME="/dev/vda" TYPE="disk" SIZE="8589934592" PTTYPE="" FSTYPE="" LABEL="" PARTLABEL="""#;

    async fn send(state: &Arc<HttpState>, request: Request<Body>) -> (StatusCode, String) {
        let response = router(state.clone()).oneshot(request).await.unwrap();
        let status = response.status();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8_lossy(&body).into_owned())
    }

    async fn report(state: &Arc<HttpState>, query: &str, body: &str) -> (StatusCode, String) {
        let request = Request::post(format!("/disk?{query}"))
            .body(Body::from(body.to_string()))
            .unwrap();
        send(state, request).await
    }

    async fn verdict(state: &Arc<HttpState>, ticket: &str) -> (StatusCode, String) {
        let request = Request::get(format!("/disk/{}", ticket.trim()))
            .body(Body::empty())
            .unwrap();
        send(state, request).await
    }

    /// Poll as the installer does until the answer isn't `wait`.
    async fn settled(state: &Arc<HttpState>, ticket: &str) -> String {
        for _ in 0..500 {
            let (status, word) = verdict(state, ticket).await;
            assert_eq!(status, StatusCode::OK);
            if word != "wait\n" {
                return word;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("no decision for ticket {ticket}");
    }

    type Prompts = Arc<std::sync::Mutex<Vec<String>>>;

    /// An operator at the terminal: questions are printed to the returned
    /// prompts, and whatever is sent on the returned channel is typed.
    fn operator() -> (
        mpsc::UnboundedSender<Question>,
        mpsc::UnboundedSender<String>,
        Prompts,
    ) {
        let (ask, questions) = mpsc::unbounded_channel();
        let (type_line, lines) = mpsc::unbounded_channel();
        let prompts = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = prompts.clone();
        tokio::spawn(crate::relish::netboot::wipe::ask_operator(
            questions,
            lines,
            move |line| sink.lock().unwrap().push(line),
        ));
        (ask, type_line, prompts)
    }

    async fn wait_for_prompt(prompts: &Prompts) -> String {
        loop {
            if let Some(prompt) = prompts.lock().unwrap().first() {
                return prompt.clone();
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// The whole exchange over a real connection, as the installer's curl
    /// does it: report, poll while the operator reads the question, see
    /// the yes arrive.
    #[tokio::test]
    async fn a_used_disk_is_reported_and_wiped_after_the_operators_yes_over_tcp() {
        let (ask, type_line, prompts) = operator();
        let f = fixture_asking(vec![], false, vec![], Some(ask), Duration::from_secs(30));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = router(f.state.clone());
        tokio::spawn(async move { axum::serve(listener, app).await });
        let client = reqwest::Client::new();

        let response = client
            .post(format!("http://{address}/disk?mac={MAC}"))
            .body(USED)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let ticket = response.text().await.unwrap().trim().to_string();
        let poll = || async {
            client
                .get(format!("http://{address}/disk/{ticket}"))
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap()
        };
        assert_eq!(poll().await, "wait\n");
        let prompt = wait_for_prompt(&prompts).await;
        assert!(
            prompt.starts_with(&format!(
                "{MAC}: /dev/vda, 8.6 GB, gpt, 1 partition: ext4 \"ThinOS\""
            )),
            "{prompt}"
        );
        assert!(prompt.ends_with("wipe? [y/N]"), "{prompt}");
        type_line.send("yes".to_string()).unwrap();
        let mut word = poll().await;
        for _ in 0..500 {
            if word != "wait\n" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
            word = poll().await;
        }
        assert_eq!(word, "wipe\n");
        let lines = f.lines.lock().unwrap();
        assert!(lines.iter().any(|l| l.contains("wiping")), "{lines:?}");
    }

    #[tokio::test]
    async fn a_no_leaves_the_disk_and_the_machine_gets_exit_for_the_rest_of_the_session() {
        let (ask, type_line, prompts) = operator();
        let f = fixture_asking(vec![], false, vec![], Some(ask), Duration::from_secs(30));
        // The machine downloaded the installer, so it's in the record...
        get_path(&f.state, &format!("/x86_64/installer.efi?mac={MAC}")).await;
        let (status, ticket) = report(&f.state, &format!("mac={MAC}"), USED).await;
        assert_eq!(status, StatusCode::ACCEPTED);
        wait_for_prompt(&prompts).await;
        type_line.send("n".to_string()).unwrap();
        assert_eq!(settled(&f.state, &ticket).await, "decline\n");
        // ...and leaves it, since it installed nothing.
        let record = std::fs::read_to_string(f.dir.path().join(RECORD_FILE)).unwrap();
        assert!(!record.contains(MAC), "{record}");
        let (_, _, _, script) = get_path(&f.state, &boot_uri(MAC)).await;
        assert_eq!(script, DECLINED_SCRIPT.as_bytes());
        // Reporting again doesn't ask again.
        let (_, again) = report(&f.state, &format!("mac={MAC}"), USED).await;
        assert_eq!(settled(&f.state, &again).await, "decline\n");
        assert_eq!(prompts.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn no_answer_in_time_declines() {
        let (ask, _type_line, _prompts) = operator();
        let f = fixture_asking(vec![], false, vec![], Some(ask), Duration::from_millis(50));
        let (_, ticket) = report(&f.state, &format!("mac={MAC}"), USED).await;
        assert_eq!(settled(&f.state, &ticket).await, "decline\n");
    }

    #[tokio::test]
    async fn a_listed_machine_is_wiped_without_a_question() {
        let (ask, _type_line, prompts) = operator();
        let f = fixture_asking(
            vec![],
            false,
            vec![MAC.parse().unwrap()],
            Some(ask),
            Duration::from_secs(30),
        );
        let (_, ticket) = report(&f.state, &format!("mac={MAC}"), USED).await;
        assert_eq!(settled(&f.state, &ticket).await, "wipe\n");
        assert!(prompts.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn without_a_terminal_only_listed_machines_are_wiped() {
        let f = fixture_asking(
            vec![],
            false,
            vec!["52:54:00:00:00:09".parse().unwrap()],
            None,
            Duration::from_secs(30),
        );
        let (_, ticket) = report(&f.state, &format!("mac={MAC}"), USED).await;
        assert_eq!(settled(&f.state, &ticket).await, "decline\n");
        let lines = f.lines.lock().unwrap();
        assert!(lines.iter().any(|l| l.contains("--wipe")), "{lines:?}");
    }

    #[tokio::test]
    async fn a_blank_disk_installs_without_a_question() {
        let f = fixture_with(vec![], false);
        let (_, ticket) = report(&f.state, &format!("mac={MAC}"), BLANK).await;
        assert_eq!(settled(&f.state, &ticket).await, "install\n");
    }

    /// Nothing a machine sends can decide: the report endpoint takes only
    /// lsblk output, and the decision endpoint only reads.
    #[tokio::test]
    async fn a_machine_cannot_answer_its_own_question() {
        let f = fixture_asking(
            vec![MAC.parse().unwrap()],
            false,
            vec![],
            None,
            Duration::from_secs(30),
        );
        let (status, ticket) = report(&f.state, &format!("mac={MAC}"), USED).await;
        assert_eq!(status, StatusCode::ACCEPTED);
        for method in ["POST", "PUT", "PATCH", "DELETE"] {
            let request = Request::builder()
                .method(method)
                .uri(format!("/disk/{}", ticket.trim()))
                .body(Body::from("wipe"))
                .unwrap();
            let (status, _) = send(&f.state, request).await;
            assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED, "{method}");
        }
        assert_eq!(settled(&f.state, &ticket).await, "decline\n");
        // A report that's a decision, not a disk, is refused.
        let (status, _) = report(&f.state, &format!("mac={MAC}"), "wipe\n").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        // So is a report nobody can be asked about, or from a machine the
        // --mac list leaves out, or one too big to be lsblk's.
        let (status, _) = report(&f.state, "", USED).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _) = report(&f.state, "mac=52:54:00:00:00:01", USED).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, _) = report(&f.state, &format!("mac={MAC}"), &"x".repeat(70_000)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _) = verdict(&f.state, "0123456789abcdef").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
}
