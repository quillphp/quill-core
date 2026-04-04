mod json;
mod manifest;
mod router;
mod validator;

use router::QuillRouter;
use std::collections::HashMap;
use std::ffi::c_char;
use std::panic::catch_unwind;
use std::ptr;
use std::slice;
use std::sync::Arc;
use validator::ValidatorRegistry;

mod ax_rt {
    use super::{QuillRouter, ValidatorRegistry};
    use axum::http::{HeaderName, HeaderValue, StatusCode};
    use axum::{
        extract::{Request, State},
        response::{IntoResponse, Response},
        routing::any,
        Router as AxumRouter,
    };
    use std::ffi::c_char;
    use std::sync::Arc;
    use tokio::net::TcpListener;

    pub type PHPHandlerCallback = extern "C" fn(
        handler_id: u32,
        params_json: *const c_char,
        dto_data_json: *const c_char,
        out_response: *mut c_char,
        out_max: usize,
    ) -> i32;

    pub struct ServerState {
        pub router: QuillRouter,
        pub validator: Option<ValidatorRegistry>,
        pub php_callback: PHPHandlerCallback,
    }

    pub async fn start_server(
        port: u16,
        state: Arc<ServerState>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let app = AxumRouter::new()
            .route("/*path", any(handle_request))
            .with_state(state);

        let addr = format!("0.0.0.0:{}", port);
        let listener = TcpListener::bind(addr).await?;
        axum::serve(listener, app).await?;
        Ok(())
    }

