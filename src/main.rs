mod browser;
mod store;

use browser::{active_profile_ids, create_session, SharedSession};
use headless_chrome::browser::default_executable;
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    env, fs,
    io::{Read, Write},
    net::{Shutdown, TcpListener, TcpStream, ToSocketAddrs},
    path::PathBuf,
    sync::{Arc, Mutex},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use store::{load_or_create_token, Store};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const CONNECTOR_NAME: &str = "com.baijimu.connector.browser";
const DEFAULT_HOST: &str = "127.0.0.1";
const DEFAULT_PORT: u16 = 18121;
const MAX_HEADER_BYTES: usize = 64 * 1024;
const MAX_REQUEST_BYTES: usize = 1024 * 1024;

struct AppState {
    store: Store,
    management_token: String,
    sessions: Mutex<HashMap<String, SharedSession>>,
}

#[derive(Debug)]
struct HttpError {
    status: u16,
    code: &'static str,
    message: String,
    data: Value,
}

impl HttpError {
    fn new(status: u16, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            data: Value::Null,
        }
    }

    fn bad_request(message: impl Into<String>) -> Self {
        Self::new(400, "INVALID_ARGUMENT", message)
    }

    fn not_found(message: impl Into<String>) -> Self {
        Self::new(404, "NOT_FOUND", message)
    }

    fn conflict(message: impl Into<String>) -> Self {
        Self::new(409, "CONFLICT", message)
    }

    fn dependency(message: impl Into<String>) -> Self {
        Self::new(503, "BROWSER_UNAVAILABLE", message)
    }

    fn internal(message: impl Into<String>) -> Self {
        Self::new(500, "INTERNAL", message)
    }
}

fn main() {
    if let Err(error) = run(env::args().skip(1).collect()) {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

fn run(args: Vec<String>) -> Result<(), String> {
    let command = args.first().map(String::as_str).unwrap_or("help");
    match command {
        "start" => start_server(server_options(&args[1..])?),
        "stop" => stop_server(server_options(&args[1..])?),
        "status" => status_command(server_options(&args[1..])?),
        "--version" | "version" => {
            println!("{VERSION}");
            Ok(())
        }
        "help" | "--help" | "-h" => {
            print_help();
            Ok(())
        }
        other => Err(format!("unknown command: {other}")),
    }
}

#[derive(Clone)]
struct ServerOptions {
    host: String,
    port: u16,
}

fn server_options(args: &[String]) -> Result<ServerOptions, String> {
    let mut host =
        env::var("BAIJIMU_BROWSER_CONNECTOR_HOST").unwrap_or_else(|_| DEFAULT_HOST.to_string());
    let mut port = env::var("BAIJIMU_BROWSER_CONNECTOR_PORT")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_PORT);
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--host" => {
                index += 1;
                host = args.get(index).ok_or("--host requires a value")?.clone();
            }
            "--port" => {
                index += 1;
                port = args
                    .get(index)
                    .ok_or("--port requires a value")?
                    .parse()
                    .map_err(|_| "--port must be an integer from 1024 to 65535")?;
            }
            value => return Err(format!("unknown option: {value}")),
        }
        index += 1;
    }
    if port < 1024 {
        return Err("--port must be between 1024 and 65535".to_string());
    }
    let addresses = (host.as_str(), port)
        .to_socket_addrs()
        .map_err(|error| format!("invalid listen address: {error}"))?
        .collect::<Vec<_>>();
    if addresses.is_empty() || addresses.iter().any(|address| !address.ip().is_loopback()) {
        return Err("Browser Connector may only listen on a loopback address".to_string());
    }
    Ok(ServerOptions { host, port })
}

