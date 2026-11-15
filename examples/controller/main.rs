//! A small controller written against Reliaburger's public API only (#1125).
//!
//! It keeps one of two apps running: a cold-standby pair such as `db-a`
//! pinned to one rack and `db-b` pinned to another. Only the active app
//! exists. When it has had no running instance for `CONTROLLER_THRESHOLD`
//! checks in a row, the controller fails over: it applies the standby and
//! deletes the app that failed. The only state it keeps is a counter;
//! which app is active lives in the cluster, as the one that exists.
//!
//! The pair is two `[app.*]` tables (`pair.toml`), or a template with a
//! `[use.*]` for each app (`postgres-pair.toml`), in which case the
//! controller applies the active `use` and the cluster expands it.
//!
//! It touches nothing but HTTP and JSON:
//!
//! - `GET /v1/diagnostics/apps`: each app and its owner;
//! - `GET /v1/status?cluster=true`: every instance and its state;
//! - `GET /v1/events?follow=true`: a live feed, to check again early;
//! - `POST /v1/apply`: the active app, owned by this controller's token;
//! - `POST /v1/delete/{app}/{namespace}`: the app it failed over from.
//!
//! The `reliaburger` crate isn't a dependency of this file, on purpose: the
//! contract is the JSON, documented in the manual's "Writing a controller"
//! chapter. Run it with a token scoped to the controller's namespace:
//!
//! ```sh
//! relish token create --name db-failover --role deployer --namespaces pg
//! RELIABURGER_TOKEN=rbrg_... CONTROLLER_NAME=db-failover \
//!   CONTROLLER_NAMESPACE=pg CONTROLLER_MANIFEST=examples/controller/pair.toml \
//!   cargo run --example controller
//! ```

use std::collections::BTreeMap;
use std::time::Duration;

use futures_util::StreamExt;
use serde::Deserialize;

/// What the controller needs to know, read from the environment.
#[derive(Debug, Clone)]
pub struct Settings {
    /// The Bun API, like `relish`'s `RELIABURGER_ENDPOINT`.
    pub endpoint: String,
    /// The controller's API token.
    pub token: String,
    /// The cluster CA in PEM, for an `https://` endpoint, like `relish`'s
    /// `RELIABURGER_CA_CERT`.
    pub ca_cert: Option<String>,
    /// The token's name: the `owner` the controller writes.
    pub name: String,
    /// The namespace the token is scoped to.
    pub namespace: String,
    /// The two apps, as an `[app.*]` TOML manifest with exactly two apps,
    /// or a template with exactly two `[use.*]` tables. The controller sets
    /// their `namespace` and `owner`.
    pub manifest: String,
    /// How many checks in a row the active app may have no running
    /// instance before the controller fails over.
    pub threshold: u32,
}

impl Settings {
    /// Read the settings from `RELIABURGER_ENDPOINT`, `RELIABURGER_TOKEN`,
    /// `RELIABURGER_CA_CERT` (a path), `CONTROLLER_NAME`,
    /// `CONTROLLER_NAMESPACE`, `CONTROLLER_MANIFEST` (a path) and
    /// `CONTROLLER_THRESHOLD`.
    pub fn from_env() -> Result<Self, String> {
        let required = |name: &str| std::env::var(name).map_err(|_| format!("{name} is not set"));
        let manifest_path = required("CONTROLLER_MANIFEST")?;
        let manifest = std::fs::read_to_string(&manifest_path)
            .map_err(|error| format!("reading {manifest_path}: {error}"))?;
        let ca_cert = match std::env::var("RELIABURGER_CA_CERT") {
            Ok(path) => Some(
                std::fs::read_to_string(&path)
                    .map_err(|error| format!("reading {path}: {error}"))?,
            ),
            Err(_) => None,
        };
        Ok(Self {
            endpoint: std::env::var("RELIABURGER_ENDPOINT")
                .unwrap_or_else(|_| "http://127.0.0.1:9117".to_string()),
            token: required("RELIABURGER_TOKEN")?,
            ca_cert,
            name: required("CONTROLLER_NAME")?,
            namespace: std::env::var("CONTROLLER_NAMESPACE")
                .unwrap_or_else(|_| "default".to_string()),
            manifest,
            threshold: std::env::var("CONTROLLER_THRESHOLD")
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(3),
        })
    }
}

