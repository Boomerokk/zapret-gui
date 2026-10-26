#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tauri::{
    AppHandle, Emitter, Manager,
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;

// ── Проверка прав ─────────────────────────────────────────────────────────────

fn is_elevated() -> bool {
    #[cfg(target_os = "windows")]
    {
        // net session возвращает 0 только от имени администратора
        std::process::Command::new("net")
            .args(["session"])
            .creation_flags(0x08000000)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }
    #[cfg(not(target_os = "windows"))]
    true
}

// ── Состояние ─────────────────────────────────────────────────────────────────

#[derive(Default)]
struct AppState {
    process:          Mutex<Option<tokio::process::Child>>,
    current_strategy: Mutex<Option<String>>,
    zapret_dir:       Mutex<Option<PathBuf>>,
}

// ── Типы ──────────────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Clone, Debug)]
struct Strategy {
    name:     String,
    filename: String,
    group:    String,
}

#[derive(Serialize, Clone, Debug)]
struct ServiceStatus {
    name:    String,
    running: bool,
    exists:  bool,
}

#[derive(Serialize, Clone, Debug)]
struct LogLine {
    text:  String,
    level: String,
}

// ── Вспомогательное ───────────────────────────────────────────────────────────

fn get_zapret_dir(state: &tauri::State<AppState>) -> Result<PathBuf, String> {
    state.zapret_dir.lock().unwrap().clone()
        .ok_or_else(|| "Папка zapret не выбрана".to_string())
}

fn app_data(app: &AppHandle) -> PathBuf {
    app.path().app_data_dir().unwrap_or_else(|_| PathBuf::from("."))
}

fn make_strategy(filename: &str) -> Strategy {
    let name = filename.replace(".bat", "");
    let group = if filename.contains("FAKE TLS AUTO") { "Fake TLS Auto" }
        else if filename.contains("SIMPLE FAKE") { "Simple Fake" }
        else if filename.contains("ALT") { "ALT" }
        else { "Основные" };
    Strategy { name, filename: filename.to_string(), group: group.to_string() }
}

fn emit_log(app: &AppHandle, text: &str, level: &str) {
    let _ = app.emit("log", LogLine { text: text.to_string(), level: level.to_string() });
}

fn make_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36")
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| e.to_string())
}

/// Скачать URL — сначала через reqwest, при ошибке фолбэк через PowerShell
async fn fetch_text(url: &str) -> String {
    if let Ok(client) = make_client() {
        if let Ok(resp) = client.get(url).send().await {
            if let Ok(text) = resp.text().await {
                if !text.trim().is_empty() {
                    return text;
                }
            }
        }
    }
    // Фолбэк через PowerShell (обходит некоторые блокировки)
    #[cfg(target_os = "windows")]
    {
        let tmp = std::env::temp_dir().join(format!("zapret_dl_{}.txt",
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs()));
        let ps = format!(
            "try{{ (Invoke-WebRequest -Uri '{}' -UseBasicParsing -TimeoutSec 30).Content | \
             Out-File -FilePath '{}' -Encoding UTF8 }}catch{{}}",
            url, tmp.display()
        );
        let _ = std::process::Command::new("powershell")
            .args(["-NoProfile", "-Command", &ps])
            .creation_flags(0x08000000)
            .output();
        std::fs::read_to_string(&tmp).unwrap_or_default()
    }
    #[cfg(not(target_os = "windows"))]
    String::new()
}

// ── Команды ───────────────────────────────────────────────────────────────────

#[tauri::command]
fn check_admin() -> bool { is_elevated() }

#[tauri::command]
fn request_admin_restart() {
    #[cfg(target_os = "windows")]
    {
        let exe = std::env::current_exe().unwrap_or_default();
        let _ = std::process::Command::new("powershell")
            .args(["-Command", &format!("Start-Process '{}' -Verb RunAs", exe.display())])
            .creation_flags(0x08000000)
            .spawn();
        std::process::exit(0);
    }
}

