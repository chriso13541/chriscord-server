//! The signed member list (the app's roster.go): who may read each channel,
//! as signed by the server's owner and the approvers they chose.
//!
//! The server only stores it, keeps it in order and passes it on. It can't
//! change it: every app checks every entry itself and only gives channel
//! and call keys to the people the list allows — whatever this server says.
//! So nothing here needs trusting; the checks below just keep junk out.
//!
//!   GET    /api/roster[?after=<seq>]          entries after seq, and the open proposals
//!   POST   /api/roster                        {body, sigs}: add an entry (signatures checked)
//!   POST   /api/roster/proposals              {body, sigs}: propose a change, for approvers to sign
//!   POST   /api/roster/proposals/:id/sign     {key, sig}: add your approval
//!   POST   /api/roster/proposals/:id/reject   {key, sig}: turn it down (signed; the apps count it)
//!   DELETE /api/roster/proposals/:id          withdraw it (whoever proposed it, or the owner)
//!   DELETE /api/admin/roster                  start the list over (owner key): every app warns
//!
//! Every change is announced as {"type": "roster_changed"}.

use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde::Deserialize;
use sqlx::{Row, SqlitePool};
use std::sync::Arc;

use crate::{admin::check_owner, db, state::AppState};

type ApiErr = (StatusCode, Json<serde_json::Value>);

fn err(code: StatusCode, msg: &str) -> ApiErr { (code, Json(serde_json::json!({ "error": msg }))) }
fn dberr() -> ApiErr { err(StatusCode::INTERNAL_SERVER_ERROR, "DB error") }
fn bad(msg: &str) -> ApiErr { err(StatusCode::BAD_REQUEST, msg) }

const CONTEXT: &[u8] = b"chriscord-roster-v1\x00";
/// What a rejection signs (the app's rosterRejectMessage): never the same
/// message as an approval.
const REJECT_CONTEXT: &[u8] = b"chriscord-roster-reject-v1\x00";
const MAX_BODY: usize = 64 << 10;
const MAX_SIGS: usize = 64;
const MAX_PROPOSALS: i64 = 200;
const MAX_PAGE: i64 = 5000;

/// Changes to the list one at a time: each one's checks (is there a list
/// yet? is this for it?) and its write happen together, so two can't both
/// pass the checks and then both be stored (two starts, say).
static WRITE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

pub async fn init(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS roster_entries (
            seq        INTEGER PRIMARY KEY AUTOINCREMENT,
            op_id      TEXT NOT NULL UNIQUE,
            body       TEXT NOT NULL,       -- exactly as signed
            sigs       TEXT NOT NULL,       -- JSON [{key, sig}]
            added_by   TEXT NOT NULL,       -- who sent it here (a username)
            added_at   TEXT NOT NULL
        )",
    ).execute(pool).await?;
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS roster_proposals (
            op_id      TEXT PRIMARY KEY,
            body       TEXT NOT NULL,
            sigs       TEXT NOT NULL,
            proposer   TEXT NOT NULL,       -- identity key
            created_at TEXT NOT NULL
        )",
    ).execute(pool).await?;
    // Approvers' rejections (added later: ignore "duplicate column").
    let _ = sqlx::query("ALTER TABLE roster_proposals ADD COLUMN rejects TEXT NOT NULL DEFAULT '[]'").execute(pool).await;
    Ok(())
}

#[derive(Deserialize, serde::Serialize, Clone)]
pub struct Sig {
    key: String,
    sig: String,
}

/// The parts of a body the server looks at (the apps check the rest).
struct Head {
    v:      i64,
    roster: String,
    id:     String,
    by:     String,
    op:     String,
}

fn head(body: &str) -> Result<Head, ApiErr> {
    if body.len() > MAX_BODY { return Err(bad("That entry is too big")); }
    let v: serde_json::Value = serde_json::from_str(body).map_err(|_| bad("That entry isn't readable"))?;
    let s = |k: &str| v[k].as_str().unwrap_or("").to_string();
    let h = Head { v: v["v"].as_i64().unwrap_or(0), roster: s("roster"), id: s("id"), by: s("by").to_lowercase(), op: s("op") };
    if h.v != 1 || h.id.len() != 32 || !h.id.chars().all(|c| c.is_ascii_hexdigit()) || h.by.len() != 64 || h.op.is_empty() {
        return Err(bad("That isn't a member list entry"));
    }
    Ok(h)
}