fn start_server(options: ServerOptions) -> Result<(), String> {
    let store = Store::open(connector_home())?;
    let management_token = load_or_create_token(store.root())?;
    let listener = TcpListener::bind((options.host.as_str(), options.port))
        .map_err(|error| format!("failed to bind Browser Connector: {error}"))?;
    fs::write(pid_path(), format!("{}\n", std::process::id()))
        .map_err(|error| format!("failed to record connector pid: {error}"))?;
    let state = Arc::new(AppState {
        store,
        management_token,
        sessions: Mutex::new(HashMap::new()),
    });
    println!(
        "{}",
        json!({"ok": true, "pid": std::process::id(), "url": format!("http://{}:{}", options.host, options.port)})
    );
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let state = Arc::clone(&state);
                thread::spawn(move || {
                    if let Err(error) = handle_connection(stream, state) {
                        eprintln!("request failed: {error}");
                    }
                });
            }
            Err(error) => eprintln!("accept failed: {error}"),
        }
    }
    Ok(())
}

fn status_command(options: ServerOptions) -> Result<(), String> {
    match connector_health(&options) {
        Ok(value) => println!("{}", serde_json::to_string_pretty(&value).unwrap()),
        Err(error) => println!("{}", json!({"ok": false, "error": error})),
    }
    Ok(())
}

fn stop_server(options: ServerOptions) -> Result<(), String> {
    let Ok(health) = connector_health(&options) else {
        println!(
            "{}",
            json!({"ok": true, "stopped": false, "reason": "healthy connector process not found"})
        );
        return Ok(());
    };
    if health
        .pointer("/status/connector/name")
        .and_then(Value::as_str)
        != Some(CONNECTOR_NAME)
    {
        return Err("health endpoint does not belong to Browser Connector".to_string());
    }
    let pid = health
        .pointer("/status/connector/pid")
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .ok_or("health endpoint returned no valid pid")?;
    terminate_process(pid)?;
    let _ = fs::remove_file(pid_path());
    println!("{}", json!({"ok": true, "stopped": true, "pid": pid}));
    Ok(())
}

fn handle_connection(mut stream: TcpStream, state: Arc<AppState>) -> Result<(), String> {
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .map_err(|error| error.to_string())?;
    stream
        .set_write_timeout(Some(Duration::from_secs(60)))
        .map_err(|error| error.to_string())?;
    let request = read_request(&mut stream)?;
    let clean_path = request.path.split('?').next().unwrap_or(&request.path);
    let response = match (request.method.as_str(), clean_path) {
        ("GET", "/healthz") | ("GET", "/readyz") => Ok(json!({
            "status": {
                "connector": {"name": CONNECTOR_NAME, "version": VERSION, "pid": std::process::id()},
                "browserAvailable": default_executable().is_ok(),
                "activeSessions": state.sessions.lock().map(|sessions| sessions.len()).unwrap_or_default()
            }
        })),
        ("POST", path) if path.starts_with("/invoke/") => {
            let workspace_id = request.workspace_id.ok_or_else(|| {
                HttpError::new(
                    400,
                    "WORKSPACE_CONTEXT_REQUIRED",
                    "missing trusted workspace context",
                )
            });
            workspace_id.and_then(|workspace_id| {
                let body = parse_json_body(&request.body)?;
                handle_invoke(path, &body, workspace_id, &state)
            })
        }
        (method, path) if path.starts_with("/management/") => {
            if !management_authorized(request.authorization.as_deref(), &state.management_token) {
                Err(HttpError::new(
                    401,
                    "UNAUTHORIZED",
                    "management authorization required",
                ))
            } else {
                parse_json_body(&request.body)
                    .and_then(|body| handle_management(method, &request.path, &body, &state))
            }
        }
        ("POST", "/__shutdown")
            if env::var("BAIJIMU_BROWSER_ENABLE_TEST_SHUTDOWN").as_deref() == Ok("1") =>
        {
            thread::spawn(|| {
                thread::sleep(Duration::from_millis(25));
                std::process::exit(0);
            });
            Ok(json!({"stopping": true}))
        }
        _ => Err(HttpError::not_found("endpoint not found")),
    };
    write_result(&mut stream, response)
}