/// One row of `GET /v1/diagnostics/apps`, the fields this controller reads.
#[derive(Debug, Clone, Deserialize)]
pub struct DesiredApp {
    pub app: String,
    pub namespace: String,
    #[serde(default)]
    pub owner: Option<String>,
}

/// One row of `GET /v1/status?cluster=true`, the fields this controller
/// reads.
#[derive(Debug, Clone, Deserialize)]
pub struct Instance {
    pub app_name: String,
    pub namespace: String,
    pub state: String,
}

/// What the controller knows about one of its two apps.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AppView {
    /// Whether the app is in desired state at all.
    pub exists: bool,
    /// Its owner, if any.
    pub owner: Option<String>,
    /// How many of its instances are `running`, which means healthy.
    pub running: usize,
}

/// What one check decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Apply app `active`, owned by the controller, and delete the other
    /// one if the controller owns it.
    Write { active: usize, reason: String },
    /// Leave everything as it is.
    Wait { reason: String },
    /// Someone else owns one of the apps: the controller won't fight.
    Blocked { reason: String },
}

/// Decide what to do, from the two apps' views and how many checks in a
/// row the active app has had no running instance (`misses`, counting this
/// one). Pure, so the rules can be tested without a cluster.
pub fn decide(views: [&AppView; 2], misses: u32, settings: &Settings) -> Decision {
    for view in views {
        if let Some(owner) = &view.owner
            && owner != &settings.name
        {
            return Decision::Blocked {
                reason: format!("an app is owned by {owner:?}, not by {:?}", settings.name),
            };
        }
    }
    let existing: Vec<usize> = (0..2).filter(|&index| views[index].exists).collect();
    let current = match existing[..] {
        [] => {
            return Decision::Write {
                active: 0,
                reason: "neither app exists; starting the first".to_string(),
            };
        }
        [only] => only,
        // Both exist, say after a failover that applied one app but didn't
        // get to delete the other: keep the one that runs, the first if
        // both or neither do.
        _ => {
            let active = usize::from(views[0].running == 0 && views[1].running > 0);
            return Decision::Write {
                active,
                reason: "both apps exist; keeping one".to_string(),
            };
        }
    };
    if views[current].owner.is_none() {
        return Decision::Write {
            active: current,
            reason: "claiming the active app".to_string(),
        };
    }
    if views[current].running > 0 {
        return Decision::Wait {
            reason: "the active app is running".to_string(),
        };
    }
    if misses < settings.threshold {
        return Decision::Wait {
            reason: format!(
                "the active app has no running instance ({misses} of {} checks)",
                settings.threshold
            ),
        };
    }
    Decision::Write {
        active: 1 - current,
        reason: format!("the active app had no running instance for {misses} checks; failing over"),
    }
}

/// The manifest the controller applies: app `active` from `settings`, in
/// its namespace, owned by its token.
///
/// A manifest with `[use.*]` tables is a templated pair: the controller
/// keeps the active `use`, and passes its token's name and its namespace
/// as the `owner` and `namespace` parameters. The cluster expands it.
pub fn manifest(settings: &Settings, active: usize) -> Result<String, String> {
    let mut document: toml::Table = settings
        .manifest
        .parse()
        .map_err(|error| format!("the manifest isn't TOML: {error}"))?;
    if let Some(uses) = document.get_mut("use").and_then(toml::Value::as_table_mut) {
        if uses.len() != 2 {
            return Err(format!("the manifest needs two uses, not {}", uses.len()));
        }
        let names: Vec<String> = uses.keys().cloned().collect();
        uses.remove(&names[1 - active]);
        let chosen = uses
            .get_mut(&names[active])
            .and_then(toml::Value::as_table_mut)
            .ok_or("a use isn't a table")?;
        chosen.insert("owner".into(), toml::Value::String(settings.name.clone()));
        chosen.insert(
            "namespace".into(),
            toml::Value::String(settings.namespace.clone()),
        );
        return toml::to_string(&document).map_err(|error| error.to_string());
    }
    let apps = document
        .get_mut("app")
        .and_then(toml::Value::as_table_mut)
        .ok_or("the manifest has no [app.*] tables")?;
    if apps.len() != 2 {
        return Err(format!("the manifest needs two apps, not {}", apps.len()));
    }
    let names: Vec<String> = apps.keys().cloned().collect();
    let (name, app) = apps
        .remove_entry(&names[active])
        .ok_or("the active app went missing")?;
    let mut app = app.as_table().cloned().ok_or("an app isn't a table")?;
    app.insert(
        "namespace".into(),
        toml::Value::String(settings.namespace.clone()),
    );
    app.insert("owner".into(), toml::Value::String(settings.name.clone()));
    let mut only = toml::Table::new();
    only.insert(name, toml::Value::Table(app));
    document.insert("app".into(), toml::Value::Table(only));
    toml::to_string(&document).map_err(|error| error.to_string())
}

