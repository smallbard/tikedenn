//! Types JSON de l'API `/api/v1` (sérialisés par le serveur, lus par `tkd`).

use serde::{Deserialize, Serialize};

/// Identité associée au token utilisé.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Me {
    pub username: String,
    /// Nom du token (identité de l'agent, ex. « claude@laptop »).
    pub token: String,
    /// « read » ou « write ».
    pub scope: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Project {
    pub id: i64,
    pub name: String,
    pub git_repo: String,
    /// Branche de base des diffs et des déploiements (vide = main, sinon master).
    pub default_branch: String,
    /// Nom du projet pour `publish <nom>` (vide = pas de publication).
    pub publish_name: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct TaskSummary {
    pub id: i64,
    pub title: String,
    pub status: String,
    pub priority: i64,
    pub project_id: Option<i64>,
    pub project: Option<String>,
    pub branch: String,
    /// Détenteur de la prise en charge (« utilisateur/agent »), même expirée.
    pub claimed_by: Option<String>,
    /// Fin du bail de prise en charge (UTC, `AAAA-MM-JJ HH:MM:SS`).
    pub claimed_until: Option<String>,
    /// Vrai si le bail est encore valide.
    pub claim_active: bool,
    /// Dépendances pas encore terminées (la tâche est bloquée tant qu'il en reste).
    pub blocked_by: Vec<i64>,
    pub updated_at: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct FileInfo {
    pub id: i64,
    pub name: String,
    pub size: i64,
    pub mime: String,
}

/// Entrée du journal d'une tâche (commentaire, rapport d'agent, revue,
/// changement d'état, prise en charge, log de déploiement).
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct LogEntry {
    pub id: i64,
    pub author: String,
    /// Nom de l'agent (token) si l'entrée vient de l'API, sinon absent.
    pub agent: Option<String>,
    pub kind: String,
    pub body: String,
    pub created_at: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct TaskDetail {
    #[serde(flatten)]
    pub summary: TaskSummary,
    pub description: String,
    /// Critères d'acceptation.
    pub acceptance: String,
    pub created_by: Option<String>,
    pub created_at: String,
    /// Toutes les dépendances (terminées ou non).
    pub depends_on: Vec<i64>,
    pub repo: Option<Project>,
    pub files: Vec<FileInfo>,
    /// Journal, du plus ancien au plus récent.
    pub log: Vec<LogEntry>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct CreateTask {
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub acceptance: String,
    /// Id ou nom du projet.
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub priority: Option<i64>,
    #[serde(default)]
    pub branch: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct PatchTask {
    #[serde(default)]
    pub branch: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct MoveTask {
    pub status: String,
    /// Rapport joint au changement d'état (résumé du travail, erreur…).
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct NewComment {
    pub body: String,
    /// « comment » (défaut), « report » ou « deploy ».
    #[serde(default)]
    pub kind: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct Claim {
    /// Durée du bail en minutes (défaut 60, max 24 h).
    #[serde(default)]
    pub ttl_minutes: Option<i64>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct ClaimNext {
    /// Projets acceptés (ids ou noms) ; vide = tous.
    #[serde(default)]
    pub projects: Vec<String>,
    /// « todo » (défaut) ou « deploy ».
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub ttl_minutes: Option<i64>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct Release {
    /// État cible optionnel (ex. « todo » pour rendre la tâche).
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
}

/// Corps d'une réponse d'erreur.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ApiErrorBody {
    pub error: String,
}