fn handle_invoke(
    path: &str,
    body: &Value,
    workspace_id: u64,
    state: &AppState,
) -> Result<Value, HttpError> {
    match path {
        "/invoke/status" => Ok(json!({
            "connectorVersion": VERSION,
            "browserAvailable": default_executable().is_ok(),
            "activeSessions": session_summaries(state, workspace_id)?.len()
        })),
        "/invoke/listProfiles" => {
            let sessions = lock_sessions(state)?;
            let active = active_profile_ids(&sessions, workspace_id);
            drop(sessions);
            let profiles = state
                .store
                .list_profiles(workspace_id)
                .map_err(HttpError::internal)?
                .into_iter()
                .map(|profile| {
                    let is_active = active.contains(&profile.profile_id);
                    json!({
                        "profileId": profile.profile_id,
                        "displayName": profile.display_name,
                        "allowedHosts": profile.allowed_hosts,
                        "createdAt": profile.created_at,
                        "updatedAt": profile.updated_at,
                        "active": is_active
                    })
                })
                .collect::<Vec<_>>();
            Ok(json!({"profiles": profiles}))
        }
        "/invoke/createProfile" => {
            let profile_id = required_string(body, "profileId")?;
            let display_name = required_string(body, "displayName")?;
            let allowed_hosts = required_strings(body, "allowedHosts", 1, 100)?;
            let profile = state
                .store
                .create_profile(workspace_id, profile_id, display_name, allowed_hosts, now())
                .map_err(HttpError::conflict)?;
            serde_json::to_value(profile).map_err(|error| HttpError::internal(error.to_string()))
        }
        "/invoke/updateProfile" => {
            let profile_id = required_string(body, "profileId")?;
            let display_name = required_string(body, "displayName")?;
            let allowed_hosts = required_strings(body, "allowedHosts", 1, 100)?;
            let profile = state
                .store
                .update_profile(workspace_id, profile_id, display_name, allowed_hosts, now())
                .map_err(HttpError::bad_request)?;
            serde_json::to_value(profile).map_err(|error| HttpError::internal(error.to_string()))
        }
        "/invoke/openSession" => {
            let profile_id = required_string(body, "profileId")?;
            let headless = optional_bool(body, "headless").unwrap_or(false);
            {
                let sessions = lock_sessions(state)?;
                if active_profile_ids(&sessions, workspace_id)
                    .iter()
                    .any(|active| active == profile_id)
                {
                    return Err(HttpError::conflict("profile already has an active session"));
                }
            }
            let profile = state
                .store
                .get_profile(workspace_id, profile_id)
                .map_err(HttpError::not_found)?;
            let session = create_session(&state.store, &profile, headless, now())
                .map_err(HttpError::dependency)?;
            let summary = session.summary();
            let session_id = session.session_id.clone();
            let mut sessions = lock_sessions(state)?;
            if active_profile_ids(&sessions, workspace_id)
                .iter()
                .any(|active| active == profile_id)
            {
                return Err(HttpError::conflict(
                    "profile became active while browser was launching",
                ));
            }
            sessions.insert(session_id, Arc::new(Mutex::new(session)));
            Ok(summary)
        }
        "/invoke/listSessions" => Ok(json!({"sessions": session_summaries(state, workspace_id)?})),
        "/invoke/closeSession" => {
            let session_id = required_string(body, "sessionId")?;
            let removed = {
                let mut sessions = lock_sessions(state)?;
                let session = sessions
                    .get(session_id)
                    .ok_or_else(|| HttpError::not_found("session not found"))?;
                let owner = session
                    .lock()
                    .map_err(|_| HttpError::internal("session lock is poisoned"))?
                    .workspace_id;
                if owner != workspace_id {
                    return Err(HttpError::not_found("session not found"));
                }
                sessions.remove(session_id)
            };
            drop(removed);
            Ok(json!({"closed": true, "sessionId": session_id}))
        }
        "/invoke/getPage" => with_session(
            state,
            workspace_id,
            required_string(body, "sessionId")?,
            |session| Ok(session.page_state()),
        ),
        "/invoke/navigate" => {
            let session_id = required_string(body, "sessionId")?;
            let url = required_string(body, "url")?;
            with_session(state, workspace_id, session_id, |session| {
                let profile = state
                    .store
                    .get_profile(workspace_id, &session.profile_id)
                    .map_err(HttpError::internal)?;
                session
                    .navigate(&profile, url)
                    .map_err(HttpError::bad_request)
            })
        }
        "/invoke/snapshot" => {
            let session_id = required_string(body, "sessionId")?;
            let maximum = optional_u64(body, "maxElements").unwrap_or(200);
            if !(1..=500).contains(&maximum) {
                return Err(HttpError::bad_request(
                    "maxElements must be between 1 and 500",
                ));
            }
            with_session(state, workspace_id, session_id, |session| {
                session
                    .snapshot(maximum as usize)
                    .map_err(HttpError::internal)
            })
        }
        "/invoke/click" => {
            let session_id = required_string(body, "sessionId")?;
            let element_ref = required_string(body, "ref")?;
            with_session(state, workspace_id, session_id, |session| {
                session.click(element_ref).map_err(HttpError::bad_request)
            })
        }
        "/invoke/typeText" => {
            let session_id = required_string(body, "sessionId")?;
            let element_ref = required_string(body, "ref")?;
            let text = required_string_allow_empty(body, "text")?;
            let clear = optional_bool(body, "clear").unwrap_or(false);
            with_session(state, workspace_id, session_id, |session| {
                session
                    .type_text(element_ref, text, clear)
                    .map_err(HttpError::bad_request)
            })
        }
        "/invoke/pressKey" => {
            let session_id = required_string(body, "sessionId")?;
            let key = required_string(body, "key")?;
            with_session(state, workspace_id, session_id, |session| {
                session.press_key(key).map_err(HttpError::bad_request)
            })
        }
        "/invoke/waitFor" => {
            let session_id = required_string(body, "sessionId")?;
            let element_ref = required_string(body, "ref")?;
            let timeout = optional_u64(body, "timeoutMs").unwrap_or(10_000);
            if !(100..=60_000).contains(&timeout) {
                return Err(HttpError::bad_request(
                    "timeoutMs must be between 100 and 60000",
                ));
            }
            with_session(state, workspace_id, session_id, |session| {
                session
                    .wait_for(element_ref, timeout)
                    .map_err(HttpError::bad_request)
            })
        }
        "/invoke/uploadFiles" => {
            let session_id = required_string(body, "sessionId")?;
            let element_ref = required_string(body, "ref")?;
            let paths = required_strings(body, "paths", 1, 10)?
                .into_iter()
                .map(PathBuf::from)
                .collect::<Vec<_>>();
            let authorized = state
                .store
                .authorize_upload_paths(workspace_id, &paths)
                .map_err(HttpError::bad_request)?;
            with_session(state, workspace_id, session_id, |session| {
                session
                    .upload_files(element_ref, &authorized)
                    .map_err(HttpError::bad_request)
            })
        }
        "/invoke/screenshot" => {
            let session_id = required_string(body, "sessionId")?;
            let format = optional_string(body, "format").unwrap_or("jpeg");
            let quality = optional_u64(body, "quality").unwrap_or(80);
            if !(30..=95).contains(&quality) {
                return Err(HttpError::bad_request("quality must be between 30 and 95"));
            }
            with_session(state, workspace_id, session_id, |session| {
                session
                    .screenshot(format, quality as u32)
                    .map_err(HttpError::internal)
            })
        }
        _ => Err(HttpError::not_found("unknown browser method")),
    }
}