/// The two app names in the manifest, in its order. In a templated pair,
/// each `use` names its app with its `name` parameter.
pub fn app_names(settings: &Settings) -> Result<[String; 2], String> {
    let document: toml::Table = settings
        .manifest
        .parse()
        .map_err(|error| format!("the manifest isn't TOML: {error}"))?;
    if let Some(uses) = document.get("use").and_then(toml::Value::as_table) {
        let names = uses
            .iter()
            .map(|(key, value)| {
                value
                    .get("name")
                    .and_then(toml::Value::as_str)
                    .map(str::to_string)
                    .ok_or(format!("use.{key} has no `name` parameter naming its app"))
            })
            .collect::<Result<Vec<String>, String>>()?;
        return names.try_into().map_err(|names: Vec<String>| {
            format!("the manifest needs two uses, not {}", names.len())
        });
    }
    let names: Vec<String> = document
        .get("app")
        .and_then(toml::Value::as_table)
        .map(|apps| apps.keys().cloned().collect())
        .unwrap_or_default();
    names
        .try_into()
        .map_err(|names: Vec<String>| format!("the manifest needs two apps, not {}", names.len()))
}

/// A thin client for the four calls the controller makes.
pub struct Api {
    http: reqwest::Client,
    settings: Settings,
}

impl Api {
    /// A client for `settings.endpoint`, presenting `settings.token`.
    pub fn new(settings: Settings) -> Result<Self, String> {
        // reqwest is built without a default TLS crypto provider, so pick
        // one. A second call finds it installed, which is fine.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let mut builder = reqwest::Client::builder().connect_timeout(Duration::from_secs(10));
        if let Some(pem) = &settings.ca_cert {
            let certificate = reqwest::Certificate::from_pem(pem.as_bytes())
                .map_err(|error| format!("the CA certificate: {error}"))?;
            builder = builder.add_root_certificate(certificate);
        }
        let http = builder.build().map_err(|error| error.to_string())?;
        Ok(Self { http, settings })
    }

    async fn get<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T, String> {
        let response = self
            .http
            .get(format!("{}{path}", self.settings.endpoint))
            .bearer_auth(&self.settings.token)
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .map_err(|error| format!("GET {path}: {error}"))?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(format!("GET {path}: {status}: {body}"));
        }
        response
            .json()
            .await
            .map_err(|error| format!("GET {path}: {error}"))
    }

    /// Apply `manifest`, and read the streamed outcome to its end.
    async fn apply(&self, manifest: String) -> Result<Vec<String>, String> {
        let response = self
            .http
            .post(format!("{}/v1/apply", self.settings.endpoint))
            .bearer_auth(&self.settings.token)
            .timeout(Duration::from_secs(120))
            .body(manifest)
            .send()
            .await
            .map_err(|error| format!("POST /v1/apply: {error}"))?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(format!("POST /v1/apply: {status}: {body}"));
        }
        let mut messages = Vec::new();
        for event in body.lines().filter_map(|line| line.strip_prefix("data:")) {
            let event: serde_json::Value =
                serde_json::from_str(event.trim()).map_err(|error| error.to_string())?;
            let message = event["message"].as_str().unwrap_or_default().to_string();
            match event["type"].as_str() {
                Some("Error") => return Err(message),
                Some("Progress") => messages.push(message),
                _ => {}
            }
        }
        Ok(messages)
    }

    /// Delete `app`, which this controller owns.
    async fn delete(&self, app: &str) -> Result<(), String> {
        let path = format!("/v1/delete/{app}/{}", self.settings.namespace);
        let response = self
            .http
            .post(format!("{}{path}", self.settings.endpoint))
            .bearer_auth(&self.settings.token)
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .map_err(|error| format!("POST {path}: {error}"))?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(format!("POST {path}: {status}: {body}"));
        }
        Ok(())
    }

    /// The two apps as the cluster sees them.
    async fn views(&self, names: &[String; 2]) -> Result<[AppView; 2], String> {
        let desired: Vec<DesiredApp> = self.get("/v1/diagnostics/apps").await?;
        let instances: Vec<Instance> = self.get("/v1/status?cluster=true").await?;
        Ok(names.clone().map(|name| {
            let row = desired
                .iter()
                .find(|row| row.app == name && row.namespace == self.settings.namespace);
            AppView {
                exists: row.is_some(),
                owner: row.and_then(|row| row.owner.clone()),
                running: instances
                    .iter()
                    .filter(|instance| {
                        instance.app_name == name
                            && instance.namespace == self.settings.namespace
                            && instance.state == "running"
                    })
                    .count(),
            }
        }))
    }
}

