//! Pièces jointes des tâches (BLOB en BDD, 10 Mo max).

use crate::auth::CurrentUser;
use crate::util::urlencode;
use crate::web::{AppError, AppResult};
use crate::AppState;
use axum::{
    body::Body,
    extract::{multipart::Multipart, Extension, Path, State},
    http::{header, StatusCode},
    response::{IntoResponse, Redirect},
};
use rusqlite::OptionalExtension;

/// Taille maximale d'une pièce jointe (10 Mo).
const MAX_FILE_SIZE: usize = 10 * 1024 * 1024;

/// Téléverse un fichier joint sur une tâche (multipart), puis revient sur
/// la page d'édition. La connexion du pool n'est retenue que pour la
/// vérification de propriété et l'INSERT — jamais pendant la lecture du
/// corps (uploads lents).
pub async fn upload_file(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Path(task_id): Path<i64>,
    mut multipart: Multipart,
) -> AppResult {
    let owned = state
        .conn()?
        .query_row(
            "SELECT 1 FROM tasks WHERE id = ?1 AND user_id = ?2",
            rusqlite::params![task_id, user.id],
            |_| Ok(()),
        )
        .optional()?;
    if owned.is_none() {
        return Err(AppError::not_found("Task not found."));
    }
    // Erreur multipart (corps tronqué par la limite de taille, etc.) : ne pas
    // laisser passer comme un upload vide.
    let bad_upload =
        |_| AppError::Http(StatusCode::BAD_REQUEST, "Upload failed (request body too large or malformed).");
    let mut filter = 0i64;
    while let Some(field) = multipart.next_field().await.map_err(bad_upload)? {
        if field.name() == Some("filter") {
            filter = field.text().await.ok().and_then(|t| t.parse().ok()).unwrap_or(0);
            continue;
        }
        let file_name = match field.file_name().map(str::trim) {
            Some(n) if !n.is_empty() => n.to_string(),
            _ => continue,
        };
        let mime = field.content_type().unwrap_or("").to_string();
        let data = field.bytes().await.map_err(bad_upload)?;
        if data.len() > MAX_FILE_SIZE {
            return Err(AppError::Http(
                StatusCode::UNPROCESSABLE_ENTITY,
                "File too large (10 MB maximum).",
            ));
        }
        state.conn()?.execute(
            "INSERT INTO files (task_id, name, size, mime, data) VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![task_id, file_name, data.len() as i64, mime, data.as_ref()],
        )?;
        // Un seul fichier par formulaire ; le champ `filter` le précède.
        return Ok(Redirect::to(&format!("/tasks/{task_id}/edit?project={filter}")).into_response());
    }
    Err(AppError::Http(StatusCode::UNPROCESSABLE_ENTITY, "No file uploaded."))
}

/// En-tête `Content-Disposition` d'un téléchargement : nom ASCII de repli
/// (`filename=`) + nom exact encodé en UTF-8 (`filename*=`, RFC 6266) — un
/// en-tête http ne peut pas contenir de non-ASCII brut.
fn content_disposition(name: &str) -> String {
    let fallback: String = name
        .chars()
        .map(|c| {
            if c == ' ' || (c.is_ascii_graphic() && c != '"' && c != '\\') {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!(
        "attachment; filename=\"{fallback}\"; filename*=UTF-8''{}",
        urlencode(name)
    )
}

/// Téléchargement d'un fichier joint (404 s'il appartient à un autre utilisateur).
pub async fn download_file(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<i64>,
) -> AppResult {
    let (name, mime, data): (String, String, Vec<u8>) = state
        .conn()?
        .query_row(
            "SELECT f.name, f.mime, f.data
             FROM files f JOIN tasks t ON t.id = f.task_id
             WHERE f.id = ?1 AND t.user_id = ?2",
            rusqlite::params![id, user.id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?
        .ok_or(AppError::not_found("File not found."))?;
    let mut resp = Body::from(data).into_response();
    let headers = resp.headers_mut();
    let mime = if mime.is_empty() { "application/octet-stream" } else { &mime };
    if let Ok(hv) = mime.parse() {
        headers.insert(header::CONTENT_TYPE, hv);
    }
    if let Ok(hv) = content_disposition(&name).parse() {
        headers.insert(header::CONTENT_DISPOSITION, hv);
    }
    Ok(resp)
}

/// Supprime un fichier joint. Renvoie du vide (htmx remplace le `<li>`).
pub async fn delete_file(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<i64>,
) -> AppResult {
    state.conn()?.execute(
        "DELETE FROM files WHERE id = ?1 AND task_id IN (SELECT id FROM tasks WHERE user_id = ?2)",
        rusqlite::params![id, user.id],
    )?;
    Ok("".into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_disposition_is_ascii_with_utf8_name() {
        let h = content_disposition("résumé \"v2\".pdf");
        assert!(h.is_ascii());
        assert_eq!(
            h,
            "attachment; filename=\"r_sum_ _v2_.pdf\"; filename*=UTF-8''r%C3%A9sum%C3%A9%20%22v2%22.pdf"
        );
        assert!(axum::http::HeaderValue::from_str(&h).is_ok());
    }
}