fn handle_management(
    method: &str,
    raw_path: &str,
    body: &Value,
    state: &AppState,
) -> Result<Value, HttpError> {
    let path = raw_path.split('?').next().unwrap_or(raw_path);
    match (method, path) {
        ("GET", "/management/v1/state") => {
            let workspace_id = query_u64(raw_path, "workspaceId")?;
            let policy = state
                .store
                .get_policy(workspace_id)
                .map_err(HttpError::internal)?;
            let profiles = state
                .store
                .list_profiles(workspace_id)
                .map_err(HttpError::internal)?;
            Ok(json!({
                "workspaceId": workspace_id,
                "profiles": profiles,
                "policy": policy,
                "sessions": session_summaries(state, workspace_id)?,
                "browserAvailable": default_executable().is_ok()
            }))
        }
        ("POST", "/management/v1/workspaces/policy") => {
            let workspace_id = required_u64(body, "workspaceId")?;
            let roots = required_strings(body, "uploadRoots", 0, 50)?
                .into_iter()
                .map(PathBuf::from)
                .collect();
            let policy = state
                .store
                .set_policy(workspace_id, roots, now())
                .map_err(HttpError::bad_request)?;
            serde_json::to_value(policy).map_err(|error| HttpError::internal(error.to_string()))
        }
        ("POST", "/management/v1/profiles/delete") => {
            let workspace_id = required_u64(body, "workspaceId")?;
            let profile_id = required_string(body, "profileId")?;
            let sessions = lock_sessions(state)?;
            if active_profile_ids(&sessions, workspace_id)
                .iter()
                .any(|active| active == profile_id)
            {
                return Err(HttpError::conflict(
                    "close the active profile session before deletion",
                ));
            }
            drop(sessions);
            state
                .store
                .delete_profile(workspace_id, profile_id)
                .map_err(HttpError::not_found)?;
            Ok(json!({"deleted": true, "profileId": profile_id}))
        }
        _ => Err(HttpError::not_found("unknown management operation")),
    }
}

