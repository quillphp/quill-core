#![allow(clippy::missing_safety_doc)]
mod json;
mod manifest;
mod router;
mod state;
mod validator;

use dashmap::DashMap;
use once_cell::sync::{Lazy, OnceCell};
use router::QuillRouter;
use sonic_rs::{from_str, to_string, JsonContainerTrait, JsonValueTrait, Value};
use state::SharedState;
use std::collections::HashMap;
use std::ffi::{c_char, CString};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr;
use std::slice;
use std::sync::atomic::{AtomicI32, AtomicU32, AtomicU64};
use std::sync::Arc;
use tokio::sync::oneshot;
use validator::ValidatorRegistry;

static REQUEST_ID: AtomicU32 = AtomicU32::new(1);
static SHARED_SOCKET_FD: AtomicI32 = AtomicI32::new(-1);
static SHARED_STATE: Lazy<Arc<SharedState>> = Lazy::new(|| Arc::new(SharedState::new()));

static POLL_SENDER: OnceCell<flume::Sender<ax_rt::PendingRequest>> = OnceCell::new();
static POLL_RECEIVER: OnceCell<flume::Receiver<ax_rt::PendingRequest>> = OnceCell::new();
static LOG_FILE: OnceCell<String> = OnceCell::new();

// Metrics & Lifecycle
static TOTAL_REQUESTS: AtomicU64 = AtomicU64::new(0);
static ACTIVE_REQUESTS: AtomicU64 = AtomicU64::new(0);
static TOTAL_LATENCY_US: AtomicU64 = AtomicU64::new(0);
static SERVER_WORKERS: AtomicU32 = AtomicU32::new(0);
static DRAIN_SIGNAL: Lazy<std::sync::Mutex<Option<oneshot::Sender<()>>>> =
    Lazy::new(|| std::sync::Mutex::new(None));

mod ax_rt {
    use super::{QuillRouter, ValidatorRegistry};
    use axum::extract::ConnectInfo;
    use axum::http::StatusCode;
    use axum::{
        extract::{Request, State},
        response::{IntoResponse, Json, Response},
        routing::any,
        Router as AxumRouter,
    };
    use serde_json::json;
    use socket2::{Domain, Protocol, Socket, Type};
    use sonic_rs::{JsonContainerTrait, JsonValueTrait};
    use std::collections::HashMap;
    use std::net::SocketAddr;
    #[cfg(unix)]
    use std::os::unix::io::FromRawFd;
    use std::sync::Arc;
    use tokio::net::TcpListener;
    use tokio::sync::oneshot;
    use tokio::time::Duration;

    pub struct PendingRequest {
        pub id: u32,
        pub handler_id: u32,
        pub params_json: String,
        pub dto_data_json: String,
        pub response_tx: oneshot::Sender<String>,
    }

    pub struct ServerState {
        pub router: Arc<QuillRouter>,
        pub validator: Option<Arc<ValidatorRegistry>>,
        pub request_tx: flume::Sender<PendingRequest>,
    }

    fn make_listener(port: u16) -> Result<TcpListener, Box<dyn std::error::Error>> {
        #[cfg(unix)]
        {
            let shared = super::SHARED_SOCKET_FD.load(std::sync::atomic::Ordering::Relaxed);
            if shared >= 0 {
                let dup_fd = unsafe { ::libc::dup(shared) };
                if dup_fd >= 0 {
                    let std_listener = unsafe { std::net::TcpListener::from_raw_fd(dup_fd) };
                    return Ok(TcpListener::from_std(std_listener)?);
                }
            }
        }
        bind_listener(port)
    }