#[tauri::command]
fn set_autostart_registry(enable: bool) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let exe_path = exe.to_string_lossy().to_string();
        let task_name = "ZapretGUI";

        if enable {
            // Используем Task Scheduler — работает с elevated процессами
            // Запускаем при входе пользователя, с наивысшим приоритетом, без задержки
            // /RL HIGHEST — запускать с правами администратора
            // /SC ONLOGON — при входе в систему
            // /DELAY 0000:05 — задержка 5 секунд (раньше большинства приложений)
            let xml = format!(r#"<?xml version="1.0"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>Zapret GUI - обход блокировок Discord и YouTube</Description>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
      <Delay>PT5S</Delay>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>HighestAvailable</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>4</Priority>
  </Settings>
  <Actions>
    <Exec>
      <Command>{exe_path}</Command>
    </Exec>
  </Actions>
</Task>"#, exe_path = exe_path);

            // Сохраняем XML во временный файл UTF-16 LE с BOM
            let tmp = std::env::temp_dir().join("zapret_gui_task.xml");
            let mut utf16: Vec<u8> = vec![0xFF, 0xFE]; // BOM для UTF-16 LE
            for c in xml.encode_utf16() {
                utf16.push(c as u8);
                utf16.push((c >> 8) as u8);
            }
            std::fs::write(&tmp, &utf16).map_err(|e| e.to_string())?;

            let out = std::process::Command::new("schtasks")
                .args(["/Create", "/TN", task_name, "/XML", tmp.to_str().unwrap_or(""), "/F"])
                .creation_flags(0x08000000)
                .output()
                .map_err(|e| e.to_string())?;

            std::fs::remove_file(&tmp).ok();

            if !out.status.success() {
                let err = String::from_utf8_lossy(&out.stderr).to_string();
                let stdout = String::from_utf8_lossy(&out.stdout).to_string();
                return Err(format!("Ошибка создания задачи: {} {}", err, stdout));
            }
        } else {
            // Удаляем задачу из планировщика
            let _ = std::process::Command::new("schtasks")
                .args(["/Delete", "/TN", task_name, "/F"])
                .creation_flags(0x08000000)
                .output();
        }

        // Также чистим старую запись из реестра Run если осталась
        let key = r"HKCU\SOFTWARE\Microsoft\Windows\CurrentVersion\Run";
        let _ = std::process::Command::new("reg")
            .args(["delete", key, "/v", "ZapretGUI", "/f"])
            .creation_flags(0x08000000).output();
    }
    Ok(())
}

#[tauri::command]
fn get_autostart_registry() -> bool {
    #[cfg(target_os = "windows")]
    {
        // Проверяем задачу в планировщике
        let task = std::process::Command::new("schtasks")
            .args(["/Query", "/TN", "ZapretGUI"])
            .creation_flags(0x08000000).output()
            .map(|o| o.status.success()).unwrap_or(false);
        if task { return true; }
        // Фолбэк — старый реестр
        return std::process::Command::new("reg")
            .args(["query", r"HKCU\SOFTWARE\Microsoft\Windows\CurrentVersion\Run", "/v", "ZapretGUI"])
            .creation_flags(0x08000000).output()
            .map(|o| o.status.success()).unwrap_or(false);
    }
    #[cfg(not(target_os = "windows"))]
    false
}

#[tauri::command]
async fn choose_zapret_dir(app: AppHandle, state: tauri::State<'_, AppState>) -> Result<String, String> {
    use tauri_plugin_dialog::DialogExt;
    let folder = app.dialog().file()
        .set_title("Выберите папку zapret-discord-youtube")
        .blocking_pick_folder();
    match folder {
        Some(p) => {
            let pb = p.into_path().map_err(|e| e.to_string())?;
            *state.zapret_dir.lock().unwrap() = Some(pb.clone());
            Ok(pb.to_string_lossy().to_string())
        }
        None => Err("Папка не выбрана".to_string()),
    }
}

#[tauri::command]
fn set_zapret_dir(path: String, state: tauri::State<'_, AppState>) -> Result<(), String> {
    let p = PathBuf::from(&path);
    if !p.exists() { return Err(format!("Путь не существует: {path}")); }
    *state.zapret_dir.lock().unwrap() = Some(p);
    Ok(())
}

#[tauri::command]
fn list_strategies(state: tauri::State<'_, AppState>) -> Result<Vec<Strategy>, String> {
    let dir = get_zapret_dir(&state)?;
    let preferred = [
        "general.bat",
        "general (ALT).bat",  "general (ALT2).bat",  "general (ALT3).bat",
        "general (ALT4).bat",  "general (ALT5).bat",  "general (ALT6).bat",
        "general (ALT7).bat",  "general (ALT8).bat",  "general (ALT9).bat",
        "general (ALT10).bat", "general (ALT11).bat",
        "general (SIMPLE FAKE).bat", "general (SIMPLE FAKE ALT).bat",
        "general (SIMPLE FAKE ALT2).bat", "general (FAKE TLS AUTO).bat",
        "general (FAKE TLS AUTO ALT).bat", "general (FAKE TLS AUTO ALT2).bat",
        "general (FAKE TLS AUTO ALT3).bat",
    ];
    let mut list: Vec<Strategy> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for f in &preferred {
        if dir.join(f).exists() { seen.insert(f.to_string()); list.push(make_strategy(f)); }
    }
    if let Ok(entries) = std::fs::read_dir(&dir) {
        let mut extra: Vec<String> = entries.filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.starts_with("general") && n.ends_with(".bat") && !seen.contains(n))
            .collect();
        extra.sort();
        for f in extra { list.push(make_strategy(&f)); }
    }
    if list.is_empty() { return Err("general*.bat файлы не найдены".to_string()); }
    Ok(list)
}

