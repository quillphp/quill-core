#![allow(clippy::missing_safety_doc)]
mod json;
mod manifest;
mod router;
mod state;
mod validator;

use dashmap::DashMap;
use once_cell::sync::Lazy;
use router::QuillRouter;
use sonic_rs::{from_str, to_string, Value};
use state::SharedState;
use std::collections::HashMap;
use std::ffi::c_char;
use std::panic::catch_unwind;
use std::ptr;
use std::slice;
use std::sync::atomic::{AtomicI32, AtomicU32};
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot};
use validator::ValidatorRegistry;

static REQUEST_ID: AtomicU32 = AtomicU32::new(1);
static SHARED_SOCKET_FD: AtomicI32 = AtomicI32::new(-1);
static SHARED_STATE: Lazy<Arc<SharedState>> = Lazy::new(|| Arc::new(SharedState::new()));

mod ax_rt {
    use super::{QuillRouter, ValidatorRegistry};
    use axum::http::StatusCode;
    use axum::{
        extract::{Request, State},
        response::{IntoResponse, Response},
        routing::any,
        Router as AxumRouter,
    };
    use socket2::{Domain, Protocol, Socket, Type};
    use sonic_rs::{JsonContainerTrait, JsonValueTrait};
    use std::collections::HashMap;
    #[cfg(unix)]
    use std::os::unix::io::FromRawFd;
    use std::sync::Arc;
    use tokio::net::TcpListener;
    use tokio::sync::{mpsc, oneshot};
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
        pub request_tx: mpsc::Sender<PendingRequest>,
    }

    /// Build a TcpListener for this worker.
    ///
    /// If `SHARED_SOCKET_FD` is set (multi-worker pre-fork path), we `dup(2)`
    /// the shared fd so this worker gets its own file-descriptor referring to
    /// the same kernel socket.  All workers then compete on the same accept
    /// queue; the kernel delivers each connection to exactly one of them.
    ///
    /// Otherwise (single-worker path) we bind a fresh socket.
    fn make_listener(port: u16) -> Result<TcpListener, Box<dyn std::error::Error>> {
        #[cfg(unix)]
        {
            let shared = super::SHARED_SOCKET_FD.load(std::sync::atomic::Ordering::Relaxed);
            if shared >= 0 {
                let dup_fd = unsafe { libc::dup(shared) };
                if dup_fd >= 0 {
                    // Safety: dup_fd is a valid, nonblocking, listening socket fd.
                    let std_listener = unsafe { std::net::TcpListener::from_raw_fd(dup_fd) };
                    return Ok(TcpListener::from_std(std_listener)?);
                }
            }
        }
        bind_listener(port)
    }

    /// Fresh bind — used by the single-worker path.
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
            .route("/*path", any(handle_request))
            .with_state(state);

        let listener = make_listener(port)?;

        // Graceful shutdown wiring
        let (close_tx, close_rx) = oneshot::channel::<()>();

        #[cfg(unix)]
        {
            tokio::spawn(async move {
                use tokio::signal::unix::{signal, SignalKind};
                let mut sigterm = signal(SignalKind::terminate()).unwrap();
                let mut sigint = signal(SignalKind::interrupt()).unwrap();
                tokio::select! {
                    _ = sigterm.recv() => {},
                    _ = sigint.recv() => {},
                };
                let _ = close_tx.send(());
            });
        }

        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = close_rx.await;
            })
            .await?;
        Ok(())
    }

    async fn handle_request(State(state): State<Arc<ServerState>>, req: Request) -> Response {
        let method = req.method().to_string();
        let path = req.uri().path().to_string();

        match state.router.match_route(&method, &path) {
            Ok(matched) => {
                let handler_id = matched.value.handler_id;
                let mut params_json = "{}".to_string();
                if !matched.params.is_empty() {
                    let mut params_map = HashMap::with_capacity(matched.params.len());
                    for (k, v) in matched.params.iter() {
                        params_map.insert(k.to_string(), v.to_string());
                    }
                    params_json =
                        sonic_rs::to_string(&params_map).unwrap_or_else(|_| "{}".to_string());
                }

                let body_bytes = axum::body::to_bytes(req.into_body(), matched.value.max_body_size)
                    .await
                    .unwrap_or_default();
                let body_str = std::str::from_utf8(&body_bytes).unwrap_or("");

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

                // Mix PID into Request ID to prevent collision after fork
                let pid_bits = (std::process::id() & 0xFF) << 24;
                let seq = super::REQUEST_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                    & 0x00FFFFFF;
                let request_id = (pid_bits as u32) | seq;

                let pending = PendingRequest {
                    id: request_id,
                    handler_id,
                    params_json,
                    dto_data_json,
                    response_tx: tx,
                };

                if state.request_tx.send(pending).await.is_err() {
                    return (StatusCode::SERVICE_UNAVAILABLE, "Server busy").into_response();
                }

                // 30s timeout for PHP response
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
                        let k: &str = k;
                        let v_str = v.as_str().unwrap_or("");
                        if let (Ok(h_name), Ok(h_val)) = (
                            axum::http::HeaderName::from_bytes(k.as_bytes()),
                            axum::http::HeaderValue::from_str(v_str),
                        ) {
                            h_map.insert(h_name, h_val);
                        }
                    }
                }

                response
            }
            Err(1) => (StatusCode::NOT_FOUND, "Not Found").into_response(),
            Err(2) => (StatusCode::METHOD_NOT_ALLOWED, "Method Not Allowed").into_response(),
            Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "Server Error").into_response(),
        }
    }
}