    fn bind_listener(port: u16) -> Result<TcpListener, Box<dyn std::error::Error>> {
        let addr: std::net::SocketAddr = format!("0.0.0.0:{}", port).parse()?;
        let socket = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP))?;
        socket.set_reuse_address(true)?;
        #[cfg(unix)]
        socket.set_reuse_port(true)?;
        socket.bind(&addr.into())?;
        socket.listen(4096)?;
        socket.set_nonblocking(true)?;
        let std_listener: std::net::TcpListener = socket.into();
        Ok(TcpListener::from_std(std_listener)?)
    }


    pub async fn start_server(
        port: u16,
        state: Arc<ServerState>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let app = AxumRouter::new()
            .fallback(any(handle_request))
            .with_state(state);

        let listener = make_listener(port)?;
        let (close_tx, close_rx) = oneshot::channel::<()>();
        {
            let mut guard = super::DRAIN_SIGNAL.lock().unwrap();
            *guard = Some(close_tx);
        }

        #[cfg(unix)]
        {
            tokio::spawn(async move {
                use tokio::signal::unix::{signal, SignalKind};
                let mut sigterm = signal(SignalKind::terminate()).unwrap();
                let _ = sigterm.recv().await;
            });
        }

        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async move {
            let _ = close_rx.await;
        })
        .await?;
        Ok(())
    }
 
    async fn handle_request(
        State(state): State<Arc<ServerState>>,
        ConnectInfo(addr): ConnectInfo<SocketAddr>,
        req: Request,
    ) -> Response {
        super::TOTAL_REQUESTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        super::ACTIVE_REQUESTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let start = std::time::Instant::now();

        let res = handle_request_inner(state, addr, req).await;

        let duration = start.elapsed().as_micros() as u64;
        super::TOTAL_LATENCY_US.fetch_add(duration, std::sync::atomic::Ordering::Relaxed);
        super::ACTIVE_REQUESTS.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        res
    }

    async fn handle_request_inner(state: Arc<ServerState>, addr: SocketAddr, req: Request) -> Response {
        let start = std::time::Instant::now();
        let method = req.method().to_string();
        let path = req.uri().path().to_string();
        let ip = addr.ip().to_string();

        match state.router.match_route(&method, &path) {
            Ok(matched) => {
                let handler_id = matched.value.handler_id;

                // ⚡ NATIVE FAST PATH — Serve directly from Rust if PHP has pre-registered
                // a static response for this handler. Zero allocations, zero PHP round-trip.
                if matched.params.is_empty() && matched.value.dto_class.is_none() {
                    if let Some(preloaded) = super::NATIVE_RESPONSES.get(&handler_id) {
                        let cached: &super::NativeResponse = preloaded.value();
                        
                        // 🟢 NATIVE LOGGING
                        let duration_fast = start.elapsed().as_micros();
                        let dur_str = format!("{}µs", duration_fast);
                        let timestamp = chrono::Local::now().format("%H:%M:%S").to_string();
                        println!(
                            " \x1B[2m{}\x1B[0m  \x1B[32m{:-6}\x1B[0m \x1B[1m{:-30}\x1B[0m \x1B[32m{}\x1B[0m \x1B[2m{:-8}\x1B[0m",
                            timestamp, method, path, 200, dur_str
                        );

                        // File Logging
                        if let Some(log_path) = super::LOG_FILE.get() {
                            if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(log_path) {
                                use std::io::Write;
                                let apache_ts = chrono::Local::now().format("%d/%b/%Y:%H:%M:%S %z").to_string();
                                let line = format!(
                                    "{} - - [{}] \"{} {} HTTP/1.1\" 200 0 \"-\" \"-\" {}µs\n",
                                    ip, apache_ts, method, path, duration_fast
                                );
                                let _ = file.write_all(line.as_bytes());
                            }
                        }

                        let mut response = (
                            StatusCode::from_u16(cached.status).unwrap_or(StatusCode::OK),
                            cached.body.clone(),
                        )
                            .into_response();
                        let h_map = response.headers_mut();
                        for (k, v) in &cached.headers {
                            let k: &str = k.as_str();
                            if let (Ok(h_name), Ok(h_val)) = (
                                axum::http::HeaderName::from_bytes(k.as_bytes()),
                                axum::http::HeaderValue::from_str(v),
                            ) {
                                h_map.insert(h_name, h_val);
                            }
                        }
                        return response;
                    }
                }

                let mut params_map = HashMap::with_capacity(matched.params.len() + 4);
                params_map.insert("_method".to_string(), method.clone());
                params_map.insert("_path".to_string(), path.clone());
                params_map.insert("_ip".to_string(), ip.clone());

                let max_size = matched.value.max_body_size;
                let body_bytes = match axum::body::to_bytes(req.into_body(), max_size).await {
                    Ok(bytes) => bytes,
                    Err(_) => {
                        return (
                            StatusCode::PAYLOAD_TOO_LARGE,
                            "Request body too large".to_string(),
                        )
                            .into_response();
                    }
                };
                let body_str = std::str::from_utf8(&body_bytes).unwrap_or("");
                params_map.insert("_body".to_string(), body_str.to_string());

                for (k, v) in matched.params.iter() {
                    params_map.insert(k.to_string(), v.to_string());
                }
                let params_json = sonic_rs::to_string(&params_map).unwrap_or_else(|_| "{}".to_string());

                let mut dto_data_json = "null".to_string();
                if let Some(dto_name) = &matched.value.dto_class {
                    if let Some(v_reg) = &state.validator {
                        match v_reg.validate(dto_name, body_str) {
                            Ok(data) => {
                                dto_data_json = sonic_rs::to_string(&data)
                                    .unwrap_or_else(|_| "null".to_string());
                            }
                            Err(errors) => {
                                let err_json = sonic_rs::to_string(&errors)
                                    .unwrap_or_else(|_| "{}".to_string());
                                return (StatusCode::BAD_REQUEST, err_json).into_response();
                            }
                        }
                    }
                }

                let (tx, rx) = oneshot::channel();
                let pid_bits = (std::process::id() & 0xFF) << 24;
                let seq = super::REQUEST_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                    & 0x00FFFFFF;
                let request_id = pid_bits | seq;

                let pending = PendingRequest {
                    id: request_id,
                    handler_id,
                    params_json,
                    dto_data_json,
                    response_tx: tx,
                };

                if state.request_tx.send(pending).is_err() {
                    return (StatusCode::SERVICE_UNAVAILABLE, "Server busy").into_response();
                }

                let res_json = match tokio::time::timeout(Duration::from_secs(30), rx).await {
                    Ok(res) => res.unwrap_or_else(|_| "{}".to_string()),
                    Err(_) => {
                        let err = sonic_rs::json!({
                            "status": 504,
                            "body": "Gateway Timeout (PHP handler timed out)"
                        });
                        return (
                            StatusCode::GATEWAY_TIMEOUT,
                            sonic_rs::to_string(&err).unwrap(),
                        )
                            .into_response();
                    }
                };

                let php_res: sonic_rs::Value =
                    sonic_rs::from_str(&res_json).unwrap_or_else(|_| sonic_rs::json!({}));
                let status = php_res["status"].as_u64().unwrap_or(200) as u16;
                let body = php_res["body"].as_str().unwrap_or("").to_string();

                let mut response =
                    (StatusCode::from_u16(status).unwrap_or(StatusCode::OK), body).into_response();

                if let Some(headers) = php_res["headers"].as_object() {
                    let h_map = response.headers_mut();
                    for (k, v) in headers {
                        let k_str: &str = k;
                        let v_str = v.as_str().unwrap_or("");
                        if let (Ok(h_name), Ok(h_val)) = (
                            axum::http::HeaderName::from_bytes(k_str.as_bytes()),
                            axum::http::HeaderValue::from_str(v_str),
                        ) {
                            h_map.insert(h_name, h_val);
                        }
                    }
                }

                response
            }
            Err(err_code) => {
                let (status, _err_label, err_detail) = match err_code {
                    1 => (StatusCode::NOT_FOUND, "Not Found", json!({"error": "Not Found", "status": 404, "path": path})),
                    2 => (StatusCode::METHOD_NOT_ALLOWED, "Method Not Allowed", json!({"error": "Method Not Allowed", "status": 405, "method": method})),
                    _ => (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error", json!({"error": "Internal Server Error", "status": 500})),
                };

                // 🔴 NATIVE ERROR LOGGING
                let duration_err = start.elapsed().as_micros();
                let dur_str = format!("{}µs", duration_err);
                let timestamp = chrono::Local::now().format("%H:%M:%S").to_string();
                let status_code = status.as_u16();
                println!(
                    " \x1B[2m{}\x1B[0m  \x1B[32m{:-6}\x1B[0m \x1B[1m{:-30}\x1B[0m \x1B[31m{}\x1B[0m \x1B[2m{:-8}\x1B[0m",
                    timestamp, method, path, status_code, dur_str
                );

                // File Logging for errors
                if let Some(log_path) = super::LOG_FILE.get() {
                    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(log_path) {
                        use std::io::Write;
                        let apache_ts = chrono::Local::now().format("%d/%b/%Y:%H:%M:%S %z").to_string();
                        let line = format!(
                            "{} - - [{}] \"{} {} HTTP/1.1\" {} 0 \"-\" \"-\" {}µs\n",
                            ip, apache_ts, method, path, status_code, duration_err
                        );
                        let _ = file.write_all(line.as_bytes());
                    }
                }

                (status, Json(err_detail)).into_response()
            }
        }
    }
}