#[tauri::command]
async fn start_strategy(app: AppHandle, state: tauri::State<'_, AppState>, filename: String) -> Result<(), String> {
    let child_opt = { state.process.lock().unwrap().take() };
    if let Some(mut child) = child_opt {
        let pid = child.id().unwrap_or(0);
        if pid > 0 {
            #[cfg(target_os = "windows")]
            let _ = std::process::Command::new("taskkill")
                .args(["/F", "/T", "/PID", &pid.to_string()])
                .creation_flags(0x08000000).output();
        }
        let _ = child.kill().await;
    }
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("taskkill")
        .args(["/F", "/IM", "winws.exe"]).creation_flags(0x08000000).output();
    *state.current_strategy.lock().unwrap() = None;

    let dir = get_zapret_dir(&state)?;
    let bat = dir.join(&filename);
    if !bat.exists() { return Err(format!("Файл не найден: {}", bat.display())); }

    emit_log(&app, &format!("▶ Запуск: {filename}"), "info");

    let mut cmd = Command::new("cmd");
    cmd.arg("/c").arg(&bat).current_dir(&dir).stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(target_os = "windows")]
    cmd.creation_flags(0x08000000 | 0x00000200);

    let mut child = cmd.spawn().map_err(|e| format!("Ошибка запуска: {e}"))?;
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();

    let app1 = app.clone();
    tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let lv = if line.to_lowercase().contains("error") {"error"} else if line.to_lowercase().contains("warn") {"warn"} else {"success"};
            emit_log(&app1, &line, lv);
        }
    });
    let app2 = app.clone();
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if !line.trim().is_empty() { emit_log(&app2, &line, "warn"); }
        }
    });

    // Скрываем окно winws через 1.5с
    #[cfg(target_os = "windows")]
    {
        let app3 = app.clone();
        tokio::spawn(async move {
            tokio::time::sleep(tokio::time::Duration::from_millis(1500)).await;
            let _ = std::process::Command::new("powershell")
                .args(["-WindowStyle","Hidden","-Command",
                    "Add-Type -Name W -Namespace N -MemberDefinition \
                    '[DllImport(\"user32.dll\")] public static extern bool ShowWindow(IntPtr h,int n);'; \
                    Get-Process winws -EA SilentlyContinue | %{ [N.W]::ShowWindow($_.MainWindowHandle,0) }"
                ])
                .creation_flags(0x08000000).output();
            let _ = app3.emit("status", serde_json::json!({ "running": true }));
        });
    }

    *state.current_strategy.lock().unwrap() = Some(filename);
    *state.process.lock().unwrap() = Some(child);
    let _ = app.emit("status", serde_json::json!({ "running": true }));
    Ok(())
}

#[tauri::command]
async fn stop_strategy(state: tauri::State<'_, AppState>) -> Result<(), String> {
    let child_opt = { state.process.lock().unwrap().take() };
    if let Some(mut child) = child_opt {
        let pid = child.id().unwrap_or(0);
        if pid > 0 {
            #[cfg(target_os = "windows")]
            let _ = std::process::Command::new("taskkill")
                .args(["/F", "/T", "/PID", &pid.to_string()])
                .creation_flags(0x08000000).output();
        }
        let _ = child.kill().await;
    }
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("taskkill")
        .args(["/F", "/IM", "winws.exe"]).creation_flags(0x08000000).output();
    *state.current_strategy.lock().unwrap() = None;
    Ok(())
}

#[tauri::command]
fn get_process_status(state: tauri::State<'_, AppState>) -> Result<bool, String> {
    #[cfg(target_os = "windows")]
    if let Ok(out) = std::process::Command::new("tasklist")
        .args(["/FI", "IMAGENAME eq winws.exe", "/NH"])
        .creation_flags(0x08000000).output()
    {
        return Ok(String::from_utf8_lossy(&out.stdout).contains("winws.exe"));
    }
    let mut guard = state.process.lock().unwrap();
    if let Some(child) = guard.as_mut() {
        match child.try_wait() {
            Ok(Some(_)) => { *guard = None; Ok(false) }
            Ok(None) => Ok(true),
            Err(_) => Ok(false),
        }
    } else { Ok(false) }
}