fn with_session<F>(
    state: &AppState,
    workspace_id: u64,
    session_id: &str,
    operation: F,
) -> Result<Value, HttpError>
where
    F: FnOnce(&mut browser::BrowserSession) -> Result<Value, HttpError>,
{
    validate_session_id(session_id)?;
    let shared = lock_sessions(state)?
        .get(session_id)
        .cloned()
        .ok_or_else(|| HttpError::not_found("session not found"))?;
    let mut session = shared
        .lock()
        .map_err(|_| HttpError::internal("session lock is poisoned"))?;
    if session.workspace_id != workspace_id {
        return Err(HttpError::not_found("session not found"));
    }
    operation(&mut session)
}

fn session_summaries(state: &AppState, workspace_id: u64) -> Result<Vec<Value>, HttpError> {
    let sessions = lock_sessions(state)?;
    let mut values = sessions
        .values()
        .filter_map(|session| session.lock().ok())
        .filter(|session| session.workspace_id == workspace_id)
        .map(|session| session.summary())
        .collect::<Vec<_>>();
    values.sort_by(|left, right| left["sessionId"].as_str().cmp(&right["sessionId"].as_str()));
    Ok(values)
}

fn lock_sessions(
    state: &AppState,
) -> Result<std::sync::MutexGuard<'_, HashMap<String, SharedSession>>, HttpError> {
    state
        .sessions
        .lock()
        .map_err(|_| HttpError::internal("session registry lock is poisoned"))
}

fn validate_session_id(value: &str) -> Result<(), HttpError> {
    let valid = value.starts_with("session-")
        && value.len() == "session-".len() + 24
        && value["session-".len()..]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit());
    if valid {
        Ok(())
    } else {
        Err(HttpError::bad_request("invalid sessionId"))
    }
}

struct HttpRequest {
    method: String,
    path: String,
    authorization: Option<String>,
    workspace_id: Option<u64>,
    body: Vec<u8>,
}