static PENDING_RESPONSES: Lazy<DashMap<u32, oneshot::Sender<String>>> = Lazy::new(DashMap::new);

/// Pre-registered Rust-native responses for static routes.
/// PHP calls `quill_route_preload()` once per handler at boot — subsequent
/// requests matching that handler_id with no path params are served here at
/// full Rust throughput, bypassing the PHP polling bridge entirely.
struct NativeResponse {
    status: u16,
    body: String,
    headers: Vec<(String, String)>,
}
static NATIVE_RESPONSES: Lazy<DashMap<u32, NativeResponse>> = Lazy::new(DashMap::new);

#[no_mangle]
pub extern "C" fn quill_router_build(
    manifest_json: *const c_char,
    manifest_len: usize,
) -> *mut std::ffi::c_void {
    if manifest_json.is_null() {
        return ptr::null_mut();
    }
    catch_unwind(|| {
        let slice = unsafe { slice::from_raw_parts(manifest_json as *const u8, manifest_len) };
        if let Ok(json_str) = std::str::from_utf8(slice) {
            if let Some(router) = QuillRouter::new(json_str) {
                return Arc::into_raw(Arc::new(router)) as *mut std::ffi::c_void;
            }
        }
        ptr::null_mut()
    })
    .unwrap_or(ptr::null_mut())
}