#[tauri::command]
async fn download_zapret(app: AppHandle) -> Result<String, String> {
    use futures_util::StreamExt;
    let zapret_dir = app_data(&app).join("zapret");
    let emit = |stage: &str, msg: String, pct: u8| {
        let _ = app.emit("download_progress", serde_json::json!({"stage":stage,"message":msg,"progress":pct}));
    };
    emit("fetch", "Получение информации о релизе…".into(), 5);
    let client = make_client()?;
    let json: serde_json::Value = client
        .get("https://api.github.com/repos/Flowseal/zapret-discord-youtube/releases/latest")
        .send().await.map_err(|e| format!("Ошибка сети: {e}"))?
        .json().await.map_err(|e| format!("Ошибка парсинга: {e}"))?;
    let tag = json["tag_name"].as_str().unwrap_or("unknown").to_string();
    emit("fetch", format!("Найден релиз: {tag}"), 15);
    let zip_url = json["assets"].as_array()
        .and_then(|a| a.iter().find(|x| x["name"].as_str().map(|n| n.ends_with(".zip")).unwrap_or(false)))
        .and_then(|a| a["browser_download_url"].as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("https://github.com/Flowseal/zapret-discord-youtube/archive/refs/tags/{tag}.zip"));
    emit("download", "Скачивание архива…".into(), 20);
    let resp = client.get(&zip_url).send().await.map_err(|e| format!("Ошибка скачивания: {e}"))?;
    let total = resp.content_length().unwrap_or(0);
    let mut downloaded: u64 = 0;
    let mut bytes: Vec<u8> = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| e.to_string())?;
        downloaded += chunk.len() as u64;
        bytes.extend_from_slice(&chunk);
        if total > 0 { emit("download", format!("{}/{} KB", downloaded/1024, total/1024), (20 + downloaded*60/total) as u8); }
    }
    emit("extract", "Распаковка…".into(), 82);
    if zapret_dir.exists() {
        // Сначала убиваем winws.exe чтобы освободить файлы
        #[cfg(target_os = "windows")]
        {
            let _ = std::process::Command::new("taskkill")
                .args(["/F", "/IM", "winws.exe"]).creation_flags(0x08000000).output();
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
        std::fs::remove_dir_all(&zapret_dir).map_err(|e| format!("Ошибка удаления старой папки: {e}"))?;
    }
    std::fs::create_dir_all(&zapret_dir).map_err(|e| e.to_string())?;
    let cursor = std::io::Cursor::new(bytes);
    let mut archive = zip::ZipArchive::new(cursor).map_err(|e| format!("Ошибка архива: {e}"))?;

    // Определяем общую корневую папку архива (например "zapret-discord-youtube-1.10.4/")
    // и срезаем её, чтобы файлы легли прямо в zapret_dir.
    let root_prefix = {
        let mut root: Option<String> = None;
        for i in 0..archive.len() {
            let name = archive.by_index(i).map_err(|e| e.to_string())?.name().to_string();
            let path = std::path::Path::new(&name);
            if path.components().count() == 0 { continue; }
            let top = path.components().next().unwrap().as_os_str().to_string_lossy().to_string();
            if top.is_empty() { continue; }
            root = match root {
                None => Some(top),
                Some(r) if r == top => Some(top),
                // Разные корневые папки у записей — общего префикса нет
                Some(_) => { root = Some(String::new()); break; }
            };
        }
        root.unwrap_or_default()
    };

    for i in 0..archive.len() {
        let mut file = archive.by_index(i).map_err(|e| e.to_string())?;
        let raw = file.name().to_string();
        if raw.ends_with('/') { continue; }
        // Срезаем корневую папку, если она есть
        let stripped = if root_prefix.is_empty() {
            raw.clone()
        } else {
            raw.strip_prefix(&format!("{root_prefix}/")).unwrap_or(&raw).to_string()
        };
        if stripped.is_empty() { continue; }
        let out = zapret_dir.join(PathBuf::from(stripped.replace('/', std::path::MAIN_SEPARATOR_STR)));
        if let Some(p) = out.parent() { std::fs::create_dir_all(p).ok(); }
        let mut f = std::fs::File::create(&out).map_err(|e| e.to_string())?;
        std::io::copy(&mut file, &mut f).map_err(|e| e.to_string())?;
    }
    std::fs::write(zapret_dir.join(".version"), &tag).ok();
    if !zapret_dir.join("general.bat").exists() {
        return Err(format!("Архив распакован, но general.bat не найден в {root_prefix}. Структура архива изменилась?"));
    }
    emit("done", format!("Готово! Версия {tag}"), 100);
    Ok(zapret_dir.to_string_lossy().to_string())
}

