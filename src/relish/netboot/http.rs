//! The HTTP half: boot scripts, the installer and the disk image.
//!
//! iPXE fetches `/boot.ipxe?mac=…&uuid=…&arch=…` and gets the install
//! script or `exit 1` ([`super::installed`]). The install script fetches
//! `/<buildarch>/installer.efi`, and the installer then fetches the disk
//! image and its signed `SHA256SUMS` from the same directory. Only files
//! [`super::artefacts::load`] checked are served, and only while they're
//! unchanged on disk. `<buildarch>` is iPXE's name: `arm64` is served from
//! the `aarch64/` directory.

use std::net::Ipv4Addr;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures_util::StreamExt;
use serde::Deserialize;
use tokio::sync::Mutex;
use tokio_util::io::ReaderStream;

use super::artefacts::{Artefacts, ServedFile};
use super::installed::{
    BootClient, BootDecision, EXIT_SCRIPT, InstalledRecord, NOT_ALLOWED_SCRIPT, decide,
    install_script,
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
        .route("/{arch}/{file}", get(artefact))
        .with_state(state)
}

async fn boot_script(
    State(state): State<Arc<HttpState>>,
    Query(query): Query<BootQuery>,
) -> Response {
    let client = query.client();
    let decision = {
        let record = state.installed.lock().await;
        decide(&client, &record, &state.allowed, state.reinstall)
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
    };
    let arch = client.arch.map_or("unknown architecture", Arch::ipxe_name);
    (state.log)(format!("http: {client} ({arch}): boot.ipxe, {what}"));
    (
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        script,
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
    (state.log)(format!("http: {client}: {arch}/{file}"));
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
}