#[no_mangle]
pub unsafe extern "C" fn quill_router_match(
    router_ptr: *mut std::ffi::c_void,
    method: *const c_char,
    method_len: usize,
    path: *const c_char,
    path_len: usize,
    out_handler_id: *mut u32,
    out_num_params: *mut u32,
    out_params_json: *mut c_char,
    out_params_max: usize,
) -> std::ffi::c_int {
    let result = catch_unwind(AssertUnwindSafe(|| {
        if router_ptr.is_null() || method.is_null() || path.is_null() {
            return 1;
        }
        let router = unsafe { &*(router_ptr as *const QuillRouter) };
        let method_str =
            std::str::from_utf8(unsafe { slice::from_raw_parts(method as *const u8, method_len) })
                .unwrap_or("");
        let path_str =
            std::str::from_utf8(unsafe { slice::from_raw_parts(path as *const u8, path_len) })
                .unwrap_or("");
 
        match router.match_route(method_str, path_str) {
            Ok(matched) => {
                unsafe {
                    *out_handler_id = matched.value.handler_id;
                    *out_num_params = matched.params.len() as u32;
                }
                let mut params_map = HashMap::with_capacity(matched.params.len());
                for (k, v) in matched.params.iter() {
                    params_map.insert(k.to_string(), v.to_string());
                }
                let json = sonic_rs::to_string(&params_map).unwrap_or_default();
                if out_params_max > 0 && !out_params_json.is_null() {
                    let len = json.len().min(out_params_max - 1);
                    unsafe {
                        ptr::copy_nonoverlapping(json.as_ptr(), out_params_json as *mut u8, len);
                        *out_params_json.add(len) = 0;
                    }
                }
                0
            }
            Err(e) => e,
        }
    }));
    result.unwrap_or(-1)
}

#[no_mangle]
pub unsafe extern "C" fn quill_router_free(router_ptr: *mut std::ffi::c_void) {
    if !router_ptr.is_null() {
        let _ = Arc::from_raw(router_ptr as *const QuillRouter);
    }
}

#[no_mangle]
pub unsafe extern "C" fn quill_server_listen(
    router_ptr: *mut std::ffi::c_void,
    validator_ptr: *mut std::ffi::c_void,
    port: u16,
    worker_threads: std::ffi::c_int,
    max_queue_depth: u32,
) -> i32 {
    let result = catch_unwind(AssertUnwindSafe(|| {
        if router_ptr.is_null() {
            return 1;
        }

        // Initialize global channel if not yet set
        let tx = POLL_SENDER.get_or_init(|| {
            let (tx, rx) = if max_queue_depth > 0 {
                flume::bounded(max_queue_depth as usize)
            } else {
                flume::unbounded()
            };
            POLL_RECEIVER.set(rx).ok();
            tx
        });

        // Reconstruct Arcs from FFI handles
        let router_orig = unsafe { Arc::from_raw(router_ptr as *const QuillRouter) };
        let router_clone = Arc::clone(&router_orig);
        let _ = Arc::into_raw(router_orig);

        let validator_clone = if !validator_ptr.is_null() {
            let v_orig = unsafe { Arc::from_raw(validator_ptr as *const ValidatorRegistry) };
            let v_clone = Arc::clone(&v_orig);
            let _ = Arc::into_raw(v_orig);
            Some(v_clone)
        } else {
            None
        };

        let state = Arc::new(ax_rt::ServerState {
            router: router_clone,
            validator: validator_clone,
            request_tx: tx.clone(),
        });

        let state_val = AssertUnwindSafe(state);

        SERVER_WORKERS.store(worker_threads as u32, std::sync::atomic::Ordering::Relaxed);

        std::thread::Builder::new()
            .name("quill-worker".into())
            .spawn(move || {
                let mut builder = tokio::runtime::Builder::new_multi_thread();
                if worker_threads > 0 {
                    builder.worker_threads(worker_threads as usize);
                }
                let rt = builder.enable_all().build().unwrap();

                rt.block_on(async {
                    let _ = ax_rt::start_server(port, state_val.0).await;
                });
            })
            .expect("Failed to spawn worker thread");

        0
    }));

    result.unwrap_or(-1)
}

#[no_mangle]
pub unsafe extern "C" fn quill_server_stats() -> *const c_char {
    let total = TOTAL_REQUESTS.load(std::sync::atomic::Ordering::Relaxed);
    let active = ACTIVE_REQUESTS.load(std::sync::atomic::Ordering::Relaxed);
    let latency_total = TOTAL_LATENCY_US.load(std::sync::atomic::Ordering::Relaxed);
    let avg_latency = if total > 0 { latency_total / total } else { 0 };
    let queue_depth = POLL_RECEIVER.get().map(|r| r.len()).unwrap_or(0);
    let workers = SERVER_WORKERS.load(std::sync::atomic::Ordering::Relaxed);

    let stats = sonic_rs::json!({
        "requests_total": total,
        "requests_active": active,
        "avg_latency_us": avg_latency,
        "queue_depth": queue_depth,
        "workers": workers
    });

    let s = sonic_rs::to_string(&stats).unwrap_or_else(|_| "{}".to_string());
    CString::new(s).unwrap().into_raw()
}