#[tauri::command]
async fn check_zapret_updates(app: AppHandle) -> Result<serde_json::Value, String> {
    let client = make_client()?;
    let json: serde_json::Value = client
        .get("https://api.github.com/repos/Flowseal/zapret-discord-youtube/releases/latest")
        .send().await.map_err(|e| format!("Ошибка сети: {e}"))?
        .json().await.map_err(|e| format!("Ошибка парсинга: {e}"))?;
    let latest = json["tag_name"].as_str().unwrap_or("").to_string();
    let current = std::fs::read_to_string(app_data(&app).join("zapret").join(".version"))
        .unwrap_or_default().trim().to_string();
    Ok(serde_json::json!({
        "latest_tag": latest, "latest_name": json["name"].as_str().unwrap_or(""),
        "body": json["body"].as_str().unwrap_or(""),
        "published_at": json["published_at"].as_str().unwrap_or(""),
        "html_url": json["html_url"].as_str().unwrap_or(""),
        "current_version": current,
        "has_update": !current.is_empty() && current != latest,
    }))
}

#[tauri::command]
fn check_services() -> Vec<ServiceStatus> {
    let mut result = Vec::new();

    // Проверяем запущен ли winws.exe как процесс (основной режим работы)
    #[cfg(target_os = "windows")]
    {
        let winws_running = std::process::Command::new("tasklist")
            .args(["/FI", "IMAGENAME eq winws.exe", "/NH"])
            .creation_flags(0x08000000)
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).contains("winws.exe"))
            .unwrap_or(false);
        result.push(ServiceStatus {
            name: "winws.exe (процесс)".to_string(),
            running: winws_running,
            exists: true,
        });
    }

    // Проверяем Windows службы (install service режим)
    #[cfg(target_os = "windows")]
    for svc in &["zapret", "WinDivert", "WinDivert14"] {
        if let Ok(out) = std::process::Command::new("sc").args(["query",svc]).creation_flags(0x08000000).output() {
            let s = String::from_utf8_lossy(&out.stdout);
            if out.status.success() || s.contains("STATE") {
                result.push(ServiceStatus { name: svc.to_string(), running: s.contains("RUNNING"), exists: true });
            }
        }
    }
    result
}

#[tauri::command]
fn open_url(url: String) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    std::process::Command::new("cmd").args(["/c","start","",&url]).creation_flags(0x08000000).spawn().map_err(|e: std::io::Error| e.to_string())?;
    #[cfg(target_os = "macos")]
    std::process::Command::new("open").arg(&url).spawn().map_err(|e: std::io::Error| e.to_string())?;
    Ok(())
}

#[tauri::command]
fn get_default_zapret_dir(app: AppHandle) -> String {
    app_data(&app).join("zapret").to_string_lossy().to_string()
}

#[tauri::command]
fn is_zapret_installed(app: AppHandle) -> bool {
    app_data(&app).join("zapret").join("general.bat").exists()
}

#[tauri::command]
fn get_service_status(state: tauri::State<'_, AppState>) -> serde_json::Value {
    let dir = match get_zapret_dir(&state) {
        Ok(d) => d,
        Err(_) => return serde_json::json!({"game_filter":"неизвестно","ipset_filter":"неизвестно","auto_update":"неизвестно"}),
    };
    let game_filter = {
        let f = dir.join("utils").join("game_filter.enabled");
        if !f.exists() { "выключен".to_string() } else {
            match std::fs::read_to_string(&f).unwrap_or_default().trim().to_lowercase().as_str() {
                "all" => "TCP и UDP", "tcp" => "только TCP", "udp" => "только UDP", _ => "включён",
            }.to_string()
        }
    };
    let ipset_filter = {
        let f = dir.join("lists").join("ipset-all.txt");
        if let Ok(c) = std::fs::read_to_string(&f) {
            let n = c.lines().filter(|l| !l.trim().is_empty()).count();
            if n == 0 { "any".to_string() }
            else if c.contains("203.0.113.113") { "none".to_string() }
            else { format!("loaded ({n} IP)") }
        } else { "неизвестно".to_string() }
    };
    let auto_update = if dir.join("utils").join("check_updates.enabled").exists() { "включена" } else { "выключена" };
    serde_json::json!({"game_filter":game_filter,"ipset_filter":ipset_filter,"auto_update":auto_update})
}