    async fn handle_request(State(state): State<Arc<ServerState>>, req: Request) -> Response {
        let method = req.method().to_string();
        let path = req.uri().path().to_string();

        // 1. Match Route
        match state.router.match_route(&method, &path) {
            Ok(matched) => {
                let handler_id = matched.value.handler_id;
                let mut params_json = "{}".to_string();
                if !matched.params.is_empty() {
                    let mut params_map =
                        std::collections::HashMap::with_capacity(matched.params.len());
                    for (k, v) in matched.params.iter() {
                        params_map.insert(k, v);
                    }
                    params_json =
                        sonic_rs::to_string(&params_map).unwrap_or_else(|_| "{}".to_string());
                }

                // 2. Body handling (Future: Move to DTO validation pass)
                let body_bytes = axum::body::to_bytes(req.into_body(), 10 * 1024 * 1024)
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
                                let err_json = serde_json::to_string(&errors)
                                    .unwrap_or_else(|_| "{}".to_string());
                                return (StatusCode::BAD_REQUEST, err_json).into_response();
                            }
                        }
                    }
                }

                // 3. Call PHP Handler (FFI Callback)
                let mut out_res = vec![0u8; 65536];
                let c_params = std::ffi::CString::new(params_json).unwrap();
                let c_dto = std::ffi::CString::new(dto_data_json).unwrap();

                let res_len = (state.php_callback)(
                    handler_id,
                    c_params.as_ptr(),
                    c_dto.as_ptr(),
                    out_res.as_mut_ptr() as *mut c_char,
                    out_res.len(),
                );

                if res_len < 0 {
                    return (StatusCode::INTERNAL_SERVER_ERROR, "PHP Handler Error")
                        .into_response();
                }

                let res_json = std::str::from_utf8(&out_res[..res_len as usize]).unwrap_or("{}");

                // Parse PHP Response (Expects JSON: {status: int, headers: {}, body: string|array})
                let php_res: serde_json::Value = serde_json::from_str(res_json).unwrap_or_default();
                let status = php_res["status"].as_u64().unwrap_or(200) as u16;
                let body = if php_res["body"].is_string() {
                    php_res["body"].as_str().unwrap().to_string()
                } else {
                    php_res["body"].to_string()
                };

                let mut response =
                    (StatusCode::from_u16(status).unwrap_or(StatusCode::OK), body).into_response();

                if let Some(headers) = php_res["headers"].as_object() {
                    let h_map = response.headers_mut();
                    for (k, v) in headers {
                        if let (Ok(h_name), Ok(h_val)) = (
                            HeaderName::from_bytes(k.as_bytes()),
                            HeaderValue::from_str(v.as_str().unwrap_or("")),
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

#[no_mangle]
pub extern "C" fn quill_router_build(
    manifest_json: *const c_char,
    manifest_len: usize,
) -> *mut std::ffi::c_void {
    if manifest_json.is_null() {
        return ptr::null_mut();
    }

    let result = catch_unwind(|| {
        // SAFETY: We checked for null, and trust PHP to pass a valid buffer of `manifest_len` length.
        let slice = unsafe { slice::from_raw_parts(manifest_json as *const u8, manifest_len) };
        if let Ok(json_str) = std::str::from_utf8(slice) {
            if let Some(router) = QuillRouter::new(json_str) {
                let boxed = Box::new(router);
                return Box::into_raw(boxed) as *mut std::ffi::c_void;
            }
        }
        ptr::null_mut()
    });

    match result {
        Ok(ptr) => ptr,
        Err(_) => ptr::null_mut(),
    }
}

#[no_mangle]
pub extern "C" fn quill_router_match(
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
    if router_ptr.is_null()
        || method.is_null()
        || path.is_null()
        || out_handler_id.is_null()
        || out_num_params.is_null()
        || out_params_json.is_null()
    {
        return 1; // Treat as not found or error
    }

    let result = catch_unwind(|| {
        // SAFETY: The router pointer is opaque and returned from build.
        let router = unsafe { &*(router_ptr as *const QuillRouter) };

        // SAFETY: Pointers are checked, length is trusted from PHP.
        let method_slice = unsafe { slice::from_raw_parts(method as *const u8, method_len) };
        let path_slice = unsafe { slice::from_raw_parts(path as *const u8, path_len) };

        let method_str = match std::str::from_utf8(method_slice) {
            Ok(s) => s,
            Err(_) => return 1,
        };

        let path_str = match std::str::from_utf8(path_slice) {
            Ok(s) => s,
            Err(_) => return 1,
        };

        match router.match_route(method_str, path_str) {
            Ok(matched) => {
                // SAFETY: pointer is valid and non-null.
                unsafe { *out_handler_id = matched.value.handler_id };

                let num_params = matched.params.len();
                unsafe { *out_num_params = num_params as u32 };

                if num_params == 0 {
                    return 0; // Success, no params
                }

                let mut params_map: HashMap<&str, &str> = HashMap::with_capacity(num_params);
                for (k, v) in matched.params.iter() {
                    params_map.insert(k, v);
                }

                let json_string = match sonic_rs::to_string(&params_map) {
                    Ok(s) => s,
                    Err(_) => "{}".to_string(),
                };

                let bytes = json_string.as_bytes();
                let len = bytes.len().min(out_params_max - 1);

                // SAFETY: pointer is valid and max size is considered
                unsafe {
                    std::ptr::copy_nonoverlapping(bytes.as_ptr(), out_params_json as *mut u8, len);
                    *out_params_json.add(len) = 0;
                }

                0 // Success
            }
            Err(e) => e, // 1 or 2
        }
    });

    match result {
        Ok(code) => code,
        Err(_) => 1,
    }
}

#[no_mangle]
pub extern "C" fn quill_router_free(router_ptr: *mut std::ffi::c_void) {
    if !router_ptr.is_null() {
        let _ = catch_unwind(|| {
            // SAFETY: Rebuilding the Box will drop it and free memoory
            let _ = unsafe { Box::from_raw(router_ptr as *mut QuillRouter) };
        });
    }
}

#[no_mangle]
pub extern "C" fn quill_json_compact(
    input: *const c_char,
    input_len: usize,
    out_buf: *mut c_char,
    out_max: usize,
) -> usize {
    if input.is_null() || out_buf.is_null() {
        return 0;
    }

    let result = catch_unwind(|| {
        // SAFETY: Pointer and len trusted from PHP
        let slice = unsafe { slice::from_raw_parts(input as *const u8, input_len) };
        if let Ok(json_str) = std::str::from_utf8(slice) {
            if let Some(compacted) = json::compact_json(json_str) {
                let bytes = compacted.as_bytes();
                if bytes.len() < out_max {
                    // SAFETY: Copying within bounds since bytes.len() < out_max
                    unsafe {
                        std::ptr::copy_nonoverlapping(
                            bytes.as_ptr(),
                            out_buf as *mut u8,
                            bytes.len(),
                        );
                        // Add null terminator just in case
                        *out_buf.add(bytes.len()) = 0;
                    }
                    return bytes.len();
                }
            }
        }
        0
    });

    match result {
        Ok(len) => len,
        Err(_) => 0,
    }
}

#[no_mangle]
pub extern "C" fn quill_validator_new() -> *mut std::ffi::c_void {
    let registry = ValidatorRegistry::new();
    Box::into_raw(Box::new(registry)) as *mut std::ffi::c_void
}

#[no_mangle]
pub extern "C" fn quill_validator_register(
    registry_ptr: *mut std::ffi::c_void,
    name: *const c_char,
    name_len: usize,
    schema_json: *const c_char,
    schema_len: usize,
) -> std::ffi::c_int {
    if registry_ptr.is_null() || name.is_null() || schema_json.is_null() {
        return 1;
    }

    let result = catch_unwind(|| {
        let registry = unsafe { &mut *(registry_ptr as *mut ValidatorRegistry) };

        let name_slice = unsafe { slice::from_raw_parts(name as *const u8, name_len) };
        let schema_slice = unsafe { slice::from_raw_parts(schema_json as *const u8, schema_len) };

        let name_str = match std::str::from_utf8(name_slice) {
            Ok(s) => s,
            Err(_) => return 1,
        };

        let schema_str = match std::str::from_utf8(schema_slice) {
            Ok(s) => s,
            Err(_) => return 1,
        };

        match registry.register(name_str.to_string(), schema_str) {
            Ok(_) => 0,
            Err(_) => 1,
        }
    });

    match result {
        Ok(code) => code,
        Err(_) => 1,
    }
}

#[no_mangle]
pub extern "C" fn quill_validator_validate(
    registry_ptr: *mut std::ffi::c_void,
    dto_name: *const c_char,
    dto_name_len: usize,
    input_json: *const c_char,
    input_len: usize,
    out_json: *mut c_char,
    out_max: usize,
) -> std::ffi::c_int {
    if registry_ptr.is_null() || dto_name.is_null() || input_json.is_null() || out_json.is_null() {
        return 2;
    }

    let result = catch_unwind(|| {
        let registry = unsafe { &*(registry_ptr as *const ValidatorRegistry) };

        let name_slice = unsafe { slice::from_raw_parts(dto_name as *const u8, dto_name_len) };
        let input_slice = unsafe { slice::from_raw_parts(input_json as *const u8, input_len) };

        let name_str = match std::str::from_utf8(name_slice) {
            Ok(s) => s,
            Err(_) => return 2,
        };

        let input_str = match std::str::from_utf8(input_slice) {
            Ok(s) => s,
            Err(_) => return 2,
        };

        match registry.validate(name_str, input_str) {
            Ok(validated_val) => {
                let json_string =
                    sonic_rs::to_string(&validated_val).unwrap_or_else(|_| "{}".to_string());
                let bytes = json_string.as_bytes();
                let len = bytes.len().min(out_max - 1);
                unsafe {
                    std::ptr::copy_nonoverlapping(bytes.as_ptr(), out_json as *mut u8, len);
                    *out_json.add(len) = 0;
                }
                0
            }
            Err(errors) => {
                let json_string =
                    serde_json::to_string(&errors).unwrap_or_else(|_| "{}".to_string());
                let bytes = json_string.as_bytes();
                let len = bytes.len().min(out_max - 1);
                unsafe {
                    std::ptr::copy_nonoverlapping(bytes.as_ptr(), out_json as *mut u8, len);
                    *out_json.add(len) = 0;
                }
                1
            }
        }
    });

    match result {
        Ok(code) => code,
        Err(_) => 2,
    }
}

#[no_mangle]
pub extern "C" fn quill_router_dispatch(
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
    if router_ptr.is_null() || method.is_null() || path.is_null() || out_json.is_null() {
        return 2; // System error
    }

    let result = catch_unwind(|| {
        let router = unsafe { &*(router_ptr as *const QuillRouter) };
        let validator = if validator_ptr.is_null() {
            None
        } else {
            Some(unsafe { &*(validator_ptr as *const ValidatorRegistry) })
        };

        let method_slice = unsafe { slice::from_raw_parts(method as *const u8, method_len) };
        let path_slice = unsafe { slice::from_raw_parts(path as *const u8, path_len) };

        let method_str = std::str::from_utf8(method_slice).unwrap_or("");
        let path_str = std::str::from_utf8(path_slice).unwrap_or("");

        let mut response = serde_json::Map::new();

        match router.match_route(method_str, path_str) {
            Ok(matched) => {
                response.insert("status".to_string(), serde_json::json!(1)); // Dispatcher::FOUND
                response.insert(
                    "handler_id".to_string(),
                    serde_json::json!(matched.value.handler_id),
                );

                let mut params = serde_json::Map::new();
                for (k, v) in matched.params.iter() {
                    params.insert(k.to_string(), serde_json::json!(v));
                }
                response.insert("params".to_string(), serde_json::Value::Object(params));

                // Unified Validation
                if let Some(dto_name) = &matched.value.dto_class {
                    if let Some(v_reg) = validator {
                        if !body_json.is_null() && body_len > 0 {
                            let body_slice =
                                unsafe { slice::from_raw_parts(body_json as *const u8, body_len) };
                            let body_str = std::str::from_utf8(body_slice).unwrap_or("");

                            match v_reg.validate(dto_name, body_str) {
                                Ok(data) => {
                                    response
                                        .insert("dto_valid".to_string(), serde_json::json!(true));
                                    response.insert("dto_data".to_string(), data);
                                }
                                Err(errors) => {
                                    response
                                        .insert("dto_valid".to_string(), serde_json::json!(false));
                                    response.insert(
                                        "dto_errors".to_string(),
                                        serde_json::json!(errors),
                                    );
                                }
                            }
                        } else {
                            // Missing body but DTO required
                            response.insert("dto_valid".to_string(), serde_json::json!(false));
                            response.insert(
                                "dto_errors".to_string(),
                                serde_json::json!({"body": ["Request body is required"]}),
                            );
                        }
                    }
                }
            }
            Err(e) => {
                // Rust Router Match logic: 1=NotFound, 2=MethodNotAllowed
                // FastRoute Dispatcher: 0=NotFound, 2=MethodNotAllowed
                let status = match e {
                    1 => 0,
                    2 => 2,
                    _ => 0,
                };
                response.insert("status".to_string(), serde_json::json!(status));
            }
        }

        let json_string = serde_json::to_string(&response).unwrap_or_else(|_| "{}".to_string());
        let bytes = json_string.as_bytes();
        let len = bytes.len().min(out_max - 1);
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), out_json as *mut u8, len);
            *out_json.add(len) = 0;
        }
        0
    });

    match result {
        Ok(code) => code,
        Err(_) => 2,
    }
}

#[no_mangle]
pub extern "C" fn quill_validator_free(registry_ptr: *mut std::ffi::c_void) {
    if !registry_ptr.is_null() {
        let _ = catch_unwind(|| {
            let _ = unsafe { Box::from_raw(registry_ptr as *mut ValidatorRegistry) };
        });
    }
}
#[no_mangle]
pub extern "C" fn quill_server_start(
    router_ptr: *mut std::ffi::c_void,
    validator_ptr: *mut std::ffi::c_void,
    port: u16,
    callback: ax_rt::PHPHandlerCallback,
) -> i32 {
    if router_ptr.is_null() {
        return 1;
    }

    let result = catch_unwind(|| {
        let router = unsafe { Box::from_raw(router_ptr as *mut QuillRouter) };
        let validator = if validator_ptr.is_null() {
            None
        } else {
            Some(unsafe { *Box::from_raw(validator_ptr as *mut ValidatorRegistry) })
        };

        let state = Arc::new(ax_rt::ServerState {
            router: *router,
            validator,
            php_callback: callback,
        });

        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();

        rt.block_on(async {
            ax_rt::start_server(port, state).await.unwrap();
        });
        0
    });

    match result {
        Ok(code) => code,
        Err(_) => 1,
    }
}
