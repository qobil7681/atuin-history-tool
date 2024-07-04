use eyre::{Result, WrapErr};
use std::io::BufRead;
use std::path::PathBuf;

use crate::state::AtuinState;
use tauri::{Manager, State};

use atuin_client::{database::Sqlite, record::sqlite_store::SqliteStore, settings::Settings};

#[tauri::command]
pub async fn pty_open<'a>(
    app: tauri::AppHandle,
    state: State<'a, AtuinState>,
) -> Result<uuid::Uuid, String> {
    let id = uuid::Uuid::new_v4();
    let pty = atuin_run::pty::Pty::open(24, 80).await.unwrap();

    let reader = pty.reader.clone();

    tauri::async_runtime::spawn_blocking(move || loop {
        let mut buf = [0u8; 512];

        match reader.lock().unwrap().read(&mut buf) {
            // EOF
            Ok(0) => {
                println!("reader loop hit eof");
                break;
            }

            Ok(n) => {
                println!("read {n} bytes");

                let buf = buf.to_vec();
                let out = String::from_utf8(buf).expect("Invalid utf8");
                let out = out.trim_matches(char::from(0));
                app.emit(format!("pty-{id}").as_str(), out).unwrap();
            }

            Err(e) => {
                println!("failed to read: {e}");
                break;
            }
        }
    });

    state.pty_sessions.write().await.insert(id, pty);

    Ok(id)
}

#[tauri::command]
pub(crate) async fn pty_write(
    pid: uuid::Uuid,
    data: String,
    state: tauri::State<'_, AtuinState>,
) -> Result<(), String> {
    let sessions = state.pty_sessions.read().await;
    let pty = sessions.get(&pid).ok_or("Pty not found")?.clone();

    let bytes = data.as_bytes().to_vec();
    pty.send_bytes(bytes.into())
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
pub(crate) async fn pty_read(
    pid: uuid::Uuid,
    state: tauri::State<'_, AtuinState>,
) -> Result<Vec<u8>, String> {
    let sessions = state.pty_sessions.read().await;
    let pty = sessions.get(&pid).ok_or("Pty not found")?.clone();

    let mut buf = [0u8; 512];

    let n = pty
        .reader
        .lock()
        .map_err(|e| e.to_string())?
        .read(&mut buf)
        .map_err(|e| e.to_string())?;

    Ok(buf.to_vec())
}