/// The keys whose signatures over body hold up; Err if any doesn't.
fn check_sigs(body: &str, sigs: &[Sig]) -> Result<Vec<String>, ApiErr> { check_sigs_in(CONTEXT, body, sigs) }

fn check_sigs_in(context: &[u8], body: &str, sigs: &[Sig]) -> Result<Vec<String>, ApiErr> {
    if sigs.is_empty() || sigs.len() > MAX_SIGS { return Err(bad("Wrong number of signatures")); }
    let mut msg = context.to_vec();
    msg.extend_from_slice(body.as_bytes());
    let mut keys = Vec::new();
    for s in sigs {
        let key = s.key.to_lowercase();
        let ok = (|| {
            let pk: [u8; 32] = hex::decode(&key).ok()?.try_into().ok()?;
            let sg: [u8; 64] = hex::decode(&s.sig).ok()?.try_into().ok()?;
            VerifyingKey::from_bytes(&pk).ok()?.verify(&msg, &Signature::from_bytes(&sg)).ok()
        })();
        if ok.is_none() { return Err(bad("A signature doesn't match")); }
        if !keys.contains(&key) { keys.push(key); }
    }
    Ok(keys)
}

/// The signed-in member: (username, identity key).
async fn me(h: &HeaderMap, s: &AppState) -> Result<(String, String), ApiErr> {
    let token = h.get("X-Session-Token").and_then(|v| v.to_str().ok()).unwrap_or("");
    let user = db::verify_token(&s.pool, token).await.map_err(|_| dberr())?
        .ok_or_else(|| err(StatusCode::UNAUTHORIZED, "Unauthorized"))?;
    let key: String = sqlx::query("SELECT public_key FROM users WHERE username = ?").bind(&user)
        .fetch_optional(&s.pool).await.map_err(|_| dberr())?
        .ok_or_else(|| err(StatusCode::UNAUTHORIZED, "Unauthorized"))?.get("public_key");
    Ok((user, key.to_lowercase()))
}

fn changed(s: &AppState) {
    let _ = s.tx.send(serde_json::json!({ "type": "roster_changed" }).to_string());
}

/// A transaction that holds SQLite's write lock from its first statement.
/// One that reads first and writes after fails ("database is locked") if
/// anything else in the database was written in between — a message, say
/// — which made adding an entry or an approval fail at random on a busy
/// server. Taking the lock first, it waits its turn instead (busy timeout).
async fn begin_write(s: &AppState) -> Result<sqlx::Transaction<'static, sqlx::Sqlite>, ApiErr> {
    let mut tx = s.pool.begin().await.map_err(|_| dberr())?;
    sqlx::query("UPDATE roster_proposals SET op_id = op_id WHERE 0").execute(&mut *tx).await.map_err(|_| dberr())?;
    Ok(tx)
}

/// The list's id: its first entry's op id ("" if there's none yet).
async fn roster_id(conn: &mut sqlx::SqliteConnection) -> Result<String, ApiErr> {
    Ok(sqlx::query("SELECT op_id FROM roster_entries ORDER BY seq LIMIT 1").fetch_optional(conn).await
        .map_err(|_| dberr())?.map(|r| r.get("op_id")).unwrap_or_default())
}

fn sigs_json(sigs: &[Sig]) -> String { serde_json::to_string(sigs).unwrap_or_else(|_| "[]".into()) }

fn sigs_of(text: &str) -> serde_json::Value { serde_json::from_str(text).unwrap_or(serde_json::json!([])) }

#[derive(Deserialize)]
pub struct AfterQuery {
    #[serde(default)]
    after: i64,
}