fn read_request(stream: &mut TcpStream) -> Result<HttpRequest, String> {
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 8192];
    let header_end = loop {
        if bytes.len() > MAX_HEADER_BYTES {
            return Err("request headers exceed 64 KiB".to_string());
        }
        let read = stream.read(&mut chunk).map_err(|error| error.to_string())?;
        if read == 0 {
            return Err("connection closed before request headers".to_string());
        }
        bytes.extend_from_slice(&chunk[..read]);
        if let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break index + 4;
        }
    };
    let headers = std::str::from_utf8(&bytes[..header_end])
        .map_err(|_| "request headers must be UTF-8".to_string())?;
    let mut lines = headers.split("\r\n");
    let request_line = lines.next().ok_or("missing request line")?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().ok_or("missing method")?.to_string();
    let path = parts.next().ok_or("missing path")?.to_string();
    if parts.next().is_none() || parts.next().is_some() {
        return Err("invalid request line".to_string());
    }
    let mut header_map = HashMap::new();
    for line in lines.filter(|line| !line.is_empty()) {
        let (name, value) = line.split_once(':').ok_or("invalid header")?;
        let key = name.trim().to_ascii_lowercase();
        if header_map.insert(key, value.trim().to_string()).is_some() {
            return Err("duplicate request header".to_string());
        }
    }
    let content_length = header_map
        .get("content-length")
        .map(|value| value.parse::<usize>())
        .transpose()
        .map_err(|_| "invalid content-length")?
        .unwrap_or(0);
    if content_length > MAX_REQUEST_BYTES {
        return Err("request body exceeds 1 MiB".to_string());
    }
    while bytes.len() - header_end < content_length {
        let read = stream.read(&mut chunk).map_err(|error| error.to_string())?;
        if read == 0 {
            return Err("connection closed before request body".to_string());
        }
        bytes.extend_from_slice(&chunk[..read]);
        if bytes.len() - header_end > MAX_REQUEST_BYTES {
            return Err("request body exceeds 1 MiB".to_string());
        }
    }
    let workspace_id = header_map
        .get("x-baijimu-workspace-id")
        .map(|value| value.parse::<u64>())
        .transpose()
        .map_err(|_| "x-baijimu-workspace-id must be a positive integer")?
        .filter(|value| *value > 0);
    Ok(HttpRequest {
        method,
        path,
        authorization: header_map.get("authorization").cloned(),
        workspace_id,
        body: bytes[header_end..header_end + content_length].to_vec(),
    })
}

fn parse_json_body(bytes: &[u8]) -> Result<Value, HttpError> {
    if bytes.is_empty() {
        return Ok(json!({}));
    }
    let value: Value = serde_json::from_slice(bytes)
        .map_err(|error| HttpError::bad_request(format!("invalid JSON body: {error}")))?;
    if !value.is_object() {
        return Err(HttpError::bad_request("request body must be a JSON object"));
    }
    Ok(value)
}

fn write_result(stream: &mut TcpStream, result: Result<Value, HttpError>) -> Result<(), String> {
    let (status, value) = match result {
        Ok(data) => (200, json!({"ok": true, "data": data})),
        Err(error) => (
            error.status,
            json!({"ok": false, "error": {"code": error.code, "message": error.message, "data": error.data}}),
        ),
    };
    let body = serde_json::to_vec(&value).map_err(|error| error.to_string())?;
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        409 => "Conflict",
        503 => "Service Unavailable",
        _ => "Internal Server Error",
    };
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\nX-Content-Type-Options: nosniff\r\nCache-Control: no-store\r\n\r\n",
        body.len()
    )
    .map_err(|error| error.to_string())?;
    stream.write_all(&body).map_err(|error| error.to_string())?;
    let _ = stream.shutdown(Shutdown::Both);
    Ok(())
}

fn connector_health(options: &ServerOptions) -> Result<Value, String> {
    let address = (options.host.as_str(), options.port)
        .to_socket_addrs()
        .map_err(|error| error.to_string())?
        .next()
        .ok_or("health address resolved to no endpoints")?;
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(1))
        .map_err(|error| error.to_string())?;
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .map_err(|error| error.to_string())?;
    stream
        .write_all(b"GET /healthz HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
        .map_err(|error| error.to_string())?;
    let mut bytes = Vec::new();
    stream
        .take(1024 * 1024)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    let text = String::from_utf8(bytes).map_err(|error| error.to_string())?;
    if !text.starts_with("HTTP/1.1 200") {
        return Err(text);
    }
    let body = text
        .split_once("\r\n\r\n")
        .map(|(_, body)| body)
        .ok_or("invalid health response")?;
    let value: Value = serde_json::from_str(body).map_err(|error| error.to_string())?;
    value
        .get("data")
        .cloned()
        .ok_or("health response has no data".to_string())
}

fn required_string<'a>(body: &'a Value, key: &str) -> Result<&'a str, HttpError> {
    let value = required_string_allow_empty(body, key)?;
    if value.trim().is_empty() {
        Err(HttpError::bad_request(format!("{key} must not be empty")))
    } else {
        Ok(value)
    }
}

fn required_string_allow_empty<'a>(body: &'a Value, key: &str) -> Result<&'a str, HttpError> {
    body.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| HttpError::bad_request(format!("{key} must be a string")))
}

