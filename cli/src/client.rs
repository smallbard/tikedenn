//! Client http de l'API `/api/v1` (authentification par token).

use crate::config::Config;
use anyhow::{anyhow, Result};
use serde::{de::DeserializeOwned, Serialize};
use std::io::Read;
use std::time::Duration;
use tikedenn_common::api::*;

#[derive(Clone)]
pub struct Client {
    base: String,
    token: String,
    agent: ureq::Agent,
}

/// Erreur renvoyée par le serveur (statut + message).
#[derive(Debug)]
pub struct ApiFailure {
    pub status: u16,
    pub message: String,
}

impl std::fmt::Display for ApiFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} (HTTP {})", self.message, self.status)
    }
}

impl std::error::Error for ApiFailure {}

/// Statut http d'une erreur de l'API (None pour une erreur réseau, etc.).
pub fn status_of(e: &anyhow::Error) -> Option<u16> {
    e.downcast_ref::<ApiFailure>().map(|f| f.status)
}

impl Client {
    pub fn new(cfg: &Config) -> Result<Client> {
        cfg.require_server()?;
        Ok(Client {
            base: format!("{}/api/v1", cfg.url),
            token: cfg.token.clone(),
            agent: ureq::AgentBuilder::new().timeout(Duration::from_secs(60)).build(),
        })
    }

    fn request(&self, method: &str, path: &str) -> ureq::Request {
        self.agent
            .request(method, &format!("{}{path}", self.base))
            .set("Authorization", &format!("Bearer {}", self.token))
    }

    /// Convertit une réponse en erreur lisible (message JSON du serveur).
    fn check(r: std::result::Result<ureq::Response, ureq::Error>) -> Result<ureq::Response> {
        match r {
            Ok(resp) => Ok(resp),
            Err(ureq::Error::Status(status, resp)) => {
                let body = resp.into_string().unwrap_or_default();
                let message = serde_json::from_str::<ApiErrorBody>(&body)
                    .map(|b| b.error)
                    .unwrap_or(body);
                Err(ApiFailure { status, message }.into())
            }
            Err(e) => Err(anyhow!("Could not reach Tikedenn: {e}")),
        }
    }

    fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        Ok(Self::check(self.request("GET", path).call())?.into_json()?)
    }

    fn send<B: Serialize, T: DeserializeOwned>(&self, method: &str, path: &str, body: &B) -> Result<T> {
        Ok(Self::check(self.request(method, path).send_json(body))?.into_json()?)
    }

    pub fn me(&self) -> Result<Me> {
        self.get("/me")
    }

    pub fn projects(&self, repo: Option<&str>) -> Result<Vec<Project>> {
        match repo {
            Some(r) => Ok(Self::check(self.request("GET", "/projects").query("repo", r).call())?.into_json()?),
            None => self.get("/projects"),
        }
    }

    pub fn tasks(&self, status: Option<&str>, project: Option<&str>) -> Result<Vec<TaskSummary>> {
        let mut req = self.request("GET", "/tasks");
        if let Some(s) = status {
            req = req.query("status", s);
        }
        if let Some(p) = project {
            req = req.query("project", p);
        }
        Ok(Self::check(req.call())?.into_json()?)
    }

    pub fn task(&self, id: i64) -> Result<TaskDetail> {
        self.get(&format!("/tasks/{id}"))
    }

    pub fn create(&self, c: &CreateTask) -> Result<TaskDetail> {
        self.send("POST", "/tasks", c)
    }

    pub fn set_branch(&self, id: i64, branch: &str) -> Result<TaskDetail> {
        self.update(id, &PatchTask { branch: Some(branch.into()), ..Default::default() })
    }

    pub fn update(&self, id: i64, p: &PatchTask) -> Result<TaskDetail> {
        self.send("PATCH", &format!("/tasks/{id}"), p)
    }

    pub fn claim(&self, id: i64, ttl_minutes: Option<i64>) -> Result<TaskDetail> {
        self.send("POST", &format!("/tasks/{id}/claim"), &Claim { ttl_minutes })
    }

    /// Prochaine tâche prise en charge, ou None (204) s'il n'y en a pas.
    pub fn claim_next(&self, c: &ClaimNext) -> Result<Option<TaskDetail>> {
        let resp = Self::check(self.request("POST", "/tasks/claim-next").send_json(c))?;
        if resp.status() == 204 {
            return Ok(None);
        }
        Ok(Some(resp.into_json()?))
    }

    pub fn release(&self, id: i64, status: Option<&str>, note: Option<&str>) -> Result<TaskDetail> {
        let r = Release { status: status.map(String::from), note: note.map(String::from) };
        self.send("POST", &format!("/tasks/{id}/release"), &r)
    }

    pub fn move_to(&self, id: i64, status: &str, note: Option<&str>) -> Result<TaskDetail> {
        let m = MoveTask { status: status.into(), note: note.map(String::from) };
        self.send("POST", &format!("/tasks/{id}/move"), &m)
    }

    pub fn comment(&self, id: i64, body: &str, kind: &str) -> Result<()> {
        let c = NewComment { body: body.into(), kind: Some(kind.into()) };
        Self::check(self.request("POST", &format!("/tasks/{id}/comments")).send_json(c))?;
        Ok(())
    }

    pub fn attach(&self, id: i64, name: &str, mime: &str, data: &[u8]) -> Result<TaskDetail> {
        let path = format!("/tasks/{id}/files/{}", urlencode(name));
        let resp = Self::check(self.request("PUT", &path).set("Content-Type", mime).send_bytes(data))?;
        Ok(resp.into_json()?)
    }

    /// Contenu d'un fichier joint.
    pub fn download(&self, file_id: i64) -> Result<Vec<u8>> {
        let resp = Self::check(self.request("GET", &format!("/files/{file_id}")).call())?;
        let mut data = vec![];
        resp.into_reader().read_to_end(&mut data)?;
        Ok(data)
    }
}


/// Encodage en pourcents d'un segment de chemin.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}