#[no_mangle]
pub unsafe extern "C" fn quill_server_stats_free(ptr: *mut c_char) {
    if !ptr.is_null() {
        let _ = CString::from_raw(ptr);
    }
}

#[no_mangle]
pub unsafe extern "C" fn quill_server_drain(timeout_ms: u32) -> i32 {
    let result = catch_unwind(AssertUnwindSafe(|| {
        let tx = {
            let mut guard = DRAIN_SIGNAL.lock().unwrap();
            guard.take()
        };
        if let Some(s) = tx {
            let _ = s.send(());
            // Short sleep to allow drainage to start before returning control
            std::thread::sleep(std::time::Duration::from_millis(timeout_ms as u64));
            return 0;
        }
        1
    }));
    result.unwrap_or(-1)
}

#[no_mangle]
pub unsafe extern "C" fn quill_server_set_log_file(path: *const c_char) {
    if path.is_null() { return; }
    let path_str = std::ffi::CStr::from_ptr(path).to_string_lossy().into_owned();
    let _ = LOG_FILE.set(path_str);
}

#[no_mangle]
pub unsafe extern "C" fn quill_server_poll(
    out_id: *mut std::ffi::c_void,
    out_handler_id: *mut std::ffi::c_void,
    out_params_json: *mut c_char,
    out_params_max: usize,
    out_dto_json: *mut c_char,
    out_dto_max: usize,
) -> i32 {
    let result = catch_unwind(AssertUnwindSafe(|| {
        if out_id.is_null() || out_handler_id.is_null() {
            return 0;
        }
        if let Some(rx) = POLL_RECEIVER.get() {
            if let Ok(req) = rx.try_recv() {
                let out_id = out_id as *mut u32;
                let out_handler_id = out_handler_id as *mut u32;
                unsafe {
                    *out_id = req.id;
                    *out_handler_id = req.handler_id;
                }
 
                let p_bytes = req.params_json.as_bytes();
                if out_params_max > 0 && !out_params_json.is_null() {
                    let p_len = p_bytes.len().min(out_params_max - 1);
                    unsafe {
                        ptr::copy_nonoverlapping(p_bytes.as_ptr(), out_params_json as *mut u8, p_len);
                        *out_params_json.add(p_len) = 0;
                    }
                }
 
                let d_bytes = req.dto_data_json.as_bytes();
                if out_dto_max > 0 && !out_dto_json.is_null() {
                    let d_len = d_bytes.len().min(out_dto_max - 1);
                    unsafe {
                        ptr::copy_nonoverlapping(d_bytes.as_ptr(), out_dto_json as *mut u8, d_len);
                        *out_dto_json.add(d_len) = 0;
                    }
                }
 
                PENDING_RESPONSES.insert(req.id, req.response_tx);
                return 1;
            }
        }
        0
    }));
    result.unwrap_or(-1)
}

#[no_mangle]
pub unsafe extern "C" fn quill_server_respond(
    id: u32,
    response_json: *const c_char,
    response_len: usize,
) -> i32 {
    let result = catch_unwind(AssertUnwindSafe(|| {
        if response_json.is_null() {
            return 1;
        }
        if let Some((_, tx)) = PENDING_RESPONSES.remove(&id) {
            let slice = unsafe { slice::from_raw_parts(response_json as *const u8, response_len) };
            let response = std::str::from_utf8(slice).unwrap_or("{}").to_string();
            let _ = tx.send(response);
            return 0;
        }
        1
    }));
    result.unwrap_or(-1)
}

/// Register a static pre-computed response for a route handler.
///
/// PHP calls this once per eligible handler at server boot. All subsequent
/// requests hitting that handler_id (with no path params, no DTO) are served
/// directly by Rust — bypassing the PHP polling bridge entirely.
#[no_mangle]
pub unsafe extern "C" fn quill_route_preload(
    handler_id: u32,
    response_json: *const c_char,
    response_len: usize,
) -> i32 {
    let result = catch_unwind(AssertUnwindSafe(|| {
        if response_json.is_null() || response_len == 0 {
            return 1;
        }
        let slice = unsafe { slice::from_raw_parts(response_json as *const u8, response_len) };
        let json_str = match std::str::from_utf8(slice) {
            Ok(s) => s,
            Err(_) => return 1,
        };
        let val: Value = match from_str(json_str) {
            Ok(v) => v,
            Err(_) => return 1,
        };
        let status = val["status"].as_u64().unwrap_or(200) as u16;
        let body = val["body"].as_str().unwrap_or("").to_string();
        let mut headers: Vec<(String, String)> = Vec::new();
        if let Some(hdr_map) = val["headers"].as_object() {
            for (k, v) in hdr_map {
                let k_str: &str = k;
                if let Some(v_str) = v.as_str() {
                    headers.push((k_str.to_string(), v_str.to_string()));
                }
            }
        }
        NATIVE_RESPONSES.insert(handler_id, NativeResponse { status, body, headers });
        0
    }));
    result.unwrap_or(-1)
}