/// GET /api/roster
pub async fn get(headers: HeaderMap, Query(q): Query<AfterQuery>, State(s): State<Arc<AppState>>) -> Result<Json<serde_json::Value>, ApiErr> {
    me(&headers, &s).await?;
    // One read transaction: the entries, the proposals and the list's id
    // all as of the same moment (not an id from after a reset alongside
    // entries from before it).
    let mut tx = s.pool.begin().await.map_err(|_| dberr())?;
    let rows = sqlx::query("SELECT seq, body, sigs FROM roster_entries WHERE seq > ? ORDER BY seq LIMIT ?")
        .bind(q.after).bind(MAX_PAGE).fetch_all(&mut *tx).await.map_err(|_| dberr())?;
    let entries: Vec<serde_json::Value> = rows.iter().map(|r| serde_json::json!({
        "seq": r.get::<i64, _>("seq"), "body": r.get::<String, _>("body"), "sigs": sigs_of(&r.get::<String, _>("sigs")),
    })).collect();
    let more = entries.len() as i64 == MAX_PAGE;
    let proposals: Vec<serde_json::Value> = sqlx::query("SELECT body, sigs, rejects, created_at FROM roster_proposals ORDER BY created_at")
        .fetch_all(&mut *tx).await.map_err(|_| dberr())?.iter().map(|r| serde_json::json!({
            "body": r.get::<String, _>("body"), "sigs": sigs_of(&r.get::<String, _>("sigs")),
            "rejects": sigs_of(&r.get::<String, _>("rejects")),
            "created_at": r.get::<String, _>("created_at"),
        })).collect();
    let id = roster_id(&mut *tx).await?;
    tx.commit().await.map_err(|_| dberr())?;
    Ok(Json(serde_json::json!({ "id": id, "entries": entries, "more": more, "proposals": proposals })))
}

#[derive(Deserialize)]
pub struct EntryReq {
    body: String,
    sigs: Vec<Sig>,
}

/// POST /api/roster — add an entry. The apps decide whether it holds up;
/// here: signatures that match, the right list, no repeats, and only the
/// owner starts one.
pub async fn append(headers: HeaderMap, State(s): State<Arc<AppState>>, Json(b): Json<EntryReq>) -> Result<Json<serde_json::Value>, ApiErr> {
    let (user, key) = me(&headers, &s).await?;
    let h = head(&b.body)?;
    let signers = check_sigs(&b.body, &b.sigs)?;
    if !signers.contains(&h.by) { return Err(bad("It has to be signed by whoever made it")); }
    let owner = crate::admin::owner_username(&s.pool).await;
    let _w = WRITE.lock().await;
    // The checks, the entry and its proposal's removal: all or nothing.
    let mut tx = begin_write(&s).await?;
    let id = roster_id(&mut *tx).await?;
    if id.is_empty() {
        if h.op != "start" || !h.roster.is_empty() || owner.as_deref() != Some(user.as_str()) || h.by != key {
            return Err(bad("Only the server's owner can start the member list"));
        }
    } else if h.roster != id || h.op == "start" {
        return Err(bad("That's for a different member list"));
    }
    let r = sqlx::query("INSERT OR IGNORE INTO roster_entries (op_id, body, sigs, added_by, added_at) VALUES (?, ?, ?, ?, ?)")
        .bind(&h.id).bind(&b.body).bind(sigs_json(&b.sigs)).bind(&user).bind(chrono::Utc::now().to_rfc3339())
        .execute(&mut *tx).await.map_err(|_| dberr())?;
    // Proposed first, now in the list: no longer waiting.
    sqlx::query("DELETE FROM roster_proposals WHERE op_id = ?").bind(&h.id).execute(&mut *tx).await.map_err(|_| dberr())?;
    let seq: i64 = sqlx::query("SELECT seq FROM roster_entries WHERE op_id = ?").bind(&h.id)
        .fetch_one(&mut *tx).await.map_err(|_| dberr())?.get("seq");
    tx.commit().await.map_err(|_| dberr())?;
    if r.rows_affected() > 0 {
        tracing::info!("roster: {user} added a {} entry", h.op);
    }
    changed(&s);
    Ok(Json(serde_json::json!({ "ok": true, "seq": seq })))
}