fn optional_string<'a>(body: &'a Value, key: &str) -> Option<&'a str> {
    body.get(key).and_then(Value::as_str)
}

fn required_strings(
    body: &Value,
    key: &str,
    minimum: usize,
    maximum: usize,
) -> Result<Vec<String>, HttpError> {
    let values = body
        .get(key)
        .and_then(Value::as_array)
        .ok_or_else(|| HttpError::bad_request(format!("{key} must be an array")))?;
    if !(minimum..=maximum).contains(&values.len()) {
        return Err(HttpError::bad_request(format!(
            "{key} must contain between {minimum} and {maximum} values"
        )));
    }
    values
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| HttpError::bad_request(format!("{key} values must be strings")))
        })
        .collect()
}

fn required_u64(body: &Value, key: &str) -> Result<u64, HttpError> {
    body.get(key)
        .and_then(Value::as_u64)
        .filter(|value| *value > 0)
        .ok_or_else(|| HttpError::bad_request(format!("{key} must be a positive integer")))
}

fn optional_u64(body: &Value, key: &str) -> Option<u64> {
    body.get(key).and_then(Value::as_u64)
}

fn optional_bool(body: &Value, key: &str) -> Option<bool> {
    body.get(key).and_then(Value::as_bool)
}

fn query_u64(raw_path: &str, key: &str) -> Result<u64, HttpError> {
    let query = raw_path
        .split_once('?')
        .map(|(_, query)| query)
        .unwrap_or_default();
    url::form_urlencoded::parse(query.as_bytes())
        .find(|(name, _)| name == key)
        .and_then(|(_, value)| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .ok_or_else(|| HttpError::bad_request(format!("{key} query parameter is required")))
}

fn management_authorized(header: Option<&str>, expected: &str) -> bool {
    header
        .and_then(|value| value.strip_prefix("Bearer "))
        .is_some_and(|value| constant_time_equal(value.as_bytes(), expected.as_bytes()))
}

fn constant_time_equal(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

fn connector_home() -> PathBuf {
    env::var_os("BAIJIMU_CONNECTOR_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| user_home().join(".baijimu-browser-connector"))
}

fn user_home() -> PathBuf {
    #[cfg(windows)]
    if let Some(value) = env::var_os("USERPROFILE").filter(|value| !value.is_empty()) {
        return PathBuf::from(value);
    }
    env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn pid_path() -> PathBuf {
    connector_home().join("connector.pid")
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(unix)]
fn terminate_process(pid: u32) -> Result<(), String> {
    let result = unsafe { libc::kill(pid as i32, libc::SIGTERM) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error().to_string())
    }
}

#[cfg(windows)]
fn terminate_process(pid: u32) -> Result<(), String> {
    use windows_sys::Win32::{
        Foundation::CloseHandle,
        System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE},
    };
    unsafe {
        let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
        if handle.is_null() {
            return Err(std::io::Error::last_os_error().to_string());
        }
        let terminated = TerminateProcess(handle, 0);
        CloseHandle(handle);
        if terminated == 0 {
            Err(std::io::Error::last_os_error().to_string())
        } else {
            Ok(())
        }
    }
}

fn print_help() {
    println!(
        "Baijimu Browser Connector {VERSION}\n\nCommands:\n  start [--host 127.0.0.1] [--port 18121]\n  status [--host 127.0.0.1] [--port 18121]\n  stop [--host 127.0.0.1] [--port 18121]\n  --version"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listen_address_is_restricted_to_loopback() {
        assert!(server_options(&["--host".into(), "127.0.0.1".into()]).is_ok());
        assert!(server_options(&["--host".into(), "0.0.0.0".into()]).is_err());
    }

    #[test]
    fn management_token_comparison_is_exact() {
        assert!(management_authorized(Some("Bearer secret"), "secret"));
        assert!(!management_authorized(Some("Bearer secreT"), "secret"));
        assert!(!management_authorized(Some("secret"), "secret"));
    }

    #[test]
    fn session_ids_reject_selector_and_path_payloads() {
        assert!(validate_session_id("session-00112233445566778899aabb").is_ok());
        assert!(validate_session_id("../session-00112233445566778899aabb").is_err());
    }
}