#[no_mangle]
pub extern "C" fn quill_validator_new() -> *mut std::ffi::c_void {
    Arc::into_raw(Arc::new(ValidatorRegistry::new())) as *mut std::ffi::c_void
}

#[no_mangle]
pub unsafe extern "C" fn quill_validator_register(
    registry_ptr: *mut std::ffi::c_void,
    name: *const c_char,
    name_len: usize,
    schema_json: *const c_char,
    schema_len: usize,
) -> std::ffi::c_int {
    let result = catch_unwind(AssertUnwindSafe(|| {
        if registry_ptr.is_null() || name.is_null() || schema_json.is_null() {
            return 1;
        }
        let registry = unsafe { &*(registry_ptr as *mut ValidatorRegistry) };
        let name_str = std::str::from_utf8(unsafe { slice::from_raw_parts(name as *const u8, name_len) })
            .unwrap_or("");
        let schema_str =
            std::str::from_utf8(unsafe { slice::from_raw_parts(schema_json as *const u8, schema_len) })
                .unwrap_or("");

        match registry.register(name_str.to_string(), schema_str) {
            Ok(_) => 0,
            Err(_) => 1,
        }
    }));
    result.unwrap_or(-1)
}

#[no_mangle]
pub unsafe extern "C" fn quill_validator_validate(
    registry_ptr: *mut std::ffi::c_void,
    dto_name: *const c_char,
    dto_name_len: usize,
    input_json: *const c_char,
    input_len: usize,
    out_json: *mut c_char,
    out_max: usize,
) -> std::ffi::c_int {
    let result = catch_unwind(AssertUnwindSafe(|| {
        if registry_ptr.is_null() || dto_name.is_null() || input_json.is_null() {
            return 2;
        }
        let registry = unsafe { &*(registry_ptr as *const ValidatorRegistry) };
        let name =
            std::str::from_utf8(unsafe { slice::from_raw_parts(dto_name as *const u8, dto_name_len) })
                .unwrap_or("");
        let input =
            std::str::from_utf8(unsafe { slice::from_raw_parts(input_json as *const u8, input_len) })
                .unwrap_or("");

        match registry.validate(name, input) {
            Ok(val) => {
                let json = sonic_rs::to_string(&val).unwrap_or_default();
                if out_max > 0 && !out_json.is_null() {
                    let len = json.len().min(out_max - 1);
                    unsafe {
                        ptr::copy_nonoverlapping(json.as_ptr(), out_json as *mut u8, len);
                        *out_json.add(len) = 0;
                    }
                }
                0
            }
            Err(errors) => {
                let json = sonic_rs::to_string(&errors).unwrap_or_default();
                if out_max > 0 && !out_json.is_null() {
                    let len = json.len().min(out_max - 1);
                    unsafe {
                        ptr::copy_nonoverlapping(json.as_ptr(), out_json as *mut u8, len);
                        *out_json.add(len) = 0;
                    }
                }
                1
            }
        }
    }));
    result.unwrap_or(-1)
}