static PENDING_RESPONSES: Lazy<DashMap<u32, oneshot::Sender<String>>> = Lazy::new(DashMap::new);

static POLL_RECEIVER: Lazy<Mutex<Option<mpsc::Receiver<ax_rt::PendingRequest>>>> =
    Lazy::new(|| Mutex::new(None));

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
    if router_ptr.is_null() || method.is_null() || path.is_null() {
        return 1;
    }
    let router = unsafe { Arc::from_raw(router_ptr as *const QuillRouter) };
    let method_str =
        std::str::from_utf8(unsafe { slice::from_raw_parts(method as *const u8, method_len) })
            .unwrap_or("");
    let path_str =
        std::str::from_utf8(unsafe { slice::from_raw_parts(path as *const u8, path_len) })
            .unwrap_or("");

    let res = match router.match_route(method_str, path_str) {
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
            if out_params_max == 0 {
                return 0;
            }
            let len = json.len().min(out_params_max - 1);
            unsafe {
                ptr::copy_nonoverlapping(json.as_ptr(), out_params_json as *mut u8, len);
                *out_params_json.add(len) = 0;
            }
            0
        }
        Err(e) => e,
    };

    let _ = Arc::into_raw(router);
    res
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
) -> i32 {
    if router_ptr.is_null() {
        return 1;
    }

    let router = unsafe { Arc::from_raw(router_ptr as *const QuillRouter) };
    let validator = if validator_ptr.is_null() {
        None
    } else {
        Some(unsafe { Arc::from_raw(validator_ptr as *const ValidatorRegistry) })
    };

    let (tx, rx) = mpsc::channel(10_000);
    {
        let mut poll_rx = POLL_RECEIVER.lock().unwrap();
        *poll_rx = Some(rx);
    }

    let state = Arc::new(ax_rt::ServerState {
        router: Arc::clone(&router),
        validator: validator.as_ref().map(Arc::clone),
        request_tx: tx,
    });

    std::thread::Builder::new()
        .name("quill-worker".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();

            rt.block_on(async {
                let _ = ax_rt::start_server(port, state).await;
            });
        })
        .expect("Failed to spawn worker thread");

    let _ = Arc::into_raw(router);
    if let Some(v) = validator {
        let _ = Arc::into_raw(v);
    }

    0
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
    let mut poll_rx = POLL_RECEIVER.lock().unwrap();
    if let Some(rx) = poll_rx.as_mut() {
        if let Ok(req) = rx.try_recv() {
            let out_id = out_id as *mut u32;
            let out_handler_id = out_handler_id as *mut u32;
            unsafe {
                *out_id = req.id;
                *out_handler_id = req.handler_id;
            }

            let p_bytes = req.params_json.as_bytes();
            if out_params_max > 0 {
                let p_len = p_bytes.len().min(out_params_max - 1);
                unsafe {
                    ptr::copy_nonoverlapping(p_bytes.as_ptr(), out_params_json as *mut u8, p_len);
                    *out_params_json.add(p_len) = 0;
                }
            }

            let d_bytes = req.dto_data_json.as_bytes();
            if out_dto_max > 0 {
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
}

#[no_mangle]
pub unsafe extern "C" fn quill_server_respond(
    id: u32,
    response_json: *const c_char,
    response_len: usize,
) -> i32 {
    if let Some((_, tx)) = PENDING_RESPONSES.remove(&id) {
        let tx: oneshot::Sender<String> = tx;
        let slice = unsafe { slice::from_raw_parts(response_json as *const u8, response_len) };
        let response = std::str::from_utf8(slice).unwrap_or("{}").to_string();
        let _ = tx.send(response);
        return 0;
    }
    1
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
    if registry_ptr.is_null() {
        return 1;
    }
    let registry = unsafe { Arc::from_raw(registry_ptr as *mut ValidatorRegistry) };
    let name = std::str::from_utf8(unsafe { slice::from_raw_parts(name as *const u8, name_len) })
        .unwrap_or("");
    let schema =
        std::str::from_utf8(unsafe { slice::from_raw_parts(schema_json as *const u8, schema_len) })
            .unwrap_or("");

    let res = match registry.register(name.to_string(), schema) {
        Ok(_) => 0,
        Err(_) => 1,
    };

    let _ = Arc::into_raw(registry);
    res
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
    if registry_ptr.is_null() {
        return 2;
    }
    let registry = unsafe { Arc::from_raw(registry_ptr as *const ValidatorRegistry) };
    let name =
        std::str::from_utf8(unsafe { slice::from_raw_parts(dto_name as *const u8, dto_name_len) })
            .unwrap_or("");
    let input =
        std::str::from_utf8(unsafe { slice::from_raw_parts(input_json as *const u8, input_len) })
            .unwrap_or("");

    let res = match registry.validate(name, input) {
        Ok(val) => {
            let json = sonic_rs::to_string(&val).unwrap_or_default();
            if out_max == 0 {
                return 0;
            }
            let len = json.len().min(out_max - 1);
            unsafe {
                ptr::copy_nonoverlapping(json.as_ptr(), out_json as *mut u8, len);
                *out_json.add(len) = 0;
            }
            0
        }
        Err(errors) => {
            let json = sonic_rs::to_string(&errors).unwrap_or_default();
            if out_max == 0 {
                return 1;
            }
            let len = json.len().min(out_max - 1);
            unsafe {
                ptr::copy_nonoverlapping(json.as_ptr(), out_json as *mut u8, len);
                *out_json.add(len) = 0;
            }
            1
        }
    };

    let _ = Arc::into_raw(registry);
    res
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
    if router_ptr.is_null() {
        return 2;
    }
    let router = unsafe { Arc::from_raw(router_ptr as *const QuillRouter) };
    let validator = if validator_ptr.is_null() {
        None
    } else {
        Some(unsafe { Arc::from_raw(validator_ptr as *const ValidatorRegistry) })
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
    if out_max > 0 {
        let len = json.len().min(out_max - 1);
        unsafe {
            ptr::copy_nonoverlapping(json.as_ptr(), out_json as *mut u8, len);
            *out_json.add(len) = 0;
        }
    }

    let _ = Arc::into_raw(router);
    if let Some(v) = validator {
        let _ = Arc::into_raw(v);
    }
    0
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
    if input.is_null() {
        return 0;
    }
    let input_str =
        std::str::from_utf8(unsafe { slice::from_raw_parts(input as *const u8, input_len) })
            .unwrap_or("");
    if let Some(compacted) = json::compact_json(input_str) {
        let bytes = compacted.as_bytes();
        if out_max == 0 {
            return 0;
        }
        let len = bytes.len().min(out_max - 1);
        unsafe {
            ptr::copy_nonoverlapping(bytes.as_ptr(), out_buf as *mut u8, len);
            *out_buf.add(len) = 0;
        }
        return len;
    }
    0
}

#[cfg(unix)]
#[no_mangle]
pub extern "C" fn quill_server_prebind(port: u16) -> i32 {
    use socket2::{Domain, Protocol, Socket, Type};
    use std::os::unix::io::IntoRawFd;

    let Ok(addr) = format!("0.0.0.0:{}", port).parse::<std::net::SocketAddr>() else {
        return -1;
    };
    let Ok(socket) = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP)) else {
        return -1;
    };
    if socket.set_reuse_address(true).is_err() {
        return -1;
    }
    let _ = socket.set_reuse_port(true); // best-effort
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
}

#[cfg(not(unix))]
#[no_mangle]
pub extern "C" fn quill_server_prebind(_port: u16) -> i32 {
    // Platform not supported for pre-fork multi-worker model
    -1
}

// --- Quill Shared State Broker (SSB) FFI ---

#[no_mangle]
pub unsafe extern "C" fn quill_shared_set(
    key: *const c_char,
    key_len: usize,
    val_json: *const c_char,
    val_len: usize,
) -> i32 {
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
}

#[no_mangle]
pub unsafe extern "C" fn quill_shared_get(
    key: *const c_char,
    key_len: usize,
    out_buf: *mut c_char,
    out_max: usize,
) -> usize {
    if key.is_null() || out_buf.is_null() || out_max == 0 {
        return 0;
    }
    let key_str =
        std::str::from_utf8(slice::from_raw_parts(key as *const u8, key_len)).unwrap_or("");
    if let Some(val) = SHARED_STATE.get(key_str) {
        let json = to_string(&val).unwrap_or_default();
        if out_max == 0 {
            return 0;
        }
        let len = json.len().min(out_max - 1);
        ptr::copy_nonoverlapping(json.as_ptr(), out_buf as *mut u8, len);
        *out_buf.add(len) = 0;
        len
    } else {
        0
    }
}

#[no_mangle]
pub unsafe extern "C" fn quill_shared_incr(key: *const c_char, key_len: usize, delta: i64) -> i64 {
    if key.is_null() {
        return 0;
    }
    let key_str =
        std::str::from_utf8(slice::from_raw_parts(key as *const u8, key_len)).unwrap_or("");
    SHARED_STATE.increment(key_str, delta)
}

#[no_mangle]
pub unsafe extern "C" fn quill_shared_remove(key: *const c_char, key_len: usize) -> i32 {
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
}

#[no_mangle]
pub unsafe extern "C" fn quill_shared_keys(out_buf: *mut c_char, out_max: usize) -> usize {
    if out_max == 0 {
        return 0;
    }
    let keys = SHARED_STATE.keys();
    let json = to_string(&keys).unwrap_or_else(|_| "[]".to_string());
    let len = json.len().min(out_max - 1);
    ptr::copy_nonoverlapping(json.as_ptr(), out_buf as *mut u8, len);
    *out_buf.add(len) = 0;
    len
}

#[cfg(test)]
mod ssb_hardening_tests {
    use super::*;
    use std::ffi::CString;

    #[test]
    fn test_ffi_invalid_utf8() {
        // Invalid UTF-8 sequence
        let invalid_key = [0, 159, 146, 150];
        let val = CString::new("123").unwrap();

        unsafe {
            let res = quill_shared_set(
                invalid_key.as_ptr() as *const c_char,
                invalid_key.len(),
                val.as_ptr(),
                3,
            );
            // Should handle gracefully (unwrap_or("") in code)
            assert_eq!(res, 0);
        }
    }

    #[test]
    fn test_ffi_null_pointer_safety() {
        unsafe {
            // We can't easily pass literal NULL to reference-checked Rust in some contexts,
            // but we can pass a raw pointer.
            let res = quill_shared_set(ptr::null(), 0, ptr::null(), 0);
            assert_eq!(res, 1); // Fails because from_str::<Value>("") is Err
        }
    }

    #[test]
    fn test_buffer_edge_cases() {
        let key = CString::new("buf_test").unwrap();
        let val_json = "\"hello world\""; // 13 chars
        let val = CString::new(val_json).unwrap();

        unsafe {
            quill_shared_set(key.as_ptr(), 8, val.as_ptr(), 13);

            // 1. Exact size (needs 13 + 1 for null = 14)
            let buf = [0u8; 14];
            let len = quill_shared_get(key.as_ptr(), 8, buf.as_ptr() as *mut c_char, 14);
            assert_eq!(len, 13);
            assert_eq!(buf[13], 0);

            // 2. Underflow (out_max = 5)
            let buf2 = [0u8; 5];
            let len2 = quill_shared_get(key.as_ptr(), 8, buf2.as_ptr() as *mut c_char, 5);
            assert_eq!(len2, 4); // max - 1
            assert_eq!(buf2[4], 0);
            assert_eq!(std::str::from_utf8(&buf2[..4]).unwrap(), "\"hel");

            // 3. Zero size
            let len3 = quill_shared_get(key.as_ptr(), 8, ptr::null_mut(), 0);
            assert_eq!(len3, 0);
        }
    }

    #[test]
    fn test_massive_concurrency_soak() {
        let _state = Arc::new(SharedState::new());
        let thread_count = 200;
        let iterations = 5000;
        let mut handles = Vec::new();

        let start = std::time::Instant::now();
        for t in 0..thread_count {
            handles.push(std::thread::spawn(move || {
                let key = format!("thread_{}", t);
                for _i in 0..iterations {
                    // Mix of operations
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