/// POST /api/roster/proposals — a change waiting for approvals.
pub async fn propose(headers: HeaderMap, State(s): State<Arc<AppState>>, Json(b): Json<EntryReq>) -> Result<Json<serde_json::Value>, ApiErr> {
    let (user, key) = me(&headers, &s).await?;
    let h = head(&b.body)?;
    let signers = check_sigs(&b.body, &b.sigs)?;
    if h.by != key || !signers.contains(&key) { return Err(bad("A proposal has to be yours, and signed")); }
    let _w = WRITE.lock().await;
    let mut tx = begin_write(&s).await?;
    let id = roster_id(&mut *tx).await?;
    if id.is_empty() || h.roster != id { return Err(bad("That's for a different member list")); }
    let n: i64 = sqlx::query("SELECT COUNT(*) AS n FROM roster_proposals").fetch_one(&mut *tx).await.map_err(|_| dberr())?.get("n");
    if n >= MAX_PROPOSALS { return Err(bad("Too many changes are waiting for approval — approve or withdraw some first")); }
    let applied: bool = sqlx::query("SELECT 1 FROM roster_entries WHERE op_id = ?").bind(&h.id)
        .fetch_optional(&mut *tx).await.map_err(|_| dberr())?.is_some();
    if applied { return Err(bad("That change is already in the member list")); }
    sqlx::query("INSERT OR IGNORE INTO roster_proposals (op_id, body, sigs, proposer, created_at) VALUES (?, ?, ?, ?, ?)")
        .bind(&h.id).bind(&b.body).bind(sigs_json(&b.sigs)).bind(&key).bind(chrono::Utc::now().to_rfc3339())
        .execute(&mut *tx).await.map_err(|_| dberr())?;
    tx.commit().await.map_err(|_| dberr())?;
    tracing::info!("roster: {user} proposed a {} change", h.op);
    changed(&s);
    Ok(Json(serde_json::json!({ "ok": true, "id": h.id })))
}

/// POST /api/roster/proposals/:id/sign — your approval.
pub async fn sign(headers: HeaderMap, Path(id): Path<String>, State(s): State<Arc<AppState>>, Json(sig): Json<Sig>) -> Result<Json<serde_json::Value>, ApiErr> {
    let (user, key) = me(&headers, &s).await?;
    if sig.key.to_lowercase() != key { return Err(bad("You can only add your own approval")); }
    let _w = WRITE.lock().await;
    let mut tx = begin_write(&s).await?;
    let row = sqlx::query("SELECT body, sigs FROM roster_proposals WHERE op_id = ?").bind(&id)
        .fetch_optional(&mut *tx).await.map_err(|_| dberr())?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "That change isn't waiting any more"))?;
    let body: String = row.get("body");
    check_sigs(&body, std::slice::from_ref(&sig))?;
    let mut sigs: Vec<Sig> = serde_json::from_str(&row.get::<String, _>("sigs")).unwrap_or_default();
    if !sigs.iter().any(|x| x.key.to_lowercase() == key) {
        if sigs.len() >= MAX_SIGS { return Err(bad("Too many signatures")); }
        sigs.push(Sig { key: key.clone(), sig: sig.sig });
    }
    sqlx::query("UPDATE roster_proposals SET sigs = ? WHERE op_id = ?").bind(sigs_json(&sigs)).bind(&id)
        .execute(&mut *tx).await.map_err(|_| dberr())?;
    tx.commit().await.map_err(|_| dberr())?;
    tracing::info!("roster: {user} approved a change");
    changed(&s);
    Ok(Json(serde_json::json!({ "ok": true, "sigs": sigs })))
}