/// The controller's one piece of memory: how many checks in a row the
/// active app has had no running instance.
#[derive(Debug, Default)]
pub struct Controller {
    misses: u32,
}

impl Controller {
    /// Check once and act. Returns what was decided.
    pub async fn reconcile(&mut self, api: &Api) -> Result<Decision, String> {
        let names = app_names(&api.settings)?;
        let views = api.views(&names).await?;
        let active_down = views.iter().any(|view| view.exists && view.running == 0);
        self.misses = if active_down { self.misses + 1 } else { 0 };
        let decision = decide([&views[0], &views[1]], self.misses, &api.settings);
        if let Decision::Write { active, .. } = &decision {
            for message in api.apply(manifest(&api.settings, *active)?).await? {
                eprintln!("controller: {message}");
            }
            let standby = &views[1 - active];
            if standby.exists && standby.owner.as_deref() == Some(api.settings.name.as_str()) {
                api.delete(&names[1 - active]).await?;
            }
            self.misses = 0;
        }
        Ok(decision)
    }
}

/// Follow the event stream and send a wake-up for every event about one of
/// `names`. The stream is a hint to check early; the periodic check is what
/// the controller relies on, so a dropped stream is reopened, not fatal.
async fn follow_events(
    http: reqwest::Client,
    settings: Settings,
    names: [String; 2],
    wake: tokio::sync::mpsc::Sender<()>,
) {
    loop {
        let request = http
            .get(format!("{}/v1/events?follow=true", settings.endpoint))
            .bearer_auth(&settings.token);
        if let Ok(response) = request.send().await
            && response.status().is_success()
        {
            let mut body = response.bytes_stream();
            let mut buffer = String::new();
            while let Some(Ok(chunk)) = body.next().await {
                buffer.push_str(&String::from_utf8_lossy(&chunk));
                while let Some(end) = buffer.find('\n') {
                    let line: String = buffer.drain(..=end).collect();
                    let Some(data) = line.strip_prefix("data:") else {
                        continue;
                    };
                    let event: BTreeMap<String, serde_json::Value> =
                        serde_json::from_str(data.trim()).unwrap_or_default();
                    let app = event.get("app").and_then(serde_json::Value::as_str);
                    if app.is_some_and(|app| names.iter().any(|name| name == app)) {
                        let _ = wake.try_send(());
                    }
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

#[tokio::main]
async fn main() {
    let settings = match Settings::from_env() {
        Ok(settings) => settings,
        Err(error) => {
            eprintln!("controller: {error}");
            std::process::exit(2);
        }
    };
    let names = match app_names(&settings) {
        Ok(names) => names,
        Err(error) => {
            eprintln!("controller: {error}");
            std::process::exit(2);
        }
    };
    let api = match Api::new(settings.clone()) {
        Ok(api) => api,
        Err(error) => {
            eprintln!("controller: {error}");
            std::process::exit(2);
        }
    };
    let (wake_tx, mut wake_rx) = tokio::sync::mpsc::channel(1);
    tokio::spawn(follow_events(api.http.clone(), settings, names, wake_tx));
    let mut controller = Controller::default();
    let mut tick = tokio::time::interval(Duration::from_secs(5));
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            _ = wake_rx.recv() => {}
        }
        match controller.reconcile(&api).await {
            Ok(Decision::Wait { .. }) => {}
            Ok(decision) => eprintln!("controller: {decision:?}"),
            Err(error) => eprintln!("controller: {error}"),
        }
    }
}