#[tauri::command]
async fn run_service_action(app: AppHandle, state: tauri::State<'_, AppState>, action: String) -> Result<String, String> {
    let dir = get_zapret_dir(&state)?;
    let utils = dir.join("utils");
    let lists = dir.join("lists");

    let result: String = match action.as_str() {

        "install" => {
            let strategy = state.current_strategy.lock().unwrap().clone();
            let filename = strategy.unwrap_or_else(|| "general.bat".to_string());
            let bat = dir.join(&filename);
            let bin_dir = dir.join("bin");
            let winws = bin_dir.join("winws.exe");
            if !winws.exists() { return Err("winws.exe не найден в bin/".to_string()); }

            emit_log(&app, &format!("⚙ Установка службы: {}…", filename), "info");

            // Читаем bat и извлекаем аргументы winws
            let bat_content = std::fs::read_to_string(&bat).unwrap_or_default();
            let bin_s   = bin_dir.to_string_lossy().to_string() + "\\";
            let lists_s = dir.join("lists").to_string_lossy().to_string() + "\\";
            let mut args_parts: Vec<String> = Vec::new();
            let mut in_cmd = false;
            for line in bat_content.lines() {
                let t = line.trim().trim_end_matches('^').trim().to_string();
                if t.to_lowercase().contains("winws.exe") && !t.starts_with("::") {
                    in_cmd = true;
                    if let Some(i) = t.to_lowercase().find("winws.exe") {
                        let rest = t[i+"winws.exe".len()..].trim_start_matches('"').trim().to_string();
                        if !rest.is_empty() { args_parts.push(rest); }
                    }
                } else if in_cmd {
                    if t.starts_with("--") { args_parts.push(t); }
                    else if !t.is_empty() && !t.starts_with("::") { break; }
                }
            }
            let args_str = args_parts.join(" ").replace("%BIN%\\", &bin_s).replace("%BIN%", &bin_s)
                .replace("%LISTS%\\", &lists_s).replace("%LISTS%", &lists_s);

            #[cfg(target_os = "windows")]
            {
                let _ = std::process::Command::new("sc").args(["stop","zapret"]).creation_flags(0x08000000).output();
                let _ = std::process::Command::new("sc").args(["delete","zapret"]).creation_flags(0x08000000).output();
                std::thread::sleep(std::time::Duration::from_millis(600));
                let bin_path = format!("\"{}\" {}", winws.to_string_lossy(), args_str.trim());
                let out = std::process::Command::new("sc")
                    .args(["create","zapret",&format!("binPath= {}",bin_path),"DisplayName=","zapret","start=","auto"])
                    .creation_flags(0x08000000).output();
                match out {
                    Ok(o) if o.status.success() => {
                        let _ = std::process::Command::new("sc").args(["description","zapret","Zapret DPI bypass"]).creation_flags(0x08000000).output();
                        let _ = std::process::Command::new("sc").args(["start","zapret"]).creation_flags(0x08000000).output();
                    }
                    _ => return Err("Ошибка создания службы. Запустите приложение от администратора.".to_string()),
                }
            }
            format!("Служба zapret установлена: {}", filename.replace(".bat",""))
        }

        "remove" => {
            emit_log(&app, "⚙ Удаление служб…", "info");
            #[cfg(target_os = "windows")]
            {
                let _ = std::process::Command::new("taskkill").args(["/F","/IM","winws.exe"]).creation_flags(0x08000000).output();
                for s in &["zapret","WinDivert","WinDivert14"] {
                    let _ = std::process::Command::new("net").args(["stop",s]).creation_flags(0x08000000).output();
                    let _ = std::process::Command::new("sc").args(["delete",s]).creation_flags(0x08000000).output();
                }
            }
            "Службы удалены".to_string()
        }

        "game_filter_off" => { std::fs::remove_file(utils.join("game_filter.enabled")).ok(); "Game Filter выключен. Перезапустите стратегию.".to_string() }
        "game_filter_all" => { std::fs::create_dir_all(&utils).ok(); std::fs::write(utils.join("game_filter.enabled"),"all").ok(); "Game Filter: TCP и UDP. Перезапустите стратегию.".to_string() }
        "game_filter_tcp" => { std::fs::create_dir_all(&utils).ok(); std::fs::write(utils.join("game_filter.enabled"),"tcp").ok(); "Game Filter: только TCP. Перезапустите стратегию.".to_string() }
        "game_filter_udp" => { std::fs::create_dir_all(&utils).ok(); std::fs::write(utils.join("game_filter.enabled"),"udp").ok(); "Game Filter: только UDP. Перезапустите стратегию.".to_string() }

        "ipset_none" => {
            let f = lists.join("ipset-all.txt"); let b = lists.join("ipset-all.txt.backup");
            if f.exists() && !b.exists() { std::fs::rename(&f,&b).ok(); }
            std::fs::write(&f,"203.0.113.113/32\n").ok();
            "IPSet Filter: none".to_string()
        }
        "ipset_any"    => { std::fs::write(lists.join("ipset-all.txt"),"").ok(); "IPSet Filter: any".to_string() }
        "ipset_loaded" => {
            let f = lists.join("ipset-all.txt"); let b = lists.join("ipset-all.txt.backup");
            if b.exists() { std::fs::rename(&b,&f).ok(); "IPSet Filter: восстановлен".to_string() }
            else { return Err("Резервная копия не найдена".to_string()); }
        }

        "toggle_autoupdate" => {
            let flag = utils.join("check_updates.enabled");
            if flag.exists() { std::fs::remove_file(&flag).ok(); "Автопроверка выключена".to_string() }
            else { std::fs::create_dir_all(&utils).ok(); std::fs::write(&flag,"ENABLED").ok(); "Автопроверка включена".to_string() }
        }

        "update_ipset" => {
            emit_log(&app, "⬇ Обновление ipset-all.txt…", "info");
            let url = "https://raw.githubusercontent.com/Flowseal/zapret-discord-youtube/refs/heads/main/.service/ipset-service.txt";
            let text = fetch_text(url).await;
            if text.trim().is_empty() { return Err("Не удалось скачать ipset. Проверьте интернет.".to_string()); }
            std::fs::create_dir_all(&lists).ok();
            std::fs::write(lists.join("ipset-all.txt"), &text).map_err(|e| e.to_string())?;
            format!("ipset-all.txt обновлён ({} записей)", text.lines().filter(|l| !l.trim().is_empty()).count())
        }

        "update_hosts" => {
            emit_log(&app, "⬇ Загрузка hosts…", "info");
            let url = "https://raw.githubusercontent.com/Flowseal/zapret-discord-youtube/refs/heads/main/.service/hosts";
            let text = fetch_text(url).await;
            if text.trim().is_empty() { return Err("Не удалось скачать hosts. Проверьте интернет.".to_string()); }
            let hosts_path = PathBuf::from(r"C:\Windows\System32\drivers\etc\hosts");
            let current = std::fs::read_to_string(&hosts_path).unwrap_or_default();
            let first = text.lines().next().unwrap_or("");
            if !first.is_empty() && current.contains(first) {
                "Hosts файл актуален".to_string()
            } else {
                let tmp = std::env::temp_dir().join("zapret_hosts_new.txt");
                std::fs::write(&tmp, &text).ok();
                emit_log(&app, &format!("Новый hosts: {}", tmp.display()), "info");
                #[cfg(target_os = "windows")]
                {
                    let _ = std::process::Command::new("notepad").arg(&tmp).creation_flags(0x08000000).spawn();
                    let _ = std::process::Command::new("powershell")
                        .args(["-Command",&format!("Start-Process notepad -ArgumentList '{}' -Verb RunAs", hosts_path.display())])
                        .creation_flags(0x08000000).spawn();
                }
                "Hosts устарел. Скопируйте содержимое нового файла в текущий hosts".to_string()
            }
        }

        "diagnostics" => {
            emit_log(&app, "🩺 Диагностика…", "info");
            let mut issues = vec![];
            #[cfg(target_os = "windows")]
            {
                if let Ok(o) = std::process::Command::new("sc").args(["query","BFE"]).creation_flags(0x08000000).output() {
                    let ok = String::from_utf8_lossy(&o.stdout).contains("RUNNING");
                    emit_log(&app, if ok {"✔ BFE: запущен"} else {"✗ BFE НЕ запущен!"}, if ok {"success"} else {"error"});
                    if !ok { issues.push("BFE"); }
                }
                if let Ok(o) = std::process::Command::new("tasklist").args(["/FI","IMAGENAME eq winws.exe","/NH"]).creation_flags(0x08000000).output() {
                    let ok = String::from_utf8_lossy(&o.stdout).contains("winws.exe");
                    emit_log(&app, if ok {"✔ winws.exe: работает"} else {"○ winws.exe: не запущен"}, if ok {"success"} else {"warn"});
                }
                let sys = dir.join("bin").join("WinDivert64.sys").exists();
                emit_log(&app, if sys {"✔ WinDivert64.sys: найден"} else {"✗ WinDivert64.sys: НЕ найден"}, if sys {"success"} else {"error"});
                if !sys { issues.push("WinDivert64.sys"); }
                if let Ok(o) = std::process::Command::new("sc").args(["query"]).creation_flags(0x08000000).output() {
                    let s = String::from_utf8_lossy(&o.stdout);
                    for (kw,msg) in &[("killer","Killer"),("tracsr","Check Point"),("smartbyte","SmartByte"),("goodbyedpi","GoodbyeDPI")] {
                        if s.to_lowercase().contains(kw) { emit_log(&app,&format!("⚠ {msg} — конфликт с zapret"),"warn"); }
                    }
                }
                if let Ok(o) = std::process::Command::new("tasklist").args(["/FI","IMAGENAME eq AdguardSvc.exe","/NH"]).creation_flags(0x08000000).output() {
                    let adg = String::from_utf8_lossy(&o.stdout).contains("AdguardSvc.exe");
                    emit_log(&app, if adg {"⚠ Adguard — может мешать Discord"} else {"✔ Adguard: не найден"}, if adg {"warn"} else {"success"});
                }
                if let Ok(o) = std::process::Command::new("reg").args(["query",r"HKCU\Software\Microsoft\Windows\CurrentVersion\Internet Settings","/v","ProxyEnable"]).creation_flags(0x08000000).output() {
                    let proxy = String::from_utf8_lossy(&o.stdout).contains("0x1");
                    emit_log(&app, if proxy {"⚠ Прокси включён"} else {"✔ Прокси: выключен"}, if proxy {"warn"} else {"success"});
                }
            }
            if issues.is_empty() { "Диагностика завершена — проблем не обнаружено".to_string() }
            else { format!("Найдены проблемы: {}", issues.join(", ")) }
        }

        "run_tests" => {
            let ps = dir.join("utils").join("test zapret.ps1");
            if !ps.exists() { return Err("utils/test zapret.ps1 не найден".to_string()); }
            #[cfg(target_os = "windows")]
            std::process::Command::new("powershell")
                .args(["-NoProfile","-ExecutionPolicy","Bypass","-File",ps.to_str().unwrap_or("")])
                .current_dir(&dir).creation_flags(0x00000010).spawn().map_err(|e| e.to_string())?;
            "Тесты запущены".to_string()
        }

        _ => return Err(format!("Неизвестное действие: {action}")),
    };

    emit_log(&app, &format!("✔ {result}"), "success");
    Ok(result)
}