/// POST /api/roster/proposals/:id/reject — you turn it down. Only kept
/// here: every app checks the signature and whether you're an approver,
/// and shows it as turned down once it can't get the approvals it needs.
pub async fn reject(headers: HeaderMap, Path(id): Path<String>, State(s): State<Arc<AppState>>, Json(sig): Json<Sig>) -> Result<Json<serde_json::Value>, ApiErr> {
    let (user, key) = me(&headers, &s).await?;
    if sig.key.to_lowercase() != key { return Err(bad("You can only turn it down yourself")); }
    let _w = WRITE.lock().await;
    let mut tx = begin_write(&s).await?;
    let row = sqlx::query("SELECT body, rejects FROM roster_proposals WHERE op_id = ?").bind(&id)
        .fetch_optional(&mut *tx).await.map_err(|_| dberr())?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "That change isn't waiting any more"))?;
    let body: String = row.get("body");
    check_sigs_in(REJECT_CONTEXT, &body, std::slice::from_ref(&sig))?;
    let mut rejects: Vec<Sig> = serde_json::from_str(&row.get::<String, _>("rejects")).unwrap_or_default();
    if !rejects.iter().any(|x| x.key.to_lowercase() == key) {
        if rejects.len() >= MAX_SIGS { return Err(bad("Too many signatures")); }
        rejects.push(Sig { key: key.clone(), sig: sig.sig });
    }
    sqlx::query("UPDATE roster_proposals SET rejects = ? WHERE op_id = ?").bind(sigs_json(&rejects)).bind(&id)
        .execute(&mut *tx).await.map_err(|_| dberr())?;
    tx.commit().await.map_err(|_| dberr())?;
    tracing::info!("roster: {user} turned a change down");
    changed(&s);
    Ok(Json(serde_json::json!({ "ok": true })))
}

/// DELETE /api/roster/proposals/:id — withdrawn (or turned down).
pub async fn withdraw(headers: HeaderMap, Path(id): Path<String>, State(s): State<Arc<AppState>>) -> Result<Json<serde_json::Value>, ApiErr> {
    let (user, key) = me(&headers, &s).await?;
    let _w = WRITE.lock().await;
    let row = sqlx::query("SELECT proposer FROM roster_proposals WHERE op_id = ?").bind(&id)
        .fetch_optional(&s.pool).await.map_err(|_| dberr())?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "That change isn't waiting any more"))?;
    let owner = crate::admin::owner_username(&s.pool).await;
    if row.get::<String, _>("proposer") != key && owner.as_deref() != Some(user.as_str()) {
        return Err(err(StatusCode::FORBIDDEN, "Only whoever proposed it (or the owner) can withdraw it"));
    }
    sqlx::query("DELETE FROM roster_proposals WHERE op_id = ?").bind(&id).execute(&s.pool).await.map_err(|_| dberr())?;
    changed(&s);
    Ok(Json(serde_json::json!({ "ok": true })))
}

/// DELETE /api/admin/roster — start over (when the owner's keys are lost).
/// Every app that had seen the old list warns that it was started over.
pub async fn reset(headers: HeaderMap, State(s): State<Arc<AppState>>) -> Result<Json<serde_json::Value>, ApiErr> {
    check_owner(&headers, &s)?;
    let _w = WRITE.lock().await;
    // Both or neither: never the old list's proposals left waiting on a
    // new list, or half a list.
    let mut tx = begin_write(&s).await?;
    sqlx::query("DELETE FROM roster_entries").execute(&mut *tx).await.map_err(|_| dberr())?;
    sqlx::query("DELETE FROM roster_proposals").execute(&mut *tx).await.map_err(|_| dberr())?;
    tx.commit().await.map_err(|_| dberr())?;
    tracing::warn!("roster: the member list was started over by the owner");
    changed(&s);
    Ok(Json(serde_json::json!({ "ok": true })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    #[test]
    fn signatures_and_heads() {
        let k = SigningKey::from_bytes(&[3u8; 32]);
        let key = hex::encode(k.verifying_key().to_bytes());
        let body = format!(r#"{{"v":1,"id":"{}","at":1,"by":"{}","op":"start"}}"#, "a".repeat(32), key);
        let mut msg = CONTEXT.to_vec();
        msg.extend_from_slice(body.as_bytes());
        let sig = Sig { key: key.clone(), sig: hex::encode(k.sign(&msg).to_bytes()) };
        assert_eq!(check_sigs(&body, &[sig.clone()]).unwrap(), vec![key.clone()]);
        let h = head(&body).unwrap();
        assert_eq!((h.op.as_str(), h.by.as_str(), h.roster.as_str()), ("start", key.as_str(), ""));
        // Changed after signing: refused.
        assert!(check_sigs(&body.replace("start", "stars"), &[sig]).is_err());
        assert!(head("{}").is_err());
    }
}