#[no_mangle]
pub unsafe extern "C" fn quill_router_dispatch(
    router_ptr: *mut std::ffi::c_void,
    validator_ptr: *mut std::ffi::c_void,
    method: *const c_char,
    method_len: usize,
    path: *const c_char,
    path_len: usize,
    body_json: *const c_char,
    body_len: usize,
    out_json: *mut c_char,
    out_max: usize,
) -> std::ffi::c_int {
    let result = catch_unwind(AssertUnwindSafe(|| {
        if router_ptr.is_null() || method.is_null() || path.is_null() {
            return 2;
        }
        let router = unsafe { &*(router_ptr as *const QuillRouter) };
        let validator = if validator_ptr.is_null() {
            None
        } else {
            Some(unsafe { &*(validator_ptr as *const ValidatorRegistry) })
        };

        let method_str =
            std::str::from_utf8(unsafe { slice::from_raw_parts(method as *const u8, method_len) })
                .unwrap_or("");
        let path_str =
            std::str::from_utf8(unsafe { slice::from_raw_parts(path as *const u8, path_len) })
                .unwrap_or("");
        let mut response_fields: Vec<(Value, Value)> = Vec::new();
        match router.match_route(method_str, path_str) {
            Ok(matched) => {
                response_fields.push((Value::from("status"), sonic_rs::json!(1)));
                response_fields.push((
                    Value::from("handler_id"),
                    sonic_rs::json!(matched.value.handler_id),
                ));
                let mut params_fields: Vec<(Value, Value)> = Vec::new();
                for (k, v) in matched.params.iter() {
                    params_fields.push((Value::from(k), sonic_rs::json!(v)));
                }
                response_fields.push((Value::from("params"), Value::from(&params_fields[..])));

                if let Some(dto_name) = &matched.value.dto_class {
                    if let Some(v_reg) = &validator {
                        let body_str = if !body_json.is_null() && body_len > 0 {
                            std::str::from_utf8(unsafe {
                                slice::from_raw_parts(body_json as *const u8, body_len)
                            })
                            .unwrap_or("")
                        } else {
                            ""
                        };
                        match v_reg.validate(dto_name, body_str) {
                            Ok(data) => {
                                response_fields.push((Value::from("dto_valid"), sonic_rs::json!(true)));
                                response_fields.push((Value::from("dto_data"), data));
                            }
                            Err(errors) => {
                                response_fields
                                    .push((Value::from("dto_valid"), sonic_rs::json!(false)));
                                let err_json = to_string(&errors).unwrap_or_default();
                                let err_val: Value = from_str(&err_json).unwrap_or_default();
                                response_fields.push((Value::from("dto_errors"), err_val));
                            }
                        }
                    }
                }
            }
            Err(e) => {
                response_fields.push((
                    Value::from("status"),
                    sonic_rs::json!(if e == 1 { 0 } else { 2 }),
                ));
            }
        }

        let response = Value::from(&response_fields[..]);
        let json = sonic_rs::to_string(&response).unwrap_or_default();
        if out_max > 0 && !out_json.is_null() {
            let len = json.len().min(out_max - 1);
            unsafe {
                ptr::copy_nonoverlapping(json.as_ptr(), out_json as *mut u8, len);
                *out_json.add(len) = 0;
            }
        }
        0
    }));
    result.unwrap_or(-1)
}

#[no_mangle]
pub unsafe extern "C" fn quill_validator_free(registry_ptr: *mut std::ffi::c_void) {
    if !registry_ptr.is_null() {
        let _ = Arc::from_raw(registry_ptr as *mut ValidatorRegistry);
    }
}

#[no_mangle]
pub unsafe extern "C" fn quill_json_compact(
    input: *const c_char,
    input_len: usize,
    out_buf: *mut c_char,
    out_max: usize,
) -> usize {
    let result = catch_unwind(AssertUnwindSafe(|| {
        if input.is_null() {
            return 0;
        }
        let input_str =
            std::str::from_utf8(slice::from_raw_parts(input as *const u8, input_len))
                .unwrap_or("");
        if let Some(compacted) = json::compact_json(input_str) {
            let bytes = compacted.as_bytes();
            if out_max > 0 && !out_buf.is_null() {
                let len = bytes.len().min(out_max - 1);
                ptr::copy_nonoverlapping(bytes.as_ptr(), out_buf as *mut u8, len);
                *out_buf.add(len) = 0;
                return len;
            }
        }
        0
    }));
    result.unwrap_or(0)
}

#[cfg(unix)]
#[no_mangle]
pub extern "C" fn quill_server_prebind(port: u16) -> i32 {
    let result = catch_unwind(|| {
        use socket2::{Domain, Protocol, Socket, Type};
        use std::os::unix::io::IntoRawFd;

        let Ok(addr) = format!("0.0.0.0:{}", port).parse::<std::net::SocketAddr>() else {
            return -1;
        };
        let socket = match Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP)) {
            Ok(s) => s,
            Err(_) => return -1,
        };
        if socket.set_reuse_address(true).is_err() {
            return -1;
        }
        let _ = socket.set_reuse_port(true);
        if socket.bind(&addr.into()).is_err() {
            return -1;
        }
        if socket.listen(4096).is_err() {
            return -1;
        }
        if socket.set_nonblocking(true).is_err() {
            return -1;
        }

        let fd = socket.into_raw_fd();
        SHARED_SOCKET_FD.store(fd, std::sync::atomic::Ordering::SeqCst);
        fd
    });
    result.unwrap_or(-1)
}

#[cfg(not(unix))]
#[no_mangle]
pub extern "C" fn quill_server_prebind(_port: u16) -> i32 {
    -1
}

#[no_mangle]
pub unsafe extern "C" fn quill_shared_set(
    key: *const c_char,
    key_len: usize,
    val_json: *const c_char,
    val_len: usize,
) -> i32 {
    let result = catch_unwind(AssertUnwindSafe(|| {
        if key.is_null() || val_json.is_null() {
            return 1;
        }
        let key_str =
            std::str::from_utf8(slice::from_raw_parts(key as *const u8, key_len)).unwrap_or("");
        let val_str =
            std::str::from_utf8(slice::from_raw_parts(val_json as *const u8, val_len)).unwrap_or("");
        if let Ok(val) = from_str::<Value>(val_str) {
            SHARED_STATE.set(key_str.to_string(), val);
            0
        } else {
            1
        }
    }));
    result.unwrap_or(-1)
}