// ── Точка входа ───────────────────────────────────────────────────────────────

#[tauri::command]
fn open_zapret_dir(app: AppHandle) -> Result<(), String> {
    let dir = app_data(&app).join("zapret");
    if !dir.exists() {
        return Err("Папка zapret не найдена. Сначала установите zapret.".to_string());
    }
    #[cfg(target_os = "windows")]
    std::process::Command::new("explorer")
        .arg(dir.to_string_lossy().as_ref())
        .creation_flags(0x08000000)
        .spawn()
        .map_err(|e| e.to_string())?;
    #[cfg(target_os = "macos")]
    std::process::Command::new("open")
        .arg(dir)
        .spawn()
        .map_err(|e| e.to_string())?;
    Ok(())
}


#[tauri::command]
async fn uninstall_zapret(app: AppHandle) -> Result<(), String> {
    let zapret_dir = app_data(&app).join("zapret");
    if !zapret_dir.exists() {
        return Ok(()); // уже удалён
    }
    // Убиваем winws.exe перед удалением
    #[cfg(target_os = "windows")]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/F", "/IM", "winws.exe"])
            .creation_flags(0x08000000).output();
        // Также WinDivert
        for svc in &["zapret", "WinDivert", "WinDivert14"] {
            let _ = std::process::Command::new("net").args(["stop", svc]).creation_flags(0x08000000).output();
            let _ = std::process::Command::new("sc").args(["delete", svc]).creation_flags(0x08000000).output();
        }
        std::thread::sleep(std::time::Duration::from_millis(800));
    }
    std::fs::remove_dir_all(&zapret_dir)
        .map_err(|e| format!("Ошибка удаления файлов: {e}"))?;
    Ok(())
}