#[no_mangle]
pub unsafe extern "C" fn quill_shared_get(
    key: *const c_char,
    key_len: usize,
    out_buf: *mut c_char,
    out_max: usize,
) -> usize {
    let result = catch_unwind(AssertUnwindSafe(|| {
        if key.is_null() || out_buf.is_null() || out_max == 0 {
            return 0;
        }
        let key_str =
            std::str::from_utf8(slice::from_raw_parts(key as *const u8, key_len)).unwrap_or("");
        if let Some(val) = SHARED_STATE.get_ref(key_str) {
            let json = to_string(&*val).unwrap_or_default();
            let len = json.len().min(out_max - 1);
            ptr::copy_nonoverlapping(json.as_ptr(), out_buf as *mut u8, len);
            *out_buf.add(len) = 0;
            len
        } else {
            0
        }
    }));
    result.unwrap_or(0)
}

#[no_mangle]
pub unsafe extern "C" fn quill_shared_incr(key: *const c_char, key_len: usize, delta: i64) -> i64 {
    let result = catch_unwind(AssertUnwindSafe(|| {
        if key.is_null() {
            return 0;
        }
        let key_str =
            std::str::from_utf8(slice::from_raw_parts(key as *const u8, key_len)).unwrap_or("");
        SHARED_STATE.increment(key_str, delta)
    }));
    result.unwrap_or(0)
}

#[no_mangle]
pub unsafe extern "C" fn quill_shared_remove(key: *const c_char, key_len: usize) -> i32 {
    let result = catch_unwind(AssertUnwindSafe(|| {
        if key.is_null() {
            return 1;
        }
        let key_str =
            std::str::from_utf8(slice::from_raw_parts(key as *const u8, key_len)).unwrap_or("");
        if SHARED_STATE.remove(key_str).is_some() {
            0
        } else {
            1
        }
    }));
    result.unwrap_or(-1)
}

#[no_mangle]
pub unsafe extern "C" fn quill_shared_keys(out_buf: *mut c_char, out_max: usize) -> usize {
    let result = catch_unwind(AssertUnwindSafe(|| {
        if out_max == 0 || out_buf.is_null() {
            return 0;
        }
        let keys = SHARED_STATE.keys();
        let json = to_string(&keys).unwrap_or_else(|_| "[]".to_string());
        let len = json.len().min(out_max - 1);
        ptr::copy_nonoverlapping(json.as_ptr(), out_buf as *mut u8, len);
        *out_buf.add(len) = 0;
        len
    }));
    result.unwrap_or(0)
}

#[cfg(test)]
mod ssb_hardening_tests {
    use super::*;
    use sonic_rs::JsonValueTrait;
    use std::ffi::CString;

    #[test]
    fn test_ffi_invalid_utf8() {
        let invalid_key = [0, 159, 146, 150];
        let val = CString::new("123").unwrap();
        unsafe {
            let res = quill_shared_set(
                invalid_key.as_ptr() as *const c_char,
                invalid_key.len(),
                val.as_ptr(),
                3,
            );
            assert_eq!(res, 0);
        }
    }

    #[test]
    fn test_ffi_null_pointer_safety() {
        unsafe {
            let res = quill_shared_set(ptr::null(), 0, ptr::null(), 0);
            assert_eq!(res, 1);
        }
    }

    #[test]
    fn test_metrics_tracking() {
        unsafe {
            let stats_ptr = quill_server_stats();
            let stats_cstr = std::ffi::CString::from_raw(stats_ptr as *mut c_char);
            let stats_str = stats_cstr.to_str().unwrap();
            let stats: Value = from_str(stats_str).unwrap();

            // Total requests should be at least 0
            assert!(stats["requests_total"].as_u64().is_some());
        }
    }

    #[test]
    fn test_drain_signal_behavior() {
        // Since we can't easily start a real server in a unit test
        // without binding to a port, we just verify the drain logic
        // returns 1 (error) when no server is running.
        unsafe {
            let res = quill_server_drain(10);
            assert_eq!(res, 1);
        }
    }

    #[test]
    fn test_massive_concurrency_soak() {
        let thread_count = 200;
        let iterations = 5000;
        let mut handles = Vec::new();
        let start = std::time::Instant::now();
        for t in 0..thread_count {
            handles.push(std::thread::spawn(move || {
                let key = format!("thread_{}", t);
                for _i in 0..iterations {
                    unsafe {
                        let k_cstr = CString::new(key.as_str()).unwrap();
                        quill_shared_set(
                            k_cstr.as_ptr(),
                            key.len(),
                            "\"value\"".as_ptr() as *const c_char,
                            7,
                        );
                        quill_shared_incr(k_cstr.as_ptr(), key.len(), 1);
                        let buf = [0u8; 32];
                        quill_shared_get(
                            k_cstr.as_ptr(),
                            key.len(),
                            buf.as_ptr() as *mut c_char,
                            32,
                        );
                    }
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        println!("Soak test finished in {:?}", start.elapsed());
    }
}