fn main() {
    // Права запрашиваются через манифест (requireAdministrator) — см. build.rs
    // Это проверка на случай если манифест не сработал

    tauri::Builder::default()
        .plugin(tauri_plugin_store::Builder::new().build())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_shell::init())
        .manage(AppState::default())
        .invoke_handler(tauri::generate_handler![
            check_admin, request_admin_restart,
            set_autostart_registry, get_autostart_registry,
            choose_zapret_dir, set_zapret_dir, list_strategies,
            start_strategy, stop_strategy, get_process_status,
            download_zapret, check_zapret_updates, check_services,
            open_url, get_default_zapret_dir,
            is_zapret_installed, get_service_status, run_service_action,
            uninstall_zapret, open_zapret_dir,
        ])
        .setup(|app| {
            let show = MenuItem::with_id(app, "show", "Показать", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "Выход",    true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show, &quit])?;
            TrayIconBuilder::with_id("main-tray")
                .icon(app.default_window_icon().unwrap().clone())
                .menu(&menu).tooltip("Zapret GUI")
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "show" => { if let Some(w) = app.get_webview_window("main") { let _ = w.show(); let _ = w.set_focus(); } }
                    "quit" => app.exit(0),
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = event {
                        if let Some(w) = tray.app_handle().get_webview_window("main") { let _ = w.show(); let _ = w.set_focus(); }
                    }
                })
                .build(app)?;
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let dir = handle.path().app_data_dir().unwrap_or_default().join("zapret");
                if dir.join("general.bat").exists() {
                    let state = handle.state::<AppState>();
                    *state.zapret_dir.lock().unwrap() = Some(dir.clone());
                    let _ = handle.emit("zapret_dir_ready", dir.to_string_lossy().to_string());
                }
            });
            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .run(tauri::generate_context!())
        .expect("ошибка запуска");
}
